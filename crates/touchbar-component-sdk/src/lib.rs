//! Guest-side bindings for sandboxed TouchBar components.
//!
//! This crate deliberately exposes the generic, policy-brokered operation
//! envelope rather than convenience functions such as `set_volume`. Typed
//! capability adapters can be layered on top without granting ambient OS
//! access or changing the core component world.

#![allow(clippy::too_many_arguments)] // generated canonical-ABI lowering

pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "plugin",
        pub_export_macro: true,
        default_bindings_module: "touchbar_component_sdk::bindings",
    });
}

pub mod kit;

pub use bindings::*;

use bindings::touchbar::plugin::broker;
pub use touchbar_broker_schema::{
    ClipboardReadRequest, ClipboardValue, ClipboardWriteRequest, CommandEvent, CommandOpened,
    CommandRunRequest, CommandValue, ContextFact, ContextFactValue, ContextReadRequest,
    ContextSnapshot, ContextSubscriptionOpened, DbusBus, DbusCall, DbusPropertiesChanged,
    DbusProperty, DbusReply, DbusReplyKind, DbusSubscription, DbusSubscriptionOpened, DbusValue,
    FilesystemDirectoryEntries, FilesystemEntry, FilesystemEntryKind, FilesystemFileChunk,
    FilesystemListDirectory, FilesystemMutationResult, FilesystemPath, FilesystemReadFile,
    FilesystemReadStream, FilesystemRename, FilesystemStreamEvent, FilesystemStreamOpened,
    FilesystemWriteFile, FilesystemWriteStream, FilesystemWriteStreamChunk,
    FilesystemWriteStreamCommit, FilesystemWriteStreamOpened, HttpRequest, HttpRequestMethod,
    HttpResponse, HttpStreamEvent, HttpStreamOpened, LocalConnect, LocalConnectionOpened,
    LocalFrameEvent, LocalSendFrame, MAX_CLIPBOARD_BYTES, MAX_CLIPBOARD_MIME_BYTES,
    MAX_DIRECTORY_ENTRIES, MAX_FILE_CHUNK_BYTES, MAX_FILE_STREAM_BYTES,
    MAX_FILE_STREAM_CHUNK_BYTES, MAX_FILE_WRITE_BYTES, MAX_FILE_WRITE_STREAM_CHUNK_BYTES,
    MAX_HTTP_INLINE_BODY_BYTES, MAX_HTTP_STREAM_CHUNK_BYTES, MAX_LOCAL_FRAME_BYTES,
    MAX_NOTIFICATION_BODY_BYTES, MAX_NOTIFICATION_TITLE_BYTES, MAX_URI_BYTES, NotificationRemove,
    NotificationSend, NotificationUrgencyValue, SecretReadRequest, SecretValue, UriOpenRequest,
};

/// An asynchronous broker operation accepted by the trusted native host.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub const fn into_raw(self) -> u64 {
        self.0
    }
}

/// Opaque identifier for a broker-owned, connection-scoped resource.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceId(u64);

impl ResourceId {
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    pub const fn into_raw(self) -> u64 {
        self.0
    }
}

/// Returns the complete current grant/availability snapshot.
pub fn capabilities() -> broker::CapabilitySnapshot {
    broker::capabilities()
}

/// Submits a bounded canonical payload and returns before backend completion.
///
/// The result arrives through `Guest::handle_host_event` as a
/// `HostEvent::Completion` carrying the same request ID.
pub fn request(
    capability: &str,
    operation: &str,
    payload: &[u8],
) -> Result<RequestId, broker::ErrorCode> {
    broker::request(capability, operation, payload).map(RequestId)
}

/// Requests cancellation of an operation previously submitted by this guest.
pub fn cancel(request_id: RequestId) -> Result<(), broker::ErrorCode> {
    broker::cancel(request_id.0)
}

