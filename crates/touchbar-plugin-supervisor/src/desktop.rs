use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use anyhow::Result as AnyResult;
use sha2::{Digest, Sha256};
use touchbar_broker_schema::{
    NotificationRemove, NotificationSend, NotificationUrgencyValue, SchemaError, UriOpenRequest,
};
use touchbar_policy::{
    CapabilityId, CapabilityScope, HttpOriginRule, NotificationScope, NotificationUrgency,
    UriOpenScope,
};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};
use url::{Host, Url};
use zbus::{
    MatchRule,
    blocking::{
        Connection, MessageIterator, Proxy, connection::Builder as ConnectionBuilder,
        fdo::DBusProxy,
    },
    message::{Message, Type},
    names::{BusName, WellKnownName},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use crate::{
    ActivationExpectation, ActivationLedger, Backend, BackendRequest, CancellationToken,
    filesystem_write::PersistentQuota,
};

pub const URI_OPEN_OPERATION: &str = "open";
pub const NOTIFICATION_SEND_OPERATION: &str = "send";
pub const NOTIFICATION_REMOVE_OPERATION: &str = "remove";

/// The broker-owned effect authorized for one notification request.
///
/// Keeping this result typed lets offline tooling reuse the exact production
/// decoder and scope checks without constructing a desktop transport or
/// mutating the durable production rate ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationAuthorization {
    Send { maximum_per_minute: u16 },
    Remove,
}

pub trait UriOpenTransport: Send + Sync + 'static {
    fn open(&self, uri: &str, cancellation: &CancellationToken) -> Result<(), BrokerErrorCode>;
}

pub trait NotificationTransport: Send + Sync + 'static {
    fn send(
        &self,
        id: &str,
        source: &str,
        notification: &NotificationSend,
        cancellation: &CancellationToken,
    ) -> Result<(), BrokerErrorCode>;

    fn remove(&self, id: &str, cancellation: &CancellationToken) -> Result<(), BrokerErrorCode>;
}

pub struct NotificationBackend<T> {
    transport: T,
    quota: PersistentQuota,
    source: String,
    id_prefix: String,
}

impl<T> NotificationBackend<T> {
    pub fn new(transport: T, state_directory: &Path, source: &str) -> AnyResult<Self> {
        let digest = format!("{:x}", Sha256::digest(source.as_bytes()));
        Ok(Self {
            transport,
            quota: PersistentQuota::new(
                state_directory,
                &format!("notification-send:{source}"),
                60,
            )?,
            source: source.into(),
            id_prefix: format!("otb_{}", &digest[..32]),
        })
    }

    fn portal_id(&self, id: &str) -> String {
        format!("{}_{}", self.id_prefix, id)
    }
}

