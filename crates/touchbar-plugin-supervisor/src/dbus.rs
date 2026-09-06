use std::{collections::HashMap, io, sync::Mutex, thread::JoinHandle, time::Duration};

use touchbar_broker_schema::{
    DbusBus as WireBus, DbusCall, DbusPropertiesChanged, DbusProperty, DbusReply, DbusReplyKind,
    DbusSubscription, DbusValue, SchemaError,
};
use touchbar_policy::{
    CapabilityId, CapabilityScope, DbusArgumentConstraint, DbusBus, DbusCallRule, DbusSignalRule,
};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};
use zbus::{
    MatchRule,
    blocking::{
        Connection, MessageIterator, connection::Builder as ConnectionBuilder, fdo::DBusProxy,
    },
    message::{Message, Type},
    names::{BusName, InterfaceName, MemberName, WellKnownName},
    zvariant::OwnedValue,
};

use crate::{
    ActivationExpectation, ActivationLedger, Backend, BackendRequest, CancellationToken,
    OpenedResource, ResourceBackend, ResourceEventSink, ResourceHandle, ResourceLimits,
};

pub const DBUS_CALL_OPERATION: &str = "call";
pub const DBUS_SUBSCRIBE_OPERATION: &str = "subscribe";
pub const DBUS_SUBSCRIPTION_RESERVED_BYTES: usize = 128 * 1024;
const DBUS_METHOD_TIMEOUT: Duration = Duration::from_secs(2);
const DBUS_MAX_QUEUED_MESSAGES: usize = 1;
const DBUS_MAX_REPLY_BYTES: usize = 64 * 1024;
const DBUS_MAX_SIGNAL_BYTES: usize = 64 * 1024;

/// The only side-effecting boundary used by [`DbusCallBackend`]. Tests provide
/// a fake transport; production uses [`ZbusTransport`]. Authorization always
/// completes before this trait is called.
pub trait DbusTransport: Send + Sync + 'static {
    fn call(
        &self,
        call: &DbusCall,
        allow_service_activation: bool,
        cancellation: &CancellationToken,
    ) -> Result<DbusReply, BrokerErrorCode>;
}

pub struct DbusCallBackend<T> {
    transport: T,
}