/// Releases a broker-owned resource before the component exits.
pub fn close(resource_id: ResourceId) -> Result<(), broker::ErrorCode> {
    broker::close(resource_id.0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DbusSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UriSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardSubmitError {
    InvalidPayload,
    Broker(broker::ErrorCode),
}

/// Reads one exact approved MIME representation. The broker requires a fresh
/// physical touch activation and never reveals the other offered formats.
pub fn clipboard_read(mime_type: &str) -> Result<RequestId, ClipboardSubmitError> {
    let payload = ClipboardReadRequest {
        mime_type: mime_type.into(),
    }
    .encode()
    .map_err(|_| ClipboardSubmitError::InvalidPayload)?;
    request("clipboard.read.v1", "read", &payload).map_err(ClipboardSubmitError::Broker)
}

/// Replaces the regular clipboard with one exact approved MIME representation.
/// Primary selection is intentionally outside the v1 API.
pub fn clipboard_write(mime_type: &str, bytes: &[u8]) -> Result<RequestId, ClipboardSubmitError> {
    let payload = ClipboardWriteRequest {
        mime_type: mime_type.into(),
        bytes: bytes.to_vec(),
    }
    .encode()
    .map_err(|_| ClipboardSubmitError::InvalidPayload)?;
    request("clipboard.write.v1", "write", &payload).map_err(ClipboardSubmitError::Broker)
}

pub fn decode_clipboard(
    payload: &[u8],
) -> Result<ClipboardValue, touchbar_broker_schema::SchemaError> {
    ClipboardValue::decode(payload)
}

pub fn context_snapshot(
    request_value: &ContextReadRequest,
) -> Result<RequestId, ContextSubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| ContextSubmitError::InvalidPayload)?;
    request("context.read.v1", "snapshot", &payload).map_err(ContextSubmitError::Broker)
}

pub fn context_subscribe(
    request_value: &ContextReadRequest,
) -> Result<RequestId, ContextSubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| ContextSubmitError::InvalidPayload)?;
    request("context.read.v1", "subscribe", &payload).map_err(ContextSubmitError::Broker)
}

pub fn decode_context_snapshot(
    payload: &[u8],
) -> Result<ContextSnapshot, touchbar_broker_schema::SchemaError> {
    ContextSnapshot::decode(payload)
}

pub fn decode_context_subscription_opened(
    payload: &[u8],
) -> Result<(ResourceId, ContextSnapshot), touchbar_broker_schema::SchemaError> {
    ContextSubscriptionOpened::decode(payload)
        .map(|opened| (ResourceId(opened.resource_id), opened.snapshot))
}

/// Requests one exact logical secret. The broker requires a fresh physical
/// activation and resolves the logical name through user-owned grant state.
pub fn read_secret(logical_name: &str) -> Result<RequestId, SecretSubmitError> {
    let payload = SecretReadRequest {
        logical_name: logical_name.into(),
    }
    .encode()
    .map_err(|_| SecretSubmitError::InvalidPayload)?;
    request("secret.read.v1", "read", &payload).map_err(SecretSubmitError::Broker)
}

pub fn decode_secret(payload: &[u8]) -> Result<SecretValue, touchbar_broker_schema::SchemaError> {
    SecretValue::decode(payload)
}

pub fn local_connect(request_value: &LocalConnect) -> Result<RequestId, LocalSubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| LocalSubmitError::InvalidPayload)?;
    request("local.connect.v1", "connect", &payload).map_err(LocalSubmitError::Broker)
}

pub fn decode_local_connection_opened(
    payload: &[u8],
) -> Result<(ResourceId, u32), touchbar_broker_schema::SchemaError> {
    LocalConnectionOpened::decode(payload)
        .map(|opened| (ResourceId(opened.resource_id), opened.maximum_frame_bytes))
}

pub fn local_send(resource_id: ResourceId, bytes: &[u8]) -> Result<RequestId, LocalSubmitError> {
    let payload = LocalSendFrame {
        resource_id: resource_id.0,
        bytes: bytes.to_vec(),
    }
    .encode()
    .map_err(|_| LocalSubmitError::InvalidPayload)?;
    request("local.connect.v1", "send", &payload).map_err(LocalSubmitError::Broker)
}

pub fn decode_local_frame(
    payload: &[u8],
) -> Result<LocalFrameEvent, touchbar_broker_schema::SchemaError> {
    LocalFrameEvent::decode(payload)
}

pub fn send_notification(
    notification: &NotificationSend,
) -> Result<RequestId, NotificationSubmitError> {
    let payload = notification
        .encode()
        .map_err(|_| NotificationSubmitError::InvalidPayload)?;
    request("notification.send.v1", "send", &payload).map_err(NotificationSubmitError::Broker)
}

pub fn remove_notification(id: &str) -> Result<RequestId, NotificationSubmitError> {
    let payload = NotificationRemove { id: id.into() }
        .encode()
        .map_err(|_| NotificationSubmitError::InvalidPayload)?;
    request("notification.send.v1", "remove", &payload).map_err(NotificationSubmitError::Broker)
}