impl<T: NotificationTransport> Backend for NotificationBackend<T> {
    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        match authorize_notification_request(request)? {
            NotificationAuthorization::Send { maximum_per_minute } => self
                .quota
                .reserve(1, u64::from(maximum_per_minute))
                .map_err(|error| {
                    if error == BrokerErrorCode::QuotaExceeded {
                        BrokerErrorCode::RateLimited
                    } else {
                        error
                    }
                }),
            NotificationAuthorization::Remove => Ok(()),
        }
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            match decode_notification(request)? {
                NotificationOperation::Send(notification, _) => self.transport.send(
                    &self.portal_id(&notification.id),
                    &self.source,
                    &notification,
                    cancellation,
                )?,
                NotificationOperation::Remove(id) => {
                    self.transport.remove(&self.portal_id(&id), cancellation)?
                }
            }
            cancellation.reason().map_or(
                Ok(BrokerResult::Success {
                    payload: Vec::new(),
                }),
                Err,
            )
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

pub fn authorize_notification_request(
    request: &BackendRequest,
) -> Result<NotificationAuthorization, BrokerErrorCode> {
    match decode_notification(request)? {
        NotificationOperation::Send(_, scope) => Ok(NotificationAuthorization::Send {
            maximum_per_minute: scope.maximum_per_minute,
        }),
        NotificationOperation::Remove(_) => Ok(NotificationAuthorization::Remove),
    }
}

enum NotificationOperation<'a> {
    Send(NotificationSend, &'a NotificationScope),
    Remove(String),
}

fn decode_notification(
    request: &BackendRequest,
) -> Result<NotificationOperation<'_>, BrokerErrorCode> {
    if request.capability != CapabilityId::NotificationSendV1 {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let CapabilityScope::NotificationSend(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    match request.operation.as_str() {
        NOTIFICATION_SEND_OPERATION => {
            let notification = NotificationSend::decode(&request.payload).map_err(schema_error)?;
            if !valid_kebab_id(&notification.id)
                || !scope.categories.contains(&notification.category)
                || notification.title.is_empty()
                || !safe_notification_text(&notification.title, false)
                || !safe_notification_text(&notification.body, true)
            {
                return Err(BrokerErrorCode::InvalidRequest);
            }
            let urgency = match notification.urgency {
                NotificationUrgencyValue::Low => NotificationUrgency::Low,
                NotificationUrgencyValue::Normal => NotificationUrgency::Normal,
                NotificationUrgencyValue::Critical => NotificationUrgency::Critical,
            };
            if !scope.urgency.contains(&urgency) {
                return Err(BrokerErrorCode::OutOfScope);
            }
            Ok(NotificationOperation::Send(notification, scope))
        }
        NOTIFICATION_REMOVE_OPERATION => {
            let remove = NotificationRemove::decode(&request.payload).map_err(schema_error)?;
            if !valid_kebab_id(&remove.id) {
                return Err(BrokerErrorCode::InvalidRequest);
            }
            Ok(NotificationOperation::Remove(remove.id))
        }
        _ => Err(BrokerErrorCode::InvalidRequest),
    }
}

fn valid_kebab_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn safe_notification_text(value: &str, allow_layout: bool) -> bool {
    value.chars().all(|character| {
        let allowed_control = allow_layout && matches!(character, '\n' | '\t');
        (!character.is_control() || allowed_control)
            && !matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    })
}

pub struct UriOpenBackend<T> {
    transport: T,
}

impl<T> UriOpenBackend<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: UriOpenTransport> Backend for UriOpenBackend<T> {
    fn authorize(
        &self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        authorize_uri(request)?;
        let activation = request
            .activation
            .as_ref()
            .ok_or(BrokerErrorCode::ActivationRequired)?;
        activations.consume(
            Some(activation),
            ActivationExpectation {
                surface_instance: activation.surface_instance,
                item_id: &activation.item_id,
                widget_id: activation.widget_id,
                now_monotonic_micros,
            },
        )?;
        Ok(())
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let uri = authorize_uri(request)?;
            self.transport.open(uri.as_str(), cancellation)?;
            cancellation.reason().map_or(
                Ok(BrokerResult::Success {
                    payload: Vec::new(),
                }),
                Err,
            )
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

fn authorize_uri(request: &BackendRequest) -> Result<Url, BrokerErrorCode> {
    if request.capability != CapabilityId::UriOpenV1 || request.operation != URI_OPEN_OPERATION {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let wire = UriOpenRequest::decode(&request.payload).map_err(schema_error)?;
    let CapabilityScope::UriOpen(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let uri = Url::parse(&wire.uri).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    if !scope.schemes.contains(uri.scheme()) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    match uri.scheme() {
        "http" | "https" => authorize_web_uri(scope, &uri)?,
        "mailto" => {
            if !scope.origins.is_empty()
                || !uri.cannot_be_a_base()
                || wire.uri.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
            {
                return Err(BrokerErrorCode::InvalidRequest);
            }
        }
        // file:, data:, javascript:, executable handlers, and custom schemes
        // are not part of the first safe portal adapter even if a manifest can
        // describe them.
        _ => return Err(BrokerErrorCode::Unsupported),
    }
    Ok(uri)
}

fn authorize_web_uri(scope: &UriOpenScope, uri: &Url) -> Result<(), BrokerErrorCode> {
    if !uri.username().is_empty()
        || uri.password().is_some()
        || uri.cannot_be_a_base()
        || uri.host().is_none()
        || uri.as_str().contains('\\')
        || dangerous_encoded_path(uri.path())
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    if scope.origins.is_empty() {
        return Ok(());
    }
    let host = match uri.host().ok_or(BrokerErrorCode::InvalidRequest)? {
        Host::Domain(host) => host.to_owned(),
        Host::Ipv4(host) => host.to_string(),
        Host::Ipv6(host) => host.to_string(),
    };
    let port = uri
        .port_or_known_default()
        .ok_or(BrokerErrorCode::InvalidRequest)?;
    scope
        .origins
        .iter()
        .any(|origin| {
            origin.scheme == uri.scheme()
                && origin.host == host
                && origin.port == port
                && path_allowed(origin, uri.path())
        })
        .then_some(())
        .ok_or(BrokerErrorCode::OutOfScope)
}

fn path_allowed(origin: &HttpOriginRule, path: &str) -> bool {
    origin.path_prefixes.is_empty()
        || origin
            .path_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn dangerous_encoded_path(path: &str) -> bool {
    let lowercase = path.to_ascii_lowercase();
    lowercase.contains("%2f")
        || lowercase.contains("%5c")
        || lowercase.contains("%2e")
        || lowercase.contains("%00")
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

pub struct ZbusDesktopPortal {
    session: Mutex<Option<Connection>>,
    next_handle: AtomicU64,
    session_address: Option<String>,
}

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_OPEN_URI_INTERFACE: &str = "org.freedesktop.portal.OpenURI";
const PORTAL_REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const PORTAL_METHOD_TIMEOUT: Duration = Duration::from_secs(2);
const PORTAL_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const PORTAL_CANCELLATION_POLL: Duration = Duration::from_millis(5);

impl Default for ZbusDesktopPortal {
    fn default() -> Self {
        Self {
            session: Mutex::new(None),
            next_handle: AtomicU64::new(0),
            session_address: None,
        }
    }
}

impl ZbusDesktopPortal {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_session_address(address: String) -> Self {
        Self {
            session: Mutex::new(None),
            next_handle: AtomicU64::new(0),
            session_address: Some(address),
        }
    }

    fn fresh_connection(&self) -> Result<Connection, BrokerErrorCode> {
        let builder = match self.session_address.as_deref() {
            Some(address) => ConnectionBuilder::address(address),
            None => ConnectionBuilder::session(),
        }
        .map_err(|_| BrokerErrorCode::Unavailable)?;
        builder
            .method_timeout(PORTAL_METHOD_TIMEOUT)
            .build()
            .map_err(|_| BrokerErrorCode::Unavailable)
    }

    fn connection(&self) -> Result<Connection, BrokerErrorCode> {
        let mut session = self.session.lock().map_err(|_| BrokerErrorCode::Internal)?;
        if let Some(connection) = session.as_ref() {
            return Ok(connection.clone());
        }
        let connection = self.fresh_connection()?;
        *session = Some(connection.clone());
        Ok(connection)
    }
}

impl UriOpenTransport for ZbusDesktopPortal {
    fn open(&self, uri: &str, cancellation: &CancellationToken) -> Result<(), BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        // URI requests use a dedicated connection. Closing it can interrupt a
        // stalled request without disrupting the notification transport.
        let connection = self.fresh_connection()?;
        let portal_owner = activate_and_resolve_portal(&connection)?;
        let sequence = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let handle_token = portal_handle_token(sequence)?;
        let expected_handle = expected_portal_handle(&connection, &handle_token)?;
        let worker_connection = connection.clone();
        let worker_uri = uri.to_owned();
        let worker_handle = expected_handle.clone();
        let worker_owner = portal_owner.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("touchbar-uri-portal".into())
            .spawn(move || {
                let result = run_portal_open(
                    &worker_connection,
                    &worker_uri,
                    &handle_token,
                    &worker_handle,
                    &worker_owner,
                );
                let _ = sender.send(result);
            })
            .map_err(|_| BrokerErrorCode::Internal)?;

        loop {
            match receiver.recv_timeout(PORTAL_CANCELLATION_POLL) {
                Ok(result) => {
                    worker.join().map_err(|_| BrokerErrorCode::Internal)?;
                    result?;
                    return cancellation.reason().map_or(Ok(()), Err);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(reason) = cancellation.reason() {
                        close_portal_request(&connection, &portal_owner, &expected_handle);
                        let _ = connection.close();
                        worker.join().map_err(|_| BrokerErrorCode::Internal)?;
                        return Err(reason);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = connection.close();
                    worker.join().map_err(|_| BrokerErrorCode::Internal)?;
                    return Err(BrokerErrorCode::Internal);
                }
            }
        }
    }
}

fn run_portal_open(
    connection: &Connection,
    uri: &str,
    handle_token: &str,
    expected_handle: &str,
    portal_owner: &str,
) -> Result<(), BrokerErrorCode> {
    // Subscribe before OpenURI because a portal may emit Response before its
    // method reply reaches the client.
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(portal_owner)
        .map_err(|_| BrokerErrorCode::Internal)?
        .path(expected_handle)
        .map_err(|_| BrokerErrorCode::Internal)?
        .interface(PORTAL_REQUEST_INTERFACE)
        .map_err(|_| BrokerErrorCode::Internal)?
        .member("Response")
        .map_err(|_| BrokerErrorCode::Internal)?
        .build();
    let mut responses = MessageIterator::for_match_rule(rule, connection, Some(1))
        .map_err(|_| BrokerErrorCode::Unavailable)?;
    let proxy = Proxy::new(
        connection,
        portal_owner,
        PORTAL_DESKTOP_PATH,
        PORTAL_OPEN_URI_INTERFACE,
    )
    .map_err(|_| BrokerErrorCode::Unavailable)?;
    let mut options = HashMap::<&str, Value<'_>>::new();
    options.insert("handle_token", Value::from(handle_token));
    options.insert("ask", Value::from(false));
    let handle: OwnedObjectPath = proxy
        .call("OpenURI", &("", uri, options))
        .map_err(map_portal_call_error)?;
    if handle.as_str() != expected_handle {
        // v1 follows the Request handle-token convention. A fallback path is
        // unsafe because a fast Response on that path may already be lost.
        return Err(BrokerErrorCode::BackendFailed);
    }
    let response = responses
        .next()
        .ok_or(BrokerErrorCode::Unavailable)?
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    decode_portal_response(&response, connection, expected_handle, portal_owner)
}

fn decode_portal_response(
    message: &Message,
    connection: &Connection,
    expected_handle: &str,
    portal_owner: &str,
) -> Result<(), BrokerErrorCode> {
    let header = message.header();
    let destination = connection
        .unique_name()
        .ok_or(BrokerErrorCode::BackendFailed)?;
    if message.message_type() != Type::Signal
        || header.sender().map(|name| name.as_str()) != Some(portal_owner)
        || header.destination().map(|name| name.as_str()) != Some(destination.as_str())
        || header.path().map(|path| path.as_str()) != Some(expected_handle)
        || header.interface().map(|name| name.as_str()) != Some(PORTAL_REQUEST_INTERFACE)
        || header.member().map(|name| name.as_str()) != Some("Response")
    {
        return Err(BrokerErrorCode::BackendFailed);
    }
    if message.body().len() > PORTAL_RESPONSE_MAX_BYTES || header.unix_fds().unwrap_or(0) != 0 {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    let signature = message.body().signature().to_string();
    let signature = signature
        .strip_prefix('(')
        .and_then(|signature| signature.strip_suffix(')'))
        .unwrap_or(&signature);
    if signature != "ua{sv}" {
        return Err(BrokerErrorCode::BackendFailed);
    }
    let (response, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    if !results.is_empty() {
        return Err(BrokerErrorCode::BackendFailed);
    }
    match response {
        0 => Ok(()),
        1 => Err(BrokerErrorCode::Cancelled),
        2 => Err(BrokerErrorCode::BackendFailed),
        _ => Err(BrokerErrorCode::BackendFailed),
    }
}

fn close_portal_request(connection: &Connection, portal_owner: &str, handle: &str) {
    if let Ok(request) = Proxy::new(connection, portal_owner, handle, PORTAL_REQUEST_INTERFACE) {
        // Request.Close has no Response. Do not let a broken portal delay
        // cancellation by waiting for a method reply it need not send.
        let _ = request.call_noreply("Close", &());
    }
}

fn activate_and_resolve_portal(connection: &Connection) -> Result<String, BrokerErrorCode> {
    let proxy = DBusProxy::new(connection).map_err(|_| BrokerErrorCode::Unavailable)?;
    let well_known =
        WellKnownName::try_from(PORTAL_DESTINATION).map_err(|_| BrokerErrorCode::Internal)?;
    proxy
        .start_service_by_name(well_known.clone(), 0)
        .map_err(map_portal_fdo_error)?;
    proxy
        .get_name_owner(BusName::WellKnown(well_known))
        .map(|owner| owner.to_string())
        .map_err(map_portal_fdo_error)
}

fn map_portal_call_error(error: zbus::Error) -> BrokerErrorCode {
    match error {
        zbus::Error::InputOutput(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            BrokerErrorCode::Timeout
        }
        _ => BrokerErrorCode::BackendFailed,
    }
}

fn map_portal_fdo_error(error: zbus::fdo::Error) -> BrokerErrorCode {
    match error {
        zbus::fdo::Error::ZBus(zbus::Error::InputOutput(error))
            if error.kind() == std::io::ErrorKind::TimedOut =>
        {
            BrokerErrorCode::Timeout
        }
        zbus::fdo::Error::NameHasNoOwner(_) | zbus::fdo::Error::ServiceUnknown(_) => {
            BrokerErrorCode::Unavailable
        }
        _ => BrokerErrorCode::BackendFailed,
    }
}

fn expected_portal_handle(
    connection: &Connection,
    handle_token: &str,
) -> Result<String, BrokerErrorCode> {
    let sender = connection
        .unique_name()
        .ok_or(BrokerErrorCode::BackendFailed)?
        .as_str()
        .strip_prefix(':')
        .ok_or(BrokerErrorCode::BackendFailed)?
        .replace('.', "_");
    Ok(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{handle_token}"
    ))
}

fn portal_handle_token(sequence: u64) -> Result<String, BrokerErrorCode> {
    let mut random = [0_u8; 16];
    let mut filled = 0;
    while filled < random.len() {
        // SAFETY: the remaining slice is writable for the supplied length.
        let count = unsafe {
            libc::syscall(
                libc::SYS_getrandom,
                random[filled..].as_mut_ptr(),
                random.len() - filled,
                0,
            )
        };
        if count < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(BrokerErrorCode::BackendFailed);
        }
        if count == 0 {
            return Err(BrokerErrorCode::BackendFailed);
        }
        filled += usize::try_from(count).map_err(|_| BrokerErrorCode::BackendFailed)?;
    }
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!(
        "touchbar_{}_{}_{}",
        std::process::id(),
        sequence,
        random
    ))
}

impl NotificationTransport for ZbusDesktopPortal {
    fn send(
        &self,
        id: &str,
        source: &str,
        notification: &NotificationSend,
        cancellation: &CancellationToken,
    ) -> Result<(), BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let connection = self.connection()?;
        let proxy = Proxy::new(
            &connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Notification",
        )
        .map_err(|_| BrokerErrorCode::Unavailable)?;
        let title = format!("TouchBar plugin · {source}");
        let body = if notification.body.is_empty() {
            notification.title.clone()
        } else {
            format!("{}\n{}", notification.title, notification.body)
        };
        let priority = match notification.urgency {
            NotificationUrgencyValue::Low => "low",
            NotificationUrgencyValue::Normal => "normal",
            NotificationUrgencyValue::Critical => "urgent",
        };
        let mut values = HashMap::<&str, Value<'_>>::new();
        values.insert("title", Value::from(title.as_str()));
        values.insert("body", Value::from(body.as_str()));
        values.insert("priority", Value::from(priority));
        values.insert("category", Value::from(notification.category.as_str()));
        values.insert(
            "display-hint",
            Value::from(vec!["hide-content-on-lock-screen"]),
        );
        let _: () = proxy
            .call("AddNotification", &(id, values))
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        cancellation.reason().map_or(Ok(()), Err)
    }

    fn remove(&self, id: &str, cancellation: &CancellationToken) -> Result<(), BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let connection = self.connection()?;
        let proxy = Proxy::new(
            &connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Notification",
        )
        .map_err(|_| BrokerErrorCode::Unavailable)?;
        let _: () = proxy
            .call("RemoveNotification", &(id,))
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        cancellation.reason().map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
        },
        time::{Duration, Instant},
    };

    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        CapabilityScope, HttpOriginRule, NotificationScope, NotificationUrgency, PackageInstance,
        Provenance, RuntimeKind, UriOpenScope,
    };
    use touchbar_protocol::broker_ipc::{ActivationContext, ActivationOrigin};

    use super::*;
    use crate::ConnectionIdentity;

    struct PrivateBus {
        address: String,
        pid: libc::pid_t,
        _directory: tempfile::TempDir,
    }

    impl PrivateBus {
        fn start() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let address = format!("unix:path={}", directory.path().join("bus").display());
            let output = Command::new("dbus-daemon")
                .args([
                    "--session",
                    "--fork",
                    "--nopidfile",
                    &format!("--address={address}"),
                    "--print-address=1",
                    "--print-pid=1",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            let output = String::from_utf8(output.stdout).unwrap();
            let mut lines = output.lines();
            let address = lines.next().unwrap().to_owned();
            let pid = lines.next().unwrap().parse().unwrap();
            assert!(lines.next().is_none());
            Self {
                address,
                pid,
                _directory: directory,
            }
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            // SAFETY: this PID came directly from the private dbus-daemon.
            let _ = unsafe { libc::kill(self.pid, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                // SAFETY: signal zero only checks this exact PID's existence.
                if unsafe { libc::kill(self.pid, 0) } != 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[derive(Clone, Copy)]
    enum PrivatePortalMode {
        Response(u32),
        MalformedResponse,
        WaitForClose,
        WrongHandle,
    }

    struct PrivatePortalRequest {
        closes: Arc<AtomicUsize>,
    }

    #[zbus::interface(name = "org.freedesktop.portal.Request")]
    impl PrivatePortalRequest {
        fn close(&self) {
            self.closes.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }

    struct PrivateOpenUriPortal {
        mode: Arc<Mutex<PrivatePortalMode>>,
        opens: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
    }

    #[zbus::interface(name = "org.freedesktop.portal.OpenURI")]
    impl PrivateOpenUriPortal {
        #[zbus(name = "OpenURI")]
        async fn open_uri(
            &self,
            _parent_window: &str,
            _uri: &str,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(object_server)] object_server: &zbus::ObjectServer,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            self.opens.fetch_add(1, AtomicOrdering::Relaxed);
            let token = options
                .get("handle_token")
                .and_then(|value| <&str>::try_from(value).ok())
                .ok_or_else(|| zbus::fdo::Error::Failed("missing handle_token".into()))?;
            if options
                .get("ask")
                .and_then(|value| bool::try_from(value).ok())
                != Some(false)
            {
                return Err(zbus::fdo::Error::Failed("ask was not false".into()));
            }
            let sender = header
                .sender()
                .ok_or_else(|| zbus::fdo::Error::Failed("missing sender".into()))?
                .to_owned();
            let sender_path = sender.as_str().trim_start_matches(':').replace('.', "_");
            let expected = format!("/org/freedesktop/portal/desktop/request/{sender_path}/{token}");
            let mode = *self.mode.lock().unwrap();
            if matches!(mode, PrivatePortalMode::WrongHandle) {
                return OwnedObjectPath::try_from(format!(
                    "/org/freedesktop/portal/desktop/request/{sender_path}/wrong"
                ))
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()));
            }
            object_server
                .at(
                    expected.clone(),
                    PrivatePortalRequest {
                        closes: self.closes.clone(),
                    },
                )
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            match mode {
                PrivatePortalMode::Response(code) => {
                    connection
                        .emit_signal(
                            Some(sender),
                            expected.as_str(),
                            PORTAL_REQUEST_INTERFACE,
                            "Response",
                            &(code, HashMap::<String, OwnedValue>::new()),
                        )
                        .await
                        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                }
                PrivatePortalMode::MalformedResponse => {
                    connection
                        .emit_signal(
                            Some(sender),
                            expected.as_str(),
                            PORTAL_REQUEST_INTERFACE,
                            "Response",
                            &(0_u32, "malformed"),
                        )
                        .await
                        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                }
                PrivatePortalMode::WaitForClose | PrivatePortalMode::WrongHandle => {}
            }
            OwnedObjectPath::try_from(expected)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        }
    }

    struct PrivatePortalHarness {
        bus: PrivateBus,
        _connection: Connection,
        mode: Arc<Mutex<PrivatePortalMode>>,
        opens: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
    }

    impl PrivatePortalHarness {
        fn start(mode: PrivatePortalMode) -> Self {
            let bus = PrivateBus::start();
            let mode = Arc::new(Mutex::new(mode));
            let opens = Arc::new(AtomicUsize::new(0));
            let closes = Arc::new(AtomicUsize::new(0));
            let connection = ConnectionBuilder::address(bus.address.as_str())
                .unwrap()
                .name(PORTAL_DESTINATION)
                .unwrap()
                .serve_at(
                    PORTAL_DESKTOP_PATH,
                    PrivateOpenUriPortal {
                        mode: mode.clone(),
                        opens: opens.clone(),
                        closes: closes.clone(),
                    },
                )
                .unwrap()
                .build()
                .unwrap();
            Self {
                bus,
                _connection: connection,
                mode,
                opens,
                closes,
            }
        }

        fn portal(&self) -> ZbusDesktopPortal {
            ZbusDesktopPortal::with_session_address(self.bus.address.clone())
        }

        fn set_mode(&self, mode: PrivatePortalMode) {
            *self.mode.lock().unwrap() = mode;
        }
    }

    #[derive(Clone, Default)]
    struct FakePortal {
        opened: Arc<Mutex<Vec<String>>>,
    }

    impl UriOpenTransport for FakePortal {
        fn open(
            &self,
            uri: &str,
            _cancellation: &CancellationToken,
        ) -> Result<(), BrokerErrorCode> {
            self.opened.lock().unwrap().push(uri.into());
            Ok(())
        }
    }

    type SentNotification = (String, String, NotificationSend);

    #[derive(Clone, Default)]
    struct FakeNotifications {
        sent: Arc<Mutex<Vec<SentNotification>>>,
        removed: Arc<Mutex<Vec<String>>>,
    }

    impl NotificationTransport for FakeNotifications {
        fn send(
            &self,
            id: &str,
            source: &str,
            notification: &NotificationSend,
            _cancellation: &CancellationToken,
        ) -> Result<(), BrokerErrorCode> {
            self.sent
                .lock()
                .unwrap()
                .push((id.into(), source.into(), notification.clone()));
            Ok(())
        }

        fn remove(
            &self,
            id: &str,
            _cancellation: &CancellationToken,
        ) -> Result<(), BrokerErrorCode> {
            self.removed.lock().unwrap().push(id.into());
            Ok(())
        }
    }

    fn notification_scope(maximum_per_minute: u16) -> NotificationScope {
        NotificationScope {
            categories: BTreeSet::from(["status".into()]),
            urgency: BTreeSet::from([NotificationUrgency::Normal]),
            actions: false,
            maximum_per_minute,
        }
    }

    fn notification_request(
        operation: &str,
        payload: Vec<u8>,
        scope: NotificationScope,
    ) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 1,
                package: PackageInstance {
                    source: GithubSource::new("alice", "notifier").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "b".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::NotificationSendV1,
            authorized_scope: CapabilityScope::NotificationSend(scope),
            bindings: touchbar_policy::GrantBindings::default(),
            activation: None,
            operation: operation.into(),
            payload,
        }
    }

    fn private_state(path: &Path) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn request(uri: &str, scope: UriOpenScope) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 1,
                package: PackageInstance {
                    source: GithubSource::new("alice", "links").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::UriOpenV1,
            authorized_scope: CapabilityScope::UriOpen(scope),
            bindings: touchbar_policy::GrantBindings::default(),
            activation: None,
            operation: URI_OPEN_OPERATION.into(),
            payload: UriOpenRequest { uri: uri.into() }.encode().unwrap(),
        }
    }

    fn activation(sequence: u64) -> ActivationContext {
        ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 4,
            item_id: "links".into(),
            widget_id: 9,
            input_sequence: sequence,
            deadline_monotonic_micros: 200,
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)).unwrap()
    }

    fn web_scope() -> UriOpenScope {
        UriOpenScope {
            schemes: BTreeSet::from(["https".into()]),
            origins: BTreeSet::from([HttpOriginRule {
                scheme: "https".into(),
                host: "example.com".into(),
                port: 443,
                path_prefixes: BTreeSet::from(["/docs/".into()]),
            }]),
        }
    }

    #[test]
    fn exact_origin_and_one_physical_activation_reach_portal() {
        let portal = FakePortal::default();
        let opened = portal.opened.clone();
        let backend = UriOpenBackend::new(portal);
        let mut request = request("https://example.com/docs/start", web_scope());
        let mut activations = ActivationLedger::new(8);
        assert_eq!(
            backend.authorize(&request, &mut activations, 100),
            Err(BrokerErrorCode::ActivationRequired)
        );
        request.activation = Some(activation(1));
        backend.authorize(&request, &mut activations, 100).unwrap();
        assert_eq!(
            backend.authorize(&request, &mut activations, 100),
            Err(BrokerErrorCode::ActivationRequired)
        );
        assert!(matches!(
            backend.execute(&request, &token()),
            BrokerResult::Success { .. }
        ));
        assert_eq!(
            &*opened.lock().unwrap(),
            &["https://example.com/docs/start"]
        );
    }

    #[test]
    fn dangerous_and_out_of_scope_uris_never_reach_portal() {
        let portal = FakePortal::default();
        let opened = portal.opened.clone();
        let backend = UriOpenBackend::new(portal);
        for uri in [
            "https://evil.example/docs/start",
            "https://example.com/other",
            "https://user@example.com/docs/start",
            "https://example.com/docs/%2e%2e/private",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            let mut request = request(uri, web_scope());
            request.activation = Some(activation(2));
            assert!(
                backend
                    .authorize(&request, &mut ActivationLedger::new(8), 100)
                    .is_err()
            );
        }
        assert!(opened.lock().unwrap().is_empty());
    }

    #[test]
    fn mailto_is_supported_only_as_an_explicit_non_origin_scheme() {
        let backend = UriOpenBackend::new(FakePortal::default());
        let scope = UriOpenScope {
            schemes: BTreeSet::from(["mailto".into()]),
            origins: BTreeSet::new(),
        };
        let mut request = request("mailto:team@example.com?subject=Hello", scope);
        request.activation = Some(activation(3));
        backend
            .authorize(&request, &mut ActivationLedger::new(8), 100)
            .unwrap();
        assert!(matches!(
            backend.execute(&request, &token()),
            BrokerResult::Success { .. }
        ));
    }

    #[test]
    fn portal_response_is_the_uri_operation_result() {
        let harness = PrivatePortalHarness::start(PrivatePortalMode::Response(0));
        let portal = harness.portal();
        assert_eq!(portal.open("https://example.com/", &token()), Ok(()));

        harness.set_mode(PrivatePortalMode::Response(1));
        assert_eq!(
            portal.open("https://example.com/", &token()),
            Err(BrokerErrorCode::Cancelled)
        );

        harness.set_mode(PrivatePortalMode::Response(2));
        assert_eq!(
            portal.open("https://example.com/", &token()),
            Err(BrokerErrorCode::BackendFailed)
        );
        assert_eq!(harness.opens.load(AtomicOrdering::Relaxed), 3);
    }

    #[test]
    fn malformed_response_and_mismatched_handle_fail_closed() {
        let harness = PrivatePortalHarness::start(PrivatePortalMode::MalformedResponse);
        let portal = harness.portal();
        assert_eq!(
            portal.open("https://example.com/", &token()),
            Err(BrokerErrorCode::BackendFailed)
        );

        harness.set_mode(PrivatePortalMode::WrongHandle);
        assert_eq!(
            portal.open("https://example.com/", &token()),
            Err(BrokerErrorCode::BackendFailed)
        );
    }

    #[test]
    fn local_cancellation_closes_a_pending_portal_request() {
        let harness = PrivatePortalHarness::start(PrivatePortalMode::WaitForClose);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation =
            CancellationToken::new(cancelled.clone(), Duration::from_secs(2)).unwrap();
        let opens = harness.opens.clone();
        let canceller = thread::spawn(move || {
            wait_for_count(&opens, 1);
            cancelled.store(true, AtomicOrdering::Release);
        });

        let started = Instant::now();
        assert_eq!(
            harness.portal().open("https://example.com/", &cancellation),
            Err(BrokerErrorCode::Cancelled)
        );
        canceller.join().unwrap();
        wait_for_count(&harness.closes, 1);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn local_timeout_closes_a_pending_portal_request() {
        let harness = PrivatePortalHarness::start(PrivatePortalMode::WaitForClose);
        let cancellation =
            CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_millis(40))
                .unwrap();
        assert_eq!(
            harness.portal().open("https://example.com/", &cancellation),
            Err(BrokerErrorCode::Timeout)
        );
        wait_for_count(&harness.closes, 1);
    }

    #[test]
    #[ignore = "release-only adversarial portal lifecycle campaign"]
    fn security_campaign_uri_portal_responses_and_teardown_are_bounded() {
        let harness = PrivatePortalHarness::start(PrivatePortalMode::Response(0));
        let baseline_threads = process_thread_count();

        for round in 0..32 {
            let portal = harness.portal();
            match round % 5 {
                0 => {
                    harness.set_mode(PrivatePortalMode::Response(0));
                    assert_eq!(portal.open("https://example.com/", &token()), Ok(()));
                }
                1 => {
                    harness.set_mode(PrivatePortalMode::Response(1));
                    assert_eq!(
                        portal.open("https://example.com/", &token()),
                        Err(BrokerErrorCode::Cancelled)
                    );
                }
                2 => {
                    harness.set_mode(PrivatePortalMode::Response(2));
                    assert_eq!(
                        portal.open("https://example.com/", &token()),
                        Err(BrokerErrorCode::BackendFailed)
                    );
                }
                3 => {
                    harness.set_mode(PrivatePortalMode::MalformedResponse);
                    assert_eq!(
                        portal.open("https://example.com/", &token()),
                        Err(BrokerErrorCode::BackendFailed)
                    );
                }
                _ => {
                    harness.set_mode(PrivatePortalMode::WaitForClose);
                    let timeout = CancellationToken::new(
                        Arc::new(AtomicBool::new(false)),
                        Duration::from_millis(20),
                    )
                    .unwrap();
                    assert_eq!(
                        portal.open("https://example.com/", &timeout),
                        Err(BrokerErrorCode::Timeout)
                    );
                }
            }
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while process_thread_count() > baseline_threads + 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(process_thread_count() <= baseline_threads + 1);
    }

    #[test]
    fn notification_scope_source_label_and_removal_are_broker_owned() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        private_state(&state);
        let transport = FakeNotifications::default();
        let sent = transport.sent.clone();
        let removed = transport.removed.clone();
        let backend = NotificationBackend::new(transport, &state, "github:alice/notifier").unwrap();
        let notification = NotificationSend {
            id: "build-finished".into(),
            category: "status".into(),
            urgency: NotificationUrgencyValue::Normal,
            title: "Build complete".into(),
            body: "All checks passed".into(),
        };
        let request = notification_request(
            NOTIFICATION_SEND_OPERATION,
            notification.encode().unwrap(),
            notification_scope(2),
        );
        backend
            .authorize(&request, &mut ActivationLedger::new(8), 0)
            .unwrap();
        assert!(matches!(
            backend.execute(&request, &token()),
            BrokerResult::Success { .. }
        ));
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].0.starts_with("otb_"));
        assert!(sent[0].0.ends_with("_build-finished"));
        assert_eq!(sent[0].1, "github:alice/notifier");
        drop(sent);

        let remove = notification_request(
            NOTIFICATION_REMOVE_OPERATION,
            NotificationRemove {
                id: "build-finished".into(),
            }
            .encode()
            .unwrap(),
            notification_scope(2),
        );
        backend
            .authorize(&remove, &mut ActivationLedger::new(8), 0)
            .unwrap();
        assert!(matches!(
            backend.execute(&remove, &token()),
            BrokerResult::Success { .. }
        ));
        assert_eq!(
            removed.lock().unwrap().as_slice(),
            &[sent_id(&backend, "build-finished")]
        );
    }