impl<T> DbusCallBackend<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: DbusTransport> Backend for DbusCallBackend<T> {
    fn authorize(
        &self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        let (call, rule) = decode_and_match(request)?;
        validate_supported_shape(&call)?;
        if rule.requires_user_activation {
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
        }
        Ok(())
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let (call, rule) = decode_and_match(request)?;
            validate_supported_shape(&call)?;
            let reply = self
                .transport
                .call(&call, rule.allow_service_activation, cancellation)?;
            let payload = reply.encode().map_err(|_| BrokerErrorCode::BackendFailed)?;
            Ok(BrokerResult::Success { payload })
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

fn decode_and_match(
    request: &BackendRequest,
) -> Result<(DbusCall, &DbusCallRule), BrokerErrorCode> {
    if request.capability != CapabilityId::DbusCallV1 || request.operation != DBUS_CALL_OPERATION {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let call = DbusCall::decode(&request.payload).map_err(schema_error)?;
    validate_call_identifiers(&call)?;
    let CapabilityScope::DbusCall(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let rule = scope
        .rules
        .iter()
        .find(|rule| rule_matches(rule, &call))
        .ok_or(BrokerErrorCode::OutOfScope)?;
    Ok((call, rule))
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

fn rule_matches(rule: &DbusCallRule, call: &DbusCall) -> bool {
    bus_matches(&rule.bus, call.bus)
        && destination_matches(&rule.destination, &call.destination)
        && rule.path == call.path
        && rule.interface == call.interface
        && rule.member == call.member
        && rule.signature == call.signature()
        && rule
            .arguments
            .iter()
            .all(|constraint| argument_matches(constraint, &call.arguments))
}

fn bus_matches(policy: &DbusBus, wire: WireBus) -> bool {
    matches!(
        (policy, wire),
        (DbusBus::Session, WireBus::Session) | (DbusBus::System, WireBus::System)
    )
}

fn destination_matches(pattern: &str, destination: &str) -> bool {
    match pattern.strip_suffix(".*") {
        Some(prefix) => destination
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('.') && suffix.len() > 1),
        None => destination == pattern,
    }
}

fn argument_matches(constraint: &DbusArgumentConstraint, arguments: &[String]) -> bool {
    let Some(argument) = arguments.get(usize::from(constraint.index)) else {
        return false;
    };
    match (&constraint.equals_string, &constraint.one_of_strings) {
        (Some(expected), None) => argument == expected,
        (None, Some(expected)) => expected.contains(argument),
        _ => false,
    }
}

fn validate_supported_shape(call: &DbusCall) -> Result<(), BrokerErrorCode> {
    match (call.arguments.as_slice(), call.reply) {
        ([], DbusReplyKind::Unit)
        | ([_, _], DbusReplyKind::VariantString | DbusReplyKind::VariantI64) => Ok(()),
        _ => Err(BrokerErrorCode::Unsupported),
    }
}

pub struct ZbusTransport {
    session: Mutex<Option<Connection>>,
    system: Mutex<Option<Connection>>,
    session_address: Option<String>,
    system_address: Option<String>,
    method_timeout: Duration,
}

impl Default for ZbusTransport {
    fn default() -> Self {
        Self {
            session: Mutex::new(None),
            system: Mutex::new(None),
            session_address: None,
            system_address: None,
            method_timeout: DBUS_METHOD_TIMEOUT,
        }
    }
}

impl ZbusTransport {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn for_session_address(address: String) -> Self {
        Self {
            session_address: Some(address),
            ..Self::default()
        }
    }

    #[cfg(test)]
    fn for_session_address_with_timeout(address: String, method_timeout: Duration) -> Self {
        Self {
            session_address: Some(address),
            method_timeout,
            ..Self::default()
        }
    }

    fn fresh_connection(&self, bus: WireBus) -> Result<Connection, BrokerErrorCode> {
        let address = match bus {
            WireBus::Session => self.session_address.as_deref(),
            WireBus::System => self.system_address.as_deref(),
        };
        let builder = match address {
            Some(address) => ConnectionBuilder::address(address),
            None => match bus {
                WireBus::Session => ConnectionBuilder::session(),
                WireBus::System => ConnectionBuilder::system(),
            },
        }
        .map_err(|_| BrokerErrorCode::Unavailable)?;
        builder
            .max_queued(DBUS_MAX_QUEUED_MESSAGES)
            .method_timeout(self.method_timeout)
            .build()
            .map_err(|_| BrokerErrorCode::Unavailable)
    }

    fn connection(&self, bus: WireBus) -> Result<Connection, BrokerErrorCode> {
        let slot = match bus {
            WireBus::Session => &self.session,
            WireBus::System => &self.system,
        };
        let mut slot = slot.lock().map_err(|_| BrokerErrorCode::Internal)?;
        if let Some(connection) = slot.as_ref() {
            return Ok(connection.clone());
        }
        let connection = self.fresh_connection(bus)?;
        *slot = Some(connection.clone());
        Ok(connection)
    }
}

impl DbusTransport for ZbusTransport {
    fn call(
        &self,
        call: &DbusCall,
        allow_service_activation: bool,
        cancellation: &CancellationToken,
    ) -> Result<DbusReply, BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let connection = self.connection(call.bus)?;
        let destination = if allow_service_activation {
            call.destination.clone()
        } else {
            resolve_name_owner(&connection, &call.destination)?
        };
        let message = match call.arguments.as_slice() {
            [] => connection.call_method(
                Some(destination.as_str()),
                call.path.as_str(),
                Some(call.interface.as_str()),
                call.member.as_str(),
                &(),
            ),
            [first, second] => connection.call_method(
                Some(destination.as_str()),
                call.path.as_str(),
                Some(call.interface.as_str()),
                call.member.as_str(),
                &(first.as_str(), second.as_str()),
            ),
            _ => return Err(BrokerErrorCode::Unsupported),
        }
        .map_err(map_call_error)?;
        validate_incoming_message(&message, DBUS_MAX_REPLY_BYTES)?;
        let reply = match (call.arguments.as_slice(), call.reply) {
            ([], DbusReplyKind::Unit) => {
                validate_body_signature(&message, "")?;
                message
                    .body()
                    .deserialize::<()>()
                    .map_err(|_| BrokerErrorCode::BackendFailed)?;
                DbusReply::Unit
            }
            ([_, _], DbusReplyKind::VariantString) => {
                validate_body_signature(&message, "v")?;
                let value: OwnedValue = message
                    .body()
                    .deserialize()
                    .map_err(|_| BrokerErrorCode::BackendFailed)?;
                DbusReply::String(
                    String::try_from(value).map_err(|_| BrokerErrorCode::BackendFailed)?,
                )
            }
            ([_, _], DbusReplyKind::VariantI64) => {
                validate_body_signature(&message, "v")?;
                let value: OwnedValue = message
                    .body()
                    .deserialize()
                    .map_err(|_| BrokerErrorCode::BackendFailed)?;
                DbusReply::I64(i64::try_from(value).map_err(|_| BrokerErrorCode::BackendFailed)?)
            }
            _ => return Err(BrokerErrorCode::Unsupported),
        };
        cancellation.reason().map_or(Ok(reply), Err)
    }
}

fn validate_call_identifiers(call: &DbusCall) -> Result<(), BrokerErrorCode> {
    WellKnownName::try_from(call.destination.as_str())
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    zbus::zvariant::ObjectPath::try_from(call.path.as_str())
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    InterfaceName::try_from(call.interface.as_str())
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    MemberName::try_from(call.member.as_str()).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    Ok(())
}

fn resolve_name_owner(
    connection: &Connection,
    well_known_name: &str,
) -> Result<String, BrokerErrorCode> {
    let proxy = DBusProxy::new(connection).map_err(map_call_error)?;
    let name = BusName::try_from(well_known_name).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    proxy
        .get_name_owner(name)
        .map(|owner| owner.to_string())
        .map_err(map_fdo_call_error)
}

fn map_call_error(error: zbus::Error) -> BrokerErrorCode {
    match error {
        zbus::Error::InputOutput(error) if error.kind() == io::ErrorKind::TimedOut => {
            BrokerErrorCode::Timeout
        }
        _ => BrokerErrorCode::BackendFailed,
    }
}

fn map_fdo_call_error(error: zbus::fdo::Error) -> BrokerErrorCode {
    match error {
        zbus::fdo::Error::ZBus(zbus::Error::InputOutput(error))
            if error.kind() == io::ErrorKind::TimedOut =>
        {
            BrokerErrorCode::Timeout
        }
        zbus::fdo::Error::NameHasNoOwner(_) | zbus::fdo::Error::ServiceUnknown(_) => {
            BrokerErrorCode::Unavailable
        }
        _ => BrokerErrorCode::BackendFailed,
    }
}

fn validate_incoming_message(
    message: &Message,
    maximum_body_bytes: usize,
) -> Result<(), BrokerErrorCode> {
    if message.body().len() > maximum_body_bytes || message.header().unix_fds().unwrap_or(0) != 0 {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(())
}

fn validate_body_signature(message: &Message, expected: &str) -> Result<(), BrokerErrorCode> {
    let body = message.body();
    let signature = body.signature().to_string();
    let signature = signature
        .strip_prefix('(')
        .and_then(|signature| signature.strip_suffix(')'))
        .unwrap_or(&signature);
    (signature == expected)
        .then_some(())
        .ok_or(BrokerErrorCode::BackendFailed)
}

pub trait DbusSubscriptionTransport: Send + Sync + 'static {
    fn open(
        &self,
        subscription: &DbusSubscription,
        events: ResourceEventSink,
    ) -> Result<Box<dyn ResourceHandle>, BrokerErrorCode>;
}

pub struct DbusSubscriptionBackend<T> {
    transport: T,
}

impl<T> DbusSubscriptionBackend<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: DbusSubscriptionTransport> ResourceBackend for DbusSubscriptionBackend<T> {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        let CapabilityScope::DbusSubscribe(scope) = &request.authorized_scope else {
            return None;
        };
        (request.capability == CapabilityId::DbusSubscribeV1
            && request.operation == DBUS_SUBSCRIBE_OPERATION)
            .then_some(ResourceLimits {
                reserved_buffered_bytes: DBUS_SUBSCRIPTION_RESERVED_BYTES,
                maximum_events_per_second: scope.maximum_events_per_second,
            })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_and_match_subscription(request).map(|_| ())
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        let (subscription, _) = decode_and_match_subscription(request)?;
        let handle = self.transport.open(&subscription, events)?;
        let response_payload = touchbar_broker_schema::DbusSubscriptionOpened { resource_id }
            .encode()
            .map_err(|_| BrokerErrorCode::Internal)?;
        Ok(OpenedResource {
            handle,
            response_payload,
        })
    }
}

fn decode_and_match_subscription(
    request: &BackendRequest,
) -> Result<(DbusSubscription, &DbusSignalRule), BrokerErrorCode> {
    if request.capability != CapabilityId::DbusSubscribeV1
        || request.operation != DBUS_SUBSCRIBE_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let subscription = DbusSubscription::decode(&request.payload).map_err(schema_error)?;
    validate_subscription_identifiers(&subscription)?;
    validate_supported_subscription(&subscription)?;
    let CapabilityScope::DbusSubscribe(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let rule = scope
        .rules
        .iter()
        .find(|rule| signal_rule_matches(rule, &subscription))
        .ok_or(BrokerErrorCode::OutOfScope)?;
    Ok((subscription, rule))
}

fn signal_rule_matches(rule: &DbusSignalRule, subscription: &DbusSubscription) -> bool {
    bus_matches(&rule.bus, subscription.bus)
        && destination_matches(&rule.sender, &subscription.sender)
        && rule.path == subscription.path
        && rule.interface == subscription.interface
        && rule.member == subscription.member
        && rule.signature == subscription.signature
        && rule.argument_zero == subscription.argument_zero
}

fn validate_supported_subscription(subscription: &DbusSubscription) -> Result<(), BrokerErrorCode> {
    if subscription.sender.ends_with(".*")
        || subscription.interface != "org.freedesktop.DBus.Properties"
        || subscription.member != "PropertiesChanged"
        || subscription.signature != "sa{sv}as"
        || subscription.argument_zero.is_none()
    {
        return Err(BrokerErrorCode::Unsupported);
    }
    Ok(())
}

fn validate_subscription_identifiers(
    subscription: &DbusSubscription,
) -> Result<(), BrokerErrorCode> {
    WellKnownName::try_from(subscription.sender.as_str())
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    zbus::zvariant::ObjectPath::try_from(subscription.path.as_str())
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    InterfaceName::try_from(subscription.interface.as_str())
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    MemberName::try_from(subscription.member.as_str())
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    if let Some(argument_zero) = &subscription.argument_zero {
        InterfaceName::try_from(argument_zero.as_str())
            .map_err(|_| BrokerErrorCode::InvalidRequest)?;
    }
    Ok(())
}

impl DbusSubscriptionTransport for ZbusTransport {
    fn open(
        &self,
        subscription: &DbusSubscription,
        events: ResourceEventSink,
    ) -> Result<Box<dyn ResourceHandle>, BrokerErrorCode> {
        let connection = self.fresh_connection(subscription.bus)?;
        let owner_changes = owner_change_iterator(&connection, &subscription.sender)?;
        let owner = resolve_name_owner(&connection, &subscription.sender)?;
        let mut builder = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(owner.as_str())
            .map_err(|_| BrokerErrorCode::InvalidRequest)?
            .path(subscription.path.as_str())
            .map_err(|_| BrokerErrorCode::InvalidRequest)?
            .interface(subscription.interface.as_str())
            .map_err(|_| BrokerErrorCode::InvalidRequest)?
            .member(subscription.member.as_str())
            .map_err(|_| BrokerErrorCode::InvalidRequest)?;
        if let Some(argument_zero) = &subscription.argument_zero {
            builder = builder
                .arg(0, argument_zero.as_str())
                .map_err(|_| BrokerErrorCode::InvalidRequest)?;
        }
        let iterator = MessageIterator::for_match_rule(
            builder.build(),
            &connection,
            Some(DBUS_MAX_QUEUED_MESSAGES),
        )
        .map_err(|_| BrokerErrorCode::Unavailable)?;
        if resolve_name_owner(&connection, &subscription.sender).as_deref() != Ok(owner.as_str()) {
            return Err(BrokerErrorCode::Unavailable);
        }
        let close_connection = connection.clone();
        let monitor_connection = connection.clone();
        let signal_events = events.clone();
        let owner_name = subscription.sender.clone();
        let subscription = subscription.clone();
        let signal_thread = std::thread::Builder::new()
            .name("touchbar-dbus-subscription".into())
            .spawn(move || {
                run_subscription(
                    iterator,
                    &monitor_connection,
                    &owner,
                    &subscription,
                    &signal_events,
                )
            })
            .map_err(|_| BrokerErrorCode::Unavailable)?;
        let owner_close_connection = connection.clone();
        let owner_thread = match std::thread::Builder::new()
            .name("touchbar-dbus-owner-watch".into())
            .spawn(move || {
                run_owner_watch(owner_changes, &owner_name, &events, owner_close_connection)
            }) {
            Ok(thread) => thread,
            Err(_) => {
                let _ = connection.close();
                let _ = signal_thread.join();
                return Err(BrokerErrorCode::Unavailable);
            }
        };
        Ok(Box::new(ZbusSubscriptionHandle {
            connection: Some(close_connection),
            threads: vec![signal_thread, owner_thread],
        }))
    }
}

fn owner_change_iterator(
    connection: &Connection,
    well_known_name: &str,
) -> Result<MessageIterator, BrokerErrorCode> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|_| BrokerErrorCode::Internal)?
        .path("/org/freedesktop/DBus")
        .map_err(|_| BrokerErrorCode::Internal)?
        .interface("org.freedesktop.DBus")
        .map_err(|_| BrokerErrorCode::Internal)?
        .member("NameOwnerChanged")
        .map_err(|_| BrokerErrorCode::Internal)?
        .arg(0, well_known_name)
        .map_err(|_| BrokerErrorCode::InvalidRequest)?
        .build();
    MessageIterator::for_match_rule(rule, connection, Some(DBUS_MAX_QUEUED_MESSAGES))
        .map_err(|_| BrokerErrorCode::Unavailable)
}

fn run_owner_watch(
    mut iterator: MessageIterator,
    well_known_name: &str,
    events: &ResourceEventSink,
    connection: Connection,
) {
    let result = match iterator.next() {
        Some(Ok(message)) => decode_owner_change(&message, well_known_name),
        Some(Err(_)) => Err(BrokerErrorCode::BackendFailed),
        None => Err(BrokerErrorCode::Unavailable),
    };
    match result {
        Ok(()) => events.finish(BrokerErrorCode::Unavailable),
        Err(error) => events.finish(error),
    }
    let _ = connection.close();
}

fn decode_owner_change(message: &Message, well_known_name: &str) -> Result<(), BrokerErrorCode> {
    validate_incoming_message(message, DBUS_MAX_SIGNAL_BYTES)?;
    let header = message.header();
    if message.message_type() != Type::Signal
        || header.path().map(|path| path.as_str()) != Some("/org/freedesktop/DBus")
        || header.interface().map(|name| name.as_str()) != Some("org.freedesktop.DBus")
        || header.member().map(|name| name.as_str()) != Some("NameOwnerChanged")
    {
        return Err(BrokerErrorCode::BackendFailed);
    }
    validate_body_signature(message, "sss")?;
    let (name, _old_owner, _new_owner): (String, String, String) = message
        .body()
        .deserialize()
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    (name == well_known_name)
        .then_some(())
        .ok_or(BrokerErrorCode::BackendFailed)
}

fn run_subscription(
    mut iterator: MessageIterator,
    connection: &Connection,
    owner: &str,
    subscription: &DbusSubscription,
    events: &ResourceEventSink,
) {
    loop {
        let Some(message) = iterator.next() else {
            events.finish(BrokerErrorCode::Unavailable);
            return;
        };
        let message = match message {
            Ok(message) => message,
            Err(_) => {
                events.finish(BrokerErrorCode::BackendFailed);
                return;
            }
        };
        match resolve_name_owner(connection, &subscription.sender) {
            Ok(current_owner) if current_owner == owner => {}
            Ok(_) => {
                events.finish(BrokerErrorCode::Unavailable);
                return;
            }
            Err(error) => {
                events.finish(error);
                return;
            }
        }
        if message.header().sender().map(|sender| sender.as_str()) != Some(owner) {
            events.finish(BrokerErrorCode::BackendFailed);
            return;
        }
        let payload = match decode_properties_changed(&message, subscription)
            .and_then(|event| event.encode().map_err(schema_error))
        {
            Ok(payload) => payload,
            Err(error) => {
                events.finish(error);
                return;
            }
        };
        let _ = events.emit(payload);
        if events.is_finished() {
            return;
        }
    }
}

fn decode_properties_changed(
    message: &Message,
    subscription: &DbusSubscription,
) -> Result<DbusPropertiesChanged, BrokerErrorCode> {
    validate_incoming_message(message, DBUS_MAX_SIGNAL_BYTES)?;
    let header = message.header();
    if message.message_type() != Type::Signal
        || header.path().map(|path| path.as_str()) != Some(subscription.path.as_str())
        || header.interface().map(|name| name.as_str()) != Some(subscription.interface.as_str())
        || header.member().map(|name| name.as_str()) != Some(subscription.member.as_str())
    {
        return Err(BrokerErrorCode::BackendFailed);
    }
    let body = message.body();
    let signature = body.signature().to_string();
    let signature = signature
        .strip_prefix('(')
        .and_then(|signature| signature.strip_suffix(')'))
        .unwrap_or(&signature);
    if signature != subscription.signature {
        return Err(BrokerErrorCode::BackendFailed);
    }
    let (interface_name, properties, invalidated): (
        String,
        HashMap<String, OwnedValue>,
        Vec<String>,
    ) = body
        .deserialize()
        .map_err(|_| BrokerErrorCode::BackendFailed)?;
    if subscription.argument_zero.as_deref() != Some(interface_name.as_str()) {
        return Err(BrokerErrorCode::BackendFailed);
    }
    let mut changed_properties = properties
        .iter()
        .filter_map(|(name, value)| {
            scalar_value(value).map(|value| DbusProperty {
                name: name.clone(),
                value,
            })
        })
        .collect::<Vec<_>>();
    changed_properties.sort_by(|left, right| left.name.cmp(&right.name));
    let mut invalidated_properties = invalidated;
    invalidated_properties.sort();
    Ok(DbusPropertiesChanged {
        interface_name,
        changed_properties,
        invalidated_properties,
    })
}

fn scalar_value(value: &OwnedValue) -> Option<DbusValue> {
    if let Ok(value) = u8::try_from(value) {
        Some(DbusValue::U8(value))
    } else if let Ok(value) = bool::try_from(value) {
        Some(DbusValue::Bool(value))
    } else if let Ok(value) = i16::try_from(value) {
        Some(DbusValue::I16(value))
    } else if let Ok(value) = u16::try_from(value) {
        Some(DbusValue::U16(value))
    } else if let Ok(value) = i32::try_from(value) {
        Some(DbusValue::I32(value))
    } else if let Ok(value) = u32::try_from(value) {
        Some(DbusValue::U32(value))
    } else if let Ok(value) = i64::try_from(value) {
        Some(DbusValue::I64(value))
    } else if let Ok(value) = u64::try_from(value) {
        Some(DbusValue::U64(value))
    } else if let Ok(value) = f64::try_from(value) {
        value.is_finite().then_some(DbusValue::F64(value))
    } else if let Ok(value) = <&str>::try_from(value) {
        Some(DbusValue::String(value.into()))
    } else {
        None
    }
}

struct ZbusSubscriptionHandle {
    connection: Option<Connection>,
    threads: Vec<JoinHandle<()>>,
}

impl ResourceHandle for ZbusSubscriptionHandle {
    fn close(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.close();
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for ZbusSubscriptionHandle {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        DbusCallScope, DbusSubscribeScope, PackageInstance, Provenance, RuntimeKind,
    };
    use touchbar_protocol::broker_ipc::{ActivationContext, ActivationOrigin};

    use super::*;
    use crate::{
        AsyncBrokerRuntime, ConnectionIdentity, ExecutorLimits, HostEvent, HostEventQueue,
        LifecycleLimits, ResourceManager,
    };

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
            // SAFETY: pid came directly from the private dbus-daemon spawned
            // by this test and SIGTERM does not access process memory.
            let _ = unsafe { libc::kill(self.pid, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                // SAFETY: signal zero checks whether this exact PID remains.
                if unsafe { libc::kill(self.pid, 0) } != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    struct PrivateMpris;

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl PrivateMpris {
        #[zbus(property)]
        fn playback_status(&self) -> &'static str {
            "Paused"
        }

        fn play_pause(&self) {}

        fn stall(&self) {
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    struct FakeTransport {
        calls: Arc<AtomicUsize>,
        reply: DbusReply,
    }

    impl DbusTransport for FakeTransport {
        fn call(
            &self,
            _call: &DbusCall,
            _allow_service_activation: bool,
            _cancellation: &CancellationToken,
        ) -> Result<DbusReply, BrokerErrorCode> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.reply.clone())
        }
    }

    fn call(destination: &str, member: &str, reply: DbusReplyKind) -> DbusCall {
        let (interface, arguments) = if member == "Get" {
            (
                "org.freedesktop.DBus.Properties",
                vec![
                    "org.mpris.MediaPlayer2.Player".into(),
                    "PlaybackStatus".into(),
                ],
            )
        } else {
            ("org.mpris.MediaPlayer2.Player", Vec::new())
        };
        DbusCall {
            bus: WireBus::Session,
            destination: destination.into(),
            path: "/org/mpris/MediaPlayer2".into(),
            interface: interface.into(),
            member: member.into(),
            arguments,
            reply,
        }
    }

    fn rule(member: &str, activation: bool) -> DbusCallRule {
        let is_get = member == "Get";
        DbusCallRule {
            bus: DbusBus::Session,
            destination: "org.mpris.MediaPlayer2.*".into(),
            path: "/org/mpris/MediaPlayer2".into(),
            interface: if is_get {
                "org.freedesktop.DBus.Properties"
            } else {
                "org.mpris.MediaPlayer2.Player"
            }
            .into(),
            member: member.into(),
            signature: if is_get { "ss" } else { "" }.into(),
            arguments: if is_get {
                vec![
                    DbusArgumentConstraint {
                        index: 0,
                        equals_string: Some("org.mpris.MediaPlayer2.Player".into()),
                        one_of_strings: None,
                    },
                    DbusArgumentConstraint {
                        index: 1,
                        equals_string: Some("PlaybackStatus".into()),
                        one_of_strings: None,
                    },
                ]
            } else {
                Vec::new()
            },
            allow_service_activation: false,
            requires_user_activation: activation,
        }
    }

    fn request(call: DbusCall, rules: impl IntoIterator<Item = DbusCallRule>) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 7,
                package: PackageInstance {
                    source: GithubSource::new("alice", "media").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::DbusCallV1,
            authorized_scope: CapabilityScope::DbusCall(DbusCallScope {
                rules: rules.into_iter().collect(),
            }),
            bindings: Default::default(),
            activation: None,
            operation: DBUS_CALL_OPERATION.into(),
            payload: call.encode().unwrap(),
        }
    }

    fn activation(sequence: u64) -> ActivationContext {
        ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 4,
            item_id: "media".into(),
            widget_id: 9,
            input_sequence: sequence,
            deadline_monotonic_micros: 200,
        }
    }