/// Requests one activation-gated URI open through the desktop portal.
pub fn open_uri(request_value: &UriOpenRequest) -> Result<RequestId, UriSubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| UriSubmitError::InvalidPayload)?;
    request("uri.open.v1", "open", &payload).map_err(UriSubmitError::Broker)
}

pub fn command_run(request_value: &CommandRunRequest) -> Result<RequestId, CommandSubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| CommandSubmitError::InvalidPayload)?;
    request("command.run.v1", "run", &payload).map_err(CommandSubmitError::Broker)
}

pub fn decode_command_opened(
    payload: &[u8],
) -> Result<ResourceId, touchbar_broker_schema::SchemaError> {
    CommandOpened::decode(payload).map(|opened| ResourceId(opened.resource_id))
}

pub fn decode_command_event(
    payload: &[u8],
) -> Result<CommandEvent, touchbar_broker_schema::SchemaError> {
    CommandEvent::decode(payload)
}

pub fn http_request(request_value: &HttpRequest) -> Result<RequestId, HttpSubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| HttpSubmitError::InvalidPayload)?;
    request("http.request.v1", "request", &payload).map_err(HttpSubmitError::Broker)
}

pub fn decode_http_response(
    payload: &[u8],
) -> Result<HttpResponse, touchbar_broker_schema::SchemaError> {
    HttpResponse::decode(payload)
}

/// Starts a bounded-response HTTP stream. The completion payload contains the
/// resource ID, followed by ordered metadata/chunk events and one terminal
/// complete event. Broker errors are terminal for that resource.
pub fn http_request_stream(request_value: &HttpRequest) -> Result<RequestId, HttpSubmitError> {
    let payload = request_value
        .encode()
        .map_err(|_| HttpSubmitError::InvalidPayload)?;
    request("http.request.v1", "request-stream", &payload).map_err(HttpSubmitError::Broker)
}

pub fn decode_http_stream_opened(
    payload: &[u8],
) -> Result<ResourceId, touchbar_broker_schema::SchemaError> {
    HttpStreamOpened::decode(payload).map(|opened| ResourceId(opened.resource_id))
}

pub fn decode_http_stream_event(
    payload: &[u8],
) -> Result<HttpStreamEvent, touchbar_broker_schema::SchemaError> {
    HttpStreamEvent::decode(payload)
}

pub fn filesystem_read_file(read: &FilesystemReadFile) -> Result<RequestId, FilesystemSubmitError> {
    let payload = read
        .encode()
        .map_err(|_| FilesystemSubmitError::InvalidPayload)?;
    request("filesystem.read.v1", "read-file", &payload).map_err(FilesystemSubmitError::Broker)
}

pub fn decode_filesystem_file_chunk(
    payload: &[u8],
) -> Result<FilesystemFileChunk, touchbar_broker_schema::SchemaError> {
    FilesystemFileChunk::decode(payload)
}

pub fn filesystem_read_stream(
    read: &FilesystemReadStream,
) -> Result<RequestId, FilesystemSubmitError> {
    let payload = read
        .encode()
        .map_err(|_| FilesystemSubmitError::InvalidPayload)?;
    request("filesystem.read.v1", "read-file-stream", &payload)
        .map_err(FilesystemSubmitError::Broker)
}

pub fn decode_filesystem_stream_opened(
    payload: &[u8],
) -> Result<ResourceId, touchbar_broker_schema::SchemaError> {
    FilesystemStreamOpened::decode(payload).map(|opened| ResourceId(opened.resource_id))
}

pub fn decode_filesystem_stream_event(
    payload: &[u8],
) -> Result<FilesystemStreamEvent, touchbar_broker_schema::SchemaError> {
    FilesystemStreamEvent::decode(payload)
}

pub fn filesystem_list_directory(
    list: &FilesystemListDirectory,
) -> Result<RequestId, FilesystemSubmitError> {
    let payload = list
        .encode()
        .map_err(|_| FilesystemSubmitError::InvalidPayload)?;
    request("filesystem.read.v1", "list-directory", &payload).map_err(FilesystemSubmitError::Broker)
}