    #[test]
    fn notification_spoofing_scope_and_restart_rate_abuse_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        private_state(&state);
        let transport = FakeNotifications::default();
        let backend =
            NotificationBackend::new(transport.clone(), &state, "github:alice/notifier").unwrap();
        let valid = NotificationSend {
            id: "one".into(),
            category: "status".into(),
            urgency: NotificationUrgencyValue::Normal,
            title: "Status".into(),
            body: String::new(),
        };
        let first = notification_request(
            NOTIFICATION_SEND_OPERATION,
            valid.encode().unwrap(),
            notification_scope(1),
        );
        backend
            .authorize(&first, &mut ActivationLedger::new(8), 0)
            .unwrap();
        drop(backend);

        let restarted =
            NotificationBackend::new(transport.clone(), &state, "github:alice/notifier").unwrap();
        assert_eq!(
            restarted.authorize(&first, &mut ActivationLedger::new(8), 0),
            Err(BrokerErrorCode::RateLimited)
        );

        for invalid in [
            NotificationSend {
                title: "System update\u{202e}exe".into(),
                ..valid.clone()
            },
            NotificationSend {
                category: "security".into(),
                ..valid.clone()
            },
            NotificationSend {
                urgency: NotificationUrgencyValue::Critical,
                ..valid.clone()
            },
            NotificationSend {
                id: "../../replace".into(),
                ..valid.clone()
            },
        ] {
            let request = notification_request(
                NOTIFICATION_SEND_OPERATION,
                invalid.encode().unwrap(),
                notification_scope(10),
            );
            assert!(
                restarted
                    .authorize(&request, &mut ActivationLedger::new(8), 0)
                    .is_err()
            );
        }
        assert!(transport.sent.lock().unwrap().is_empty());
    }

    fn sent_id<T>(backend: &NotificationBackend<T>, id: &str) -> String {
        backend.portal_id(id)
    }

    fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while counter.load(AtomicOrdering::Acquire) < expected && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(counter.load(AtomicOrdering::Acquire) >= expected);
    }

    fn process_thread_count() -> usize {
        fs::read_dir("/proc/self/task").unwrap().count()
    }
}