    fn cancellation() -> CancellationToken {
        CancellationToken::new(
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            std::time::Duration::from_secs(1),
        )
        .unwrap()
    }

    #[test]
    fn scoped_property_read_reaches_transport_and_returns_typed_value() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = DbusCallBackend::new(FakeTransport {
            calls: Arc::clone(&calls),
            reply: DbusReply::String("Playing".into()),
        });
        let request = request(
            call(
                "org.mpris.MediaPlayer2.demo",
                "Get",
                DbusReplyKind::VariantString,
            ),
            [rule("Get", false)],
        );
        backend
            .authorize(&request, &mut ActivationLedger::new(8), 100)
            .unwrap();
        let result = backend.execute(&request, &cancellation());
        let BrokerResult::Success { payload } = result else {
            panic!("property read failed")
        };
        assert_eq!(
            DbusReply::decode(&payload),
            Ok(DbusReply::String("Playing".into()))
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn out_of_scope_destination_and_argument_never_reach_transport() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = DbusCallBackend::new(FakeTransport {
            calls: Arc::clone(&calls),
            reply: DbusReply::Unit,
        });
        let denied_destination = request(
            call("org.example.Secrets", "Get", DbusReplyKind::VariantString),
            [rule("Get", false)],
        );
        assert_eq!(
            backend.authorize(&denied_destination, &mut ActivationLedger::new(8), 100,),
            Err(BrokerErrorCode::OutOfScope)
        );
        let mut wrong_property = call(
            "org.mpris.MediaPlayer2.demo",
            "Get",
            DbusReplyKind::VariantString,
        );
        wrong_property.arguments[1] = "Metadata".into();
        let request = request(wrong_property, [rule("Get", false)]);
        assert_eq!(
            backend.authorize(&request, &mut ActivationLedger::new(8), 100),
            Err(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn malformed_dbus_identifiers_never_reach_transport() {
        let cases = [
            (
                "org.mpris.MediaPlayer2.demo/../../org.example.Secrets",
                "/org/mpris/MediaPlayer2",
                "org.mpris.MediaPlayer2.Player",
                "PlayPause",
            ),
            (
                "org.mpris.MediaPlayer2.demo",
                "/org/mpris/../MediaPlayer2",
                "org.mpris.MediaPlayer2.Player",
                "PlayPause",
            ),
            (
                "org.mpris.MediaPlayer2.demo",
                "/org/mpris/MediaPlayer2",
                "org.mpris.MediaPlayer2.Player/evil",
                "PlayPause",
            ),
            (
                "org.mpris.MediaPlayer2.demo",
                "/org/mpris/MediaPlayer2",
                "org.mpris.MediaPlayer2.Player",
                "PlayPause.Evil",
            ),
        ];
        for (destination, path, interface, member) in cases {
            let calls = Arc::new(AtomicUsize::new(0));
            let backend = DbusCallBackend::new(FakeTransport {
                calls: Arc::clone(&calls),
                reply: DbusReply::Unit,
            });
            let mut wire_call = call(destination, member, DbusReplyKind::Unit);
            wire_call.path = path.into();
            wire_call.interface = interface.into();
            let mut matching_rule = rule(member, false);
            matching_rule.destination = destination.into();
            matching_rule.path = path.into();
            matching_rule.interface = interface.into();
            let request = request(wire_call, [matching_rule]);
            assert_eq!(
                backend.authorize(&request, &mut ActivationLedger::new(1), 0),
                Err(BrokerErrorCode::InvalidRequest)
            );
            assert_eq!(
                backend.execute(&request, &cancellation()),
                BrokerResult::Error(BrokerErrorCode::InvalidRequest)
            );
            assert_eq!(calls.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn system_bus_request_cannot_reuse_a_session_bus_rule() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = DbusCallBackend::new(FakeTransport {
            calls: Arc::clone(&calls),
            reply: DbusReply::Unit,
        });
        let mut wire_call = call(
            "org.mpris.MediaPlayer2.demo",
            "PlayPause",
            DbusReplyKind::Unit,
        );
        wire_call.bus = WireBus::System;
        let request = request(wire_call, [rule("PlayPause", false)]);
        assert_eq!(
            backend.authorize(&request, &mut ActivationLedger::new(1), 0),
            Err(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn every_scoped_call_field_is_jointly_authorized() {
        let mut cases = Vec::new();
        let baseline = call(
            "org.mpris.MediaPlayer2.demo",
            "Get",
            DbusReplyKind::VariantString,
        );
        let mut changed = baseline.clone();
        changed.bus = WireBus::System;
        cases.push(changed);
        let mut changed = baseline.clone();
        changed.destination = "org.example.Player".into();
        cases.push(changed);
        let mut changed = baseline.clone();
        changed.path = "/org/mpris/Other".into();
        cases.push(changed);
        let mut changed = baseline.clone();
        changed.interface = "org.example.Properties".into();
        cases.push(changed);
        let mut changed = baseline.clone();
        changed.member = "Set".into();
        cases.push(changed);
        let mut changed = baseline.clone();
        changed.arguments[1] = "Metadata".into();
        cases.push(changed);
        let mut changed = baseline.clone();
        changed.arguments.pop();
        cases.push(changed);
        let mut changed = baseline;
        changed.arguments.push("extra".into());
        cases.push(changed);

        let calls = Arc::new(AtomicUsize::new(0));
        let backend = DbusCallBackend::new(FakeTransport {
            calls: Arc::clone(&calls),
            reply: DbusReply::String("Playing".into()),
        });
        for changed in cases {
            let request = request(changed, [rule("Get", false)]);
            assert_eq!(
                backend.authorize(&request, &mut ActivationLedger::new(1), 0),
                Err(BrokerErrorCode::OutOfScope)
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn play_pause_requires_one_fresh_physical_activation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = DbusCallBackend::new(FakeTransport {
            calls: Arc::clone(&calls),
            reply: DbusReply::Unit,
        });
        let mut request = request(
            call(
                "org.mpris.MediaPlayer2.demo",
                "PlayPause",
                DbusReplyKind::Unit,
            ),
            [rule("PlayPause", true)],
        );
        let mut activations = ActivationLedger::new(8);
        assert_eq!(
            backend.authorize(&request, &mut activations, 100),
            Err(BrokerErrorCode::ActivationRequired)
        );
        request.activation = Some(activation(12));
        assert_eq!(backend.authorize(&request, &mut activations, 100), Ok(()));
        assert_eq!(
            backend.authorize(&request, &mut activations, 100),
            Err(BrokerErrorCode::ActivationRequired)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    struct NoopResource;

    impl ResourceHandle for NoopResource {
        fn close(&mut self) {}
    }

    struct FakeSubscriptionTransport {
        opens: Arc<AtomicUsize>,
    }

    impl DbusSubscriptionTransport for FakeSubscriptionTransport {
        fn open(
            &self,
            _subscription: &DbusSubscription,
            events: ResourceEventSink,
        ) -> Result<Box<dyn ResourceHandle>, BrokerErrorCode> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            events.emit(
                DbusPropertiesChanged {
                    interface_name: "org.mpris.MediaPlayer2.Player".into(),
                    changed_properties: vec![DbusProperty {
                        name: "PlaybackStatus".into(),
                        value: DbusValue::String("Playing".into()),
                    }],
                    invalidated_properties: Vec::new(),
                }
                .encode()
                .unwrap(),
            )?;
            Ok(Box::new(NoopResource))
        }
    }

    struct CountingResource {
        closes: Arc<AtomicUsize>,
        closed: bool,
    }

    impl ResourceHandle for CountingResource {
        fn close(&mut self) {
            if !self.closed {
                self.closed = true;
                self.closes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    struct CountingSubscriptionTransport {
        opens: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
    }

    impl DbusSubscriptionTransport for CountingSubscriptionTransport {
        fn open(
            &self,
            _subscription: &DbusSubscription,
            _events: ResourceEventSink,
        ) -> Result<Box<dyn ResourceHandle>, BrokerErrorCode> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(CountingResource {
                closes: self.closes.clone(),
                closed: false,
            }))
        }
    }

    fn subscription(sender: &str) -> DbusSubscription {
        DbusSubscription {
            bus: WireBus::Session,
            sender: sender.into(),
            path: "/org/mpris/MediaPlayer2".into(),
            interface: "org.freedesktop.DBus.Properties".into(),
            member: "PropertiesChanged".into(),
            signature: "sa{sv}as".into(),
            argument_zero: Some("org.mpris.MediaPlayer2.Player".into()),
        }
    }

    fn signal_rule(sender: &str) -> DbusSignalRule {
        DbusSignalRule {
            bus: DbusBus::Session,
            sender: sender.into(),
            path: "/org/mpris/MediaPlayer2".into(),
            interface: "org.freedesktop.DBus.Properties".into(),
            member: "PropertiesChanged".into(),
            signature: "sa{sv}as".into(),
            argument_zero: Some("org.mpris.MediaPlayer2.Player".into()),
        }
    }

    fn subscription_request(
        subscription: DbusSubscription,
        rules: impl IntoIterator<Item = DbusSignalRule>,
    ) -> BackendRequest {
        let mut request = request(
            call(
                "org.mpris.MediaPlayer2.demo",
                "Get",
                DbusReplyKind::VariantString,
            ),
            [rule("Get", false)],
        );
        request.capability = CapabilityId::DbusSubscribeV1;
        request.authorized_scope = CapabilityScope::DbusSubscribe(DbusSubscribeScope {
            rules: rules.into_iter().collect(),
            maximum_events_per_second: 30,
        });
        request.operation = DBUS_SUBSCRIBE_OPERATION.into();
        request.payload = subscription.encode().unwrap();
        request
    }

    fn emit_playback_status(service: &Connection, subscription: &DbusSubscription, status: &str) {
        use zbus::zvariant::Value;

        let mut properties = HashMap::new();
        properties.insert("PlaybackStatus", Value::from(status));
        service
            .emit_signal(
                Option::<&str>::None,
                subscription.path.as_str(),
                subscription.interface.as_str(),
                subscription.member.as_str(),
                &(
                    "org.mpris.MediaPlayer2.Player",
                    properties,
                    Vec::<String>::new(),
                ),
            )
            .unwrap();
    }

    fn wait_readable(descriptor: i32, message: &str) {
        let mut poll = libc::pollfd {
            fd: descriptor,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized writable pollfd and the
        // resource manager keeps descriptor live throughout this call.
        let ready = unsafe { libc::poll(&mut poll, 1, 5_000) };
        assert_eq!(ready, 1, "{message}");
        assert_ne!(poll.revents & libc::POLLIN, 0, "{message}");
    }

    #[test]
    fn exact_subscription_opens_one_ordered_resource_event() {
        let opens = Arc::new(AtomicUsize::new(0));
        let backend = DbusSubscriptionBackend::new(FakeSubscriptionTransport {
            opens: Arc::clone(&opens),
        });
        let mut manager = ResourceManager::new(4).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(backend));
        let request = subscription_request(
            subscription("org.mpris.MediaPlayer2.playerctld"),
            [signal_rule("org.mpris.MediaPlayer2.*")],
        );
        manager
            .open(
                4,
                &request,
                ResourceLimits {
                    reserved_buffered_bytes: DBUS_SUBSCRIPTION_RESERVED_BYTES,
                    maximum_events_per_second: 30,
                },
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();
        let mut events = HostEventQueue::new(2, 4);
        manager.pump(&mut events, 1);
        let Some(HostEvent::ResourceEvent {
            resource_id,
            sequence,
            result: BrokerResult::Success { payload },
        }) = events.pop()
        else {
            panic!("expected a typed resource event")
        };
        assert_eq!((resource_id, sequence), (4, 1));
        assert_eq!(
            DbusPropertiesChanged::decode(&payload)
                .unwrap()
                .changed_properties[0]
                .value,
            DbusValue::String("Playing".into())
        );
        assert_eq!(opens.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn out_of_scope_subscription_never_opens_transport() {
        let opens = Arc::new(AtomicUsize::new(0));
        let backend = DbusSubscriptionBackend::new(FakeSubscriptionTransport {
            opens: Arc::clone(&opens),
        });
        let mut manager = ResourceManager::new(4).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(backend));
        let request = subscription_request(
            subscription("org.example.Secrets"),
            [signal_rule("org.mpris.MediaPlayer2.*")],
        );
        assert_eq!(
            manager.open(
                4,
                &request,
                ResourceLimits {
                    reserved_buffered_bytes: DBUS_SUBSCRIPTION_RESERVED_BYTES,
                    maximum_events_per_second: 30,
                },
                &mut ActivationLedger::new(1),
                0,
            ),
            Err(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(opens.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn malformed_subscription_identifiers_never_open_transport() {
        let cases = [
            (
                "org.mpris.MediaPlayer2.playerctld/escape",
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
            ),
            (
                "org.mpris.MediaPlayer2.playerctld",
                "../org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
            ),
            (
                "org.mpris.MediaPlayer2.playerctld",
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties/evil",
                "PropertiesChanged",
            ),
            (
                "org.mpris.MediaPlayer2.playerctld",
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged.Evil",
            ),
        ];
        for (sender, path, interface, member) in cases {
            let opens = Arc::new(AtomicUsize::new(0));
            let backend = DbusSubscriptionBackend::new(FakeSubscriptionTransport {
                opens: Arc::clone(&opens),
            });
            let mut requested = subscription(sender);
            requested.path = path.into();
            requested.interface = interface.into();
            requested.member = member.into();
            let mut matching_rule = signal_rule(sender);
            matching_rule.path = path.into();
            matching_rule.interface = interface.into();
            matching_rule.member = member.into();
            let request = subscription_request(requested, [matching_rule]);
            assert_eq!(
                backend.authorize(&request, &mut ActivationLedger::new(1), 0),
                Err(BrokerErrorCode::InvalidRequest)
            );
            assert_eq!(opens.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn properties_changed_decoder_forwards_safe_scalars_and_omits_containers() {
        use zbus::zvariant::Value;

        let mut properties = HashMap::new();
        properties.insert("PlaybackStatus", Value::from("Playing"));
        properties.insert("Volume", Value::from(0.75_f64));
        properties.insert("Metadata", Value::from(vec!["nested", "value"]));
        let message = Message::signal(
            "/org/mpris/MediaPlayer2",
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .unwrap()
        .build(&(
            "org.mpris.MediaPlayer2.Player",
            properties,
            Vec::<String>::new(),
        ))
        .unwrap();
        assert_eq!(message.body().signature().to_string(), "(sa{sv}as)");
        let event =
            decode_properties_changed(&message, &subscription("org.mpris.MediaPlayer2.playerctld"))
                .unwrap();
        assert_eq!(event.changed_properties.len(), 2);
        assert_eq!(event.changed_properties[0].name, "PlaybackStatus");
        assert_eq!(
            event.changed_properties[0].value,
            DbusValue::String("Playing".into())
        );
        assert_eq!(event.changed_properties[1].name, "Volume");
        assert_eq!(event.changed_properties[1].value, DbusValue::F64(0.75));
    }

    #[test]
    fn oversized_signal_is_rejected_before_deserialization() {
        use zbus::zvariant::Value;

        let mut properties = HashMap::new();
        properties.insert(
            "Artwork",
            Value::from("x".repeat(DBUS_MAX_SIGNAL_BYTES + 1)),
        );
        let message = Message::signal(
            "/org/mpris/MediaPlayer2",
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .unwrap()
        .build(&(
            "org.mpris.MediaPlayer2.Player",
            properties,
            Vec::<String>::new(),
        ))
        .unwrap();
        assert_eq!(
            decode_properties_changed(&message, &subscription("org.mpris.MediaPlayer2.playerctld")),
            Err(BrokerErrorCode::QuotaExceeded)
        );
    }

    #[cfg(unix)]
    #[test]
    fn incoming_unix_file_descriptors_are_rejected_before_deserialization() {
        use std::os::fd::AsFd;
        use zbus::zvariant::Fd;

        let file = std::fs::File::open("/dev/null").unwrap();
        let message = Message::signal(
            "/org/mpris/MediaPlayer2",
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .unwrap()
        .build(&(Fd::from(file.as_fd())))
        .unwrap();
        assert_eq!(message.header().unix_fds(), Some(1));
        assert_eq!(
            decode_properties_changed(&message, &subscription("org.mpris.MediaPlayer2.playerctld")),
            Err(BrokerErrorCode::QuotaExceeded)
        );
    }

    #[test]
    fn raw_reply_envelope_rejects_oversize_and_wrong_signature() {
        let request = Message::method_call("/org/mpris/MediaPlayer2", "Get")
            .unwrap()
            .destination("org.mpris.MediaPlayer2.playerctld")
            .unwrap()
            .interface("org.freedesktop.DBus.Properties")
            .unwrap()
            .build(&())
            .unwrap();
        let oversized = Message::method_return(&request.header())
            .unwrap()
            .build(&("x".repeat(DBUS_MAX_REPLY_BYTES + 1)))
            .unwrap();
        assert_eq!(
            validate_incoming_message(&oversized, DBUS_MAX_REPLY_BYTES),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        let wrong_signature = Message::method_return(&request.header())
            .unwrap()
            .build(&(7_i64))
            .unwrap();
        assert_eq!(
            validate_body_signature(&wrong_signature, "v"),
            Err(BrokerErrorCode::BackendFailed)
        );
    }

    #[test]
    fn stalled_service_is_cut_off_by_the_transport_timeout() {
        let bus = PrivateBus::start();
        let service = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.playerctld")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", PrivateMpris)
            .unwrap()
            .build()
            .unwrap();
        let backend = DbusCallBackend::new(ZbusTransport::for_session_address_with_timeout(
            bus.address.clone(),
            Duration::from_millis(30),
        ));
        let request = request(
            call(
                "org.mpris.MediaPlayer2.playerctld",
                "Stall",
                DbusReplyKind::Unit,
            ),
            [rule("Stall", false)],
        );
        let started = Instant::now();
        assert_eq!(
            backend.execute(&request, &cancellation()),
            BrokerResult::Error(BrokerErrorCode::Timeout)
        );
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(service);
    }

    #[test]
    fn nonactivating_call_to_an_unowned_name_fails_without_starting_it() {
        let bus = PrivateBus::start();
        let backend = DbusCallBackend::new(ZbusTransport::for_session_address(bus.address.clone()));
        let request = request(
            call(
                "org.mpris.MediaPlayer2.absent",
                "PlayPause",
                DbusReplyKind::Unit,
            ),
            [rule("PlayPause", false)],
        );
        assert_eq!(
            backend.execute(&request, &cancellation()),
            BrokerResult::Error(BrokerErrorCode::Unavailable)
        );
    }

    #[test]
    fn subscription_stops_if_the_approved_name_changes_owner() {
        use zbus::names::WellKnownName;

        let bus = PrivateBus::start();
        let service = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.playerctld")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", PrivateMpris)
            .unwrap()
            .build()
            .unwrap();
        let backend =
            DbusSubscriptionBackend::new(ZbusTransport::for_session_address(bus.address.clone()));
        let mut manager = ResourceManager::new(4).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(backend));
        let requested = subscription("org.mpris.MediaPlayer2.playerctld");
        let request = subscription_request(
            requested,
            [signal_rule("org.mpris.MediaPlayer2.playerctld")],
        );
        manager
            .open(
                1,
                &request,
                ResourceLimits {
                    reserved_buffered_bytes: DBUS_SUBSCRIPTION_RESERVED_BYTES,
                    maximum_events_per_second: 30,
                },
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();

        let bus_proxy = DBusProxy::new(&service).unwrap();
        bus_proxy
            .release_name(WellKnownName::try_from("org.mpris.MediaPlayer2.playerctld").unwrap())
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = HostEventQueue::new(2, 4);
        loop {
            manager.pump(&mut events, 1);
            if let Some(HostEvent::ResourceEvent { result, .. }) = events.pop() {
                assert_eq!(result, BrokerResult::Error(BrokerErrorCode::Unavailable));
                break;
            }
            assert!(Instant::now() < deadline, "owner churn was not detected");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!manager.close(1));
        drop(service);
    }

    #[test]
    fn private_bus_exercises_real_call_and_subscription_transports() {
        use zbus::zvariant::Value;

        let bus = PrivateBus::start();
        let service = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.playerctld")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", PrivateMpris)
            .unwrap()
            .build()
            .unwrap();

        let call_backend =
            DbusCallBackend::new(ZbusTransport::for_session_address(bus.address.clone()));
        let call_request = request(
            call(
                "org.mpris.MediaPlayer2.playerctld",
                "Get",
                DbusReplyKind::VariantString,
            ),
            [rule("Get", false)],
        );
        call_backend
            .authorize(&call_request, &mut ActivationLedger::new(1), 0)
            .unwrap();
        let BrokerResult::Success { payload } =
            call_backend.execute(&call_request, &cancellation())
        else {
            panic!("real D-Bus property call failed")
        };
        assert_eq!(
            DbusReply::decode(&payload),
            Ok(DbusReply::String("Paused".into()))
        );

        let subscription_backend =
            DbusSubscriptionBackend::new(ZbusTransport::for_session_address(bus.address.clone()));
        let mut manager = ResourceManager::new(4).unwrap();
        manager.register(
            CapabilityId::DbusSubscribeV1,
            Arc::new(subscription_backend),
        );
        let subscription = subscription("org.mpris.MediaPlayer2.playerctld");
        let subscription_request = subscription_request(
            subscription.clone(),
            [signal_rule("org.mpris.MediaPlayer2.playerctld")],
        );
        manager
            .open(
                1,
                &subscription_request,
                ResourceLimits {
                    reserved_buffered_bytes: DBUS_SUBSCRIPTION_RESERVED_BYTES,
                    maximum_events_per_second: 30,
                },
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();

        let mut properties = HashMap::new();
        properties.insert("PlaybackStatus", Value::from("Playing"));
        service
            .emit_signal(
                Option::<&str>::None,
                subscription.path.as_str(),
                subscription.interface.as_str(),
                subscription.member.as_str(),
                &(
                    "org.mpris.MediaPlayer2.Player",
                    properties,
                    Vec::<String>::new(),
                ),
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = HostEventQueue::new(2, 4);
        let payload = loop {
            manager.pump(&mut events, 1);
            if let Some(HostEvent::ResourceEvent {
                result: BrokerResult::Success { payload },
                ..
            }) = events.pop()
            {
                break payload;
            }
            assert!(Instant::now() < deadline, "real signal was not delivered");
            std::thread::sleep(Duration::from_millis(1));
        };
        let event = DbusPropertiesChanged::decode(&payload).unwrap();
        assert_eq!(
            event.changed_properties[0].value,
            DbusValue::String("Playing".into())
        );
        assert!(manager.close(1));
        drop(service);
    }

    #[test]
    #[ignore = "release-gate private-bus signal-flood and owner-churn campaign"]
    fn security_campaign_dbus_signal_flood_and_owner_churn_are_bounded() {
        use zbus::{blocking::fdo::DBusProxy, names::WellKnownName};

        const SENDER: &str = "org.mpris.MediaPlayer2.playerctld";
        const FLOOD_ROUNDS: u64 = 32;
        const OWNER_ROUNDS: u64 = 32;

        let bus = PrivateBus::start();
        let service = ConnectionBuilder::address(bus.address.as_str())
            .unwrap()
            .name(SENDER)
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", PrivateMpris)
            .unwrap()
            .build()
            .unwrap();
        let backend =
            DbusSubscriptionBackend::new(ZbusTransport::for_session_address(bus.address.clone()));
        let mut manager = ResourceManager::new(1).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(backend));
        let requested = subscription(SENDER);
        let subscribe = subscription_request(requested.clone(), [signal_rule(SENDER)]);
        for round in 0..FLOOD_ROUNDS {
            let resource_id = 1 + round;
            manager
                .open(
                    resource_id,
                    &subscribe,
                    ResourceLimits {
                        reserved_buffered_bytes: DBUS_SUBSCRIPTION_RESERVED_BYTES,
                        maximum_events_per_second: 30,
                    },
                    &mut ActivationLedger::new(1),
                    0,
                )
                .unwrap();

            // Wait until the first event occupies the one-message broker queue
            // without draining its eventfd. The next signal must then produce
            // one explicit overflow and terminate the subscription.
            emit_playback_status(&service, &requested, &format!("Playing-{round}-first"));
            wait_readable(manager.event_fd(), "first flood signal was not queued");
            emit_playback_status(&service, &requested, &format!("Playing-{round}-second"));
            let deadline = Instant::now() + Duration::from_secs(5);
            while manager.resource_finished(resource_id) != Some(true) {
                assert!(Instant::now() < deadline, "signal flood did not terminate");
                std::thread::sleep(Duration::from_millis(1));
            }

            let mut events = HostEventQueue::new(4, 8);
            assert_eq!(manager.pump(&mut events, 1), vec![resource_id]);
            let mut successes = 0;
            let mut terminal = None;
            let mut dropped = 0;
            while let Some(event) = events.pop() {
                match event {
                    HostEvent::ResourceEvent {
                        resource_id: actual,
                        result: BrokerResult::Success { payload },
                        ..
                    } => {
                        assert_eq!(actual, resource_id);
                        DbusPropertiesChanged::decode(&payload).unwrap();
                        successes += 1;
                    }
                    HostEvent::ResourceEvent {
                        resource_id: actual,
                        result: BrokerResult::Error(error),
                        ..
                    } => {
                        assert_eq!(actual, resource_id);
                        terminal = Some(error);
                    }
                    HostEvent::Overflow { dropped_events, .. } => dropped += dropped_events,
                    other => panic!("unexpected flood event: {other:?}"),
                }
            }
            assert_eq!(
                (successes, terminal, dropped),
                (1, Some(BrokerErrorCode::QuotaExceeded), 1)
            );
            assert_eq!(manager.resource_count(), 0);
        }
        drop(service);

        let backend =
            DbusSubscriptionBackend::new(ZbusTransport::for_session_address(bus.address.clone()));
        let mut manager = ResourceManager::new(4).unwrap();
        manager.register(CapabilityId::DbusSubscribeV1, Arc::new(backend));
        let mut events = HostEventQueue::new(4, 8);
        for round in 0..OWNER_ROUNDS {
            let service = ConnectionBuilder::address(bus.address.as_str())
                .unwrap()
                .name(SENDER)
                .unwrap()
                .serve_at("/org/mpris/MediaPlayer2", PrivateMpris)
                .unwrap()
                .build()
                .unwrap();
            let resource_id = 100 + round;
            manager
                .open(
                    resource_id,
                    &subscribe,
                    ResourceLimits {
                        reserved_buffered_bytes: DBUS_SUBSCRIPTION_RESERVED_BYTES,
                        maximum_events_per_second: 30,
                    },
                    &mut ActivationLedger::new(1),
                    0,
                )
                .unwrap();
            DBusProxy::new(&service)
                .unwrap()
                .release_name(WellKnownName::try_from(SENDER).unwrap())
                .unwrap();

            let deadline = Instant::now() + Duration::from_secs(5);
            let terminal = loop {
                manager.pump(&mut events, 2);
                let terminal = events.pop().map(|event| match event {
                    HostEvent::ResourceEvent {
                        resource_id: actual,
                        result: BrokerResult::Error(error),
                        ..
                    } => {
                        assert_eq!(actual, resource_id);
                        error
                    }
                    other => panic!("unexpected owner-churn event: {other:?}"),
                });
                if let Some(terminal) = terminal {
                    break terminal;
                }
                assert!(Instant::now() < deadline, "owner churn was not detected");
                std::thread::sleep(Duration::from_millis(1));
            };
            assert_eq!(terminal, BrokerErrorCode::Unavailable);
            assert_eq!(manager.resource_count(), 0);
            drop(service);
        }
    }

    #[test]
    #[ignore = "release-gate D-Bus subscription resource-pressure campaign"]
    fn security_campaign_dbus_subscription_resources_are_reclaimable() {
        const CAPACITY: usize = 16;

        let opens = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(DbusSubscriptionBackend::new(
            CountingSubscriptionTransport {
                opens: opens.clone(),
                closes: closes.clone(),
            },
        ));
        let mut runtime = AsyncBrokerRuntime::new(
            ExecutorLimits::default(),
            LifecycleLimits::default(),
            64,
            128,
        )
        .unwrap();
        runtime.register_resource(CapabilityId::DbusSubscribeV1, backend);
        let subscribe = subscription_request(
            subscription("org.mpris.MediaPlayer2.playerctld"),
            [signal_rule("org.mpris.MediaPlayer2.playerctld")],
        );

        let mut resource_ids = Vec::new();
        for _ in 0..CAPACITY {
            let (resource_id, _) = runtime
                .open_resource(&subscribe, &mut ActivationLedger::new(1), 0)
                .unwrap();
            resource_ids.push(resource_id);
        }
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY);
        assert_eq!(
            runtime.lifecycle().buffered_bytes(),
            CAPACITY * DBUS_SUBSCRIPTION_RESERVED_BYTES
        );
        assert_eq!(opens.load(Ordering::Relaxed), CAPACITY);
        assert_eq!(
            runtime.open_resource(&subscribe, &mut ActivationLedger::new(1), 0),
            Err(BrokerErrorCode::QuotaExceeded)
        );
        assert_eq!(runtime.lifecycle().resource_count(), CAPACITY);
        assert_eq!(opens.load(Ordering::Relaxed), CAPACITY);

        let effect = runtime.revoke(&CapabilityId::DbusSubscribeV1, 1);
        assert_eq!(effect.closed_resources, resource_ids);
        assert_eq!(
            effect.released_buffered_bytes,
            CAPACITY * DBUS_SUBSCRIPTION_RESERVED_BYTES
        );
        assert_eq!(runtime.lifecycle().resource_count(), 0);
        assert_eq!(runtime.lifecycle().buffered_bytes(), 0);
        assert_eq!(closes.load(Ordering::Relaxed), CAPACITY);

        let mut last_resource_id = CAPACITY as u64;
        for _ in 0..64 {
            let (resource_id, _) = runtime
                .open_resource(&subscribe, &mut ActivationLedger::new(1), 0)
                .unwrap();
            assert!(resource_id > last_resource_id);
            last_resource_id = resource_id;
            assert!(runtime.close_resource(resource_id));
            assert_eq!(runtime.lifecycle().resource_count(), 0);
            assert_eq!(runtime.lifecycle().buffered_bytes(), 0);
        }
        assert_eq!(opens.load(Ordering::Relaxed), CAPACITY + 64);
        assert_eq!(closes.load(Ordering::Relaxed), CAPACITY + 64);

        runtime
            .open_resource(&subscribe, &mut ActivationLedger::new(1), 0)
            .unwrap();
        assert_eq!(runtime.lifecycle().resource_count(), 1);
        drop(runtime);
        assert_eq!(opens.load(Ordering::Relaxed), CAPACITY + 65);
        assert_eq!(closes.load(Ordering::Relaxed), CAPACITY + 65);
    }
}