pub fn decode_filesystem_directory_entries(
    payload: &[u8],
) -> Result<FilesystemDirectoryEntries, touchbar_broker_schema::SchemaError> {
    FilesystemDirectoryEntries::decode(payload)
}

fn filesystem_write_request(
    operation: &str,
    payload: Vec<u8>,
) -> Result<RequestId, FilesystemSubmitError> {
    request("filesystem.write.v1", operation, &payload).map_err(FilesystemSubmitError::Broker)
}

pub fn filesystem_create_file(
    write: &FilesystemWriteFile,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "create-file",
        write
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn filesystem_replace_file(
    write: &FilesystemWriteFile,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "replace-file",
        write
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn filesystem_append_file(
    write: &FilesystemWriteFile,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "append-file",
        write
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn filesystem_delete_file(path: &FilesystemPath) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "delete-file",
        path.encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn filesystem_create_directory(
    path: &FilesystemPath,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "create-directory",
        path.encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn filesystem_rename(rename: &FilesystemRename) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "rename",
        rename
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn decode_filesystem_mutation_result(
    payload: &[u8],
) -> Result<FilesystemMutationResult, touchbar_broker_schema::SchemaError> {
    FilesystemMutationResult::decode(payload)
}

/// Opens a broker-owned staging resource for a large create. Chunks must be
/// submitted one at a time in exact offset order; the destination remains
/// absent until an explicit commit succeeds.
pub fn filesystem_create_file_stream(
    stream: &FilesystemWriteStream,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "create-file-stream",
        stream
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

/// Opens a broker-owned staging resource for a large atomic replacement.
pub fn filesystem_replace_file_stream(
    stream: &FilesystemWriteStream,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "replace-file-stream",
        stream
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

/// Opens a broker-owned staging resource for a large atomic append. The
/// existing file is copied and revalidated inside a bounded worker before the
/// new bytes are committed as one replacement.
pub fn filesystem_append_file_stream(
    stream: &FilesystemWriteStream,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "append-file-stream",
        stream
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn decode_filesystem_write_stream_opened(
    payload: &[u8],
) -> Result<FilesystemWriteStreamOpened, touchbar_broker_schema::SchemaError> {
    FilesystemWriteStreamOpened::decode(payload)
}

pub fn filesystem_write_stream_chunk(
    chunk: &FilesystemWriteStreamChunk,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "write-stream-chunk",
        chunk
            .encode()
            .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

pub fn filesystem_write_stream_commit(
    resource_id: ResourceId,
) -> Result<RequestId, FilesystemSubmitError> {
    filesystem_write_request(
        "write-stream-commit",
        FilesystemWriteStreamCommit {
            resource_id: resource_id.into_raw(),
        }
        .encode()
        .map_err(|_| FilesystemSubmitError::InvalidPayload)?,
    )
}

/// Submit one typed `dbus.call.v1` operation. The host reconstructs the D-Bus
/// message only after matching every field and argument against the grant.
pub fn dbus_call(call: &DbusCall) -> Result<RequestId, DbusSubmitError> {
    let payload = call.encode().map_err(|_| DbusSubmitError::InvalidPayload)?;
    request("dbus.call.v1", "call", &payload).map_err(DbusSubmitError::Broker)
}

/// Decode the bounded reply carried by a broker completion event.
pub fn decode_dbus_reply(payload: &[u8]) -> Result<DbusReply, touchbar_broker_schema::SchemaError> {
    DbusReply::decode(payload)
}

/// Open one exact `dbus.subscribe.v1` signal subscription. Completion contains
/// a broker-owned resource ID; subsequent signal payloads arrive as ordered
/// resource events until closed or revoked.
pub fn dbus_subscribe(subscription: &DbusSubscription) -> Result<RequestId, DbusSubmitError> {
    let payload = subscription
        .encode()
        .map_err(|_| DbusSubmitError::InvalidPayload)?;
    request("dbus.subscribe.v1", "subscribe", &payload).map_err(DbusSubmitError::Broker)
}

pub fn decode_dbus_subscription_opened(
    payload: &[u8],
) -> Result<ResourceId, touchbar_broker_schema::SchemaError> {
    DbusSubscriptionOpened::decode(payload).map(|opened| ResourceId(opened.resource_id))
}

pub fn decode_dbus_properties_changed(
    payload: &[u8],
) -> Result<DbusPropertiesChanged, touchbar_broker_schema::SchemaError> {
    DbusPropertiesChanged::decode(payload)
}
