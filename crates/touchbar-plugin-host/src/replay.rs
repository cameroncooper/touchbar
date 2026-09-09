use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use touchbar_broker_schema::{
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
    LocalFrameEvent, LocalSendFrame, MAX_CONTEXT_VALUE_BYTES, MAX_FILE_WRITE_STREAM_CHUNK_BYTES,
    NotificationRemove, NotificationSend, NotificationUrgencyValue, SecretReadRequest, SecretValue,
    UriOpenRequest,
};
use touchbar_plugin_host::{
    Appearance, BrokerClient, ColorScheme, ComponentPresentationCommand,
    ComponentPresentationDismissal, ComponentPresentationEndReason, ComponentPresentationEvent,
    ComponentPresentationLifecycle, ComponentPresentationPlacement, HostedItem, InputActivation,
    InputEvent, InputKind, PluginHost,
};
use touchbar_plugin_supervisor::{
    ActivationLedger, Backend, BackendRequest, CLIPBOARD_READ_OPERATION, CLIPBOARD_WRITE_OPERATION,
    COMMAND_RUN_OPERATION, CancellationToken, ClipboardAuthorization, CommandRunBackend,
    ConnectionIdentity, DBUS_CALL_OPERATION, DbusCallBackend, DbusSubscriptionBackend,
    DbusSubscriptionTransport, DbusTransport, FILESYSTEM_APPEND_FILE_OPERATION,
    FILESYSTEM_APPEND_FILE_STREAM_OPERATION, FILESYSTEM_CREATE_DIRECTORY_OPERATION,
    FILESYSTEM_CREATE_FILE_OPERATION, FILESYSTEM_CREATE_FILE_STREAM_OPERATION,
    FILESYSTEM_DELETE_FILE_OPERATION, FILESYSTEM_LIST_DIRECTORY_OPERATION,
    FILESYSTEM_READ_FILE_OPERATION, FILESYSTEM_READ_STREAM_OPERATION, FILESYSTEM_RENAME_OPERATION,
    FILESYSTEM_REPLACE_FILE_OPERATION, FILESYSTEM_REPLACE_FILE_STREAM_OPERATION,
    FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION, FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
    FilesystemReadBackend, FilesystemWriteStreamCommandAuthorization, HTTP_REQUEST_OPERATION,
    HTTP_STREAM_OPERATION, HttpRequestBackend, HttpResolver, HttpTransport, HttpTransportResponse,
    LOCAL_CONNECT_OPERATION, LOCAL_MAXIMUM_EVENTS_PER_SECOND, LOCAL_SEND_OPERATION,
    LocalConnectionAuthorization, NOTIFICATION_REMOVE_OPERATION, NOTIFICATION_SEND_OPERATION,
    NotificationAuthorization, ResourceBackend, ResourceEventSink, ResourceHandle,
    SECRET_READ_OPERATION, URI_OPEN_OPERATION, UriOpenBackend, UriOpenTransport,
    authorize_clipboard_request, authorize_filesystem_write_mutation_request,
    authorize_filesystem_write_stream_begin_request, authorize_filesystem_write_stream_command,
    authorize_local_connect_request, authorize_local_send_request, authorize_notification_request,
    authorize_secret_read_request, validate_secret_value,
};
use touchbar_policy::{
    CapabilityId, CapabilityRegistry, CapabilityRequest, CapabilityScope, ClipboardBinding,
    FileKind, FilesystemMountBinding, FilesystemWriteScope, GrantBindings, LocalEndpointBinding,
    PackageInstance, Provenance, RuntimeKind, SecretBinding,
};
use touchbar_protocol::broker_ipc::{
    ActivationContext, ActivationOrigin, BrokerErrorCode, BrokerResult, CallbackPhase,
    CapabilityState, HostMessage, Seqpacket, SupervisorMessage, WireCapabilityStatus,
    monotonic_micros,
};
use touchbar_ui::{
    Color, Contact, ContactPhase, InteractionMap, InteractionState, MotionPolicy, Point, Primitive,
    Rect, Representation, SemanticNode, SemanticRole, Theme, UiEvent,
};

const MAX_SCENARIO_BYTES: u64 = 1024 * 1024;
const MAX_STEPS: usize = 1024;
const MAX_RASTER_SNAPSHOTS: usize = 64;
const MAX_TIME_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    version: u32,
    item: String,
    width: u32,
    #[serde(default)]
    appearance: AppearanceFixture,
    #[serde(default)]
    context: BTreeMap<String, ContextFixtureValue>,
    #[serde(default)]
    broker: BrokerFixture,
    steps: Vec<Step>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BrokerFixture {
    clipboard_requests: Vec<ClipboardRequestFixture>,
    command_runs: Vec<CommandRunFixture>,
    dbus_calls: Vec<DbusCallFixture>,
    dbus_subscriptions: Vec<DbusSubscriptionFixture>,
    filesystem_reads: Vec<FilesystemReadFixture>,
    filesystem_writes: Vec<FilesystemWriteFixture>,
    http_requests: Vec<HttpRequestFixture>,
    local_connections: Vec<LocalConnectionFixture>,
    notification_requests: Vec<NotificationRequestFixture>,
    secret_reads: Vec<SecretReadFixture>,
    uri_opens: Vec<UriOpenFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretReadFixture {
    id: String,
    request: SecretReadFixtureRequest,
    responses: Vec<SecretResultFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretReadFixtureRequest {
    logical_name: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum SecretResultFixture {
    Value {
        content_type: String,
        body: ByteFixture,
    },
    Error {
        code: BrokerErrorFixture,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardRequestFixture {
    id: String,
    operation: ClipboardOperationFixture,
    request: ClipboardRequestFixtureValue,
    responses: Vec<ClipboardResultFixture>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ClipboardOperationFixture {
    Read,
    Write,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ClipboardRequestFixtureValue {
    Read(ClipboardReadFixture),
    Write(ClipboardWriteFixture),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardReadFixture {
    mime_type: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardWriteFixture {
    mime_type: String,
    body: ByteFixture,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum ClipboardResultFixture {
    Value {
        mime_type: String,
        body: ByteFixture,
    },
    Success,
    Error {
        code: BrokerErrorFixture,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationRequestFixture {
    id: String,
    operation: NotificationOperationFixture,
    request: NotificationRequestFixtureValue,
    responses: Vec<DesktopResultFixture>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum NotificationOperationFixture {
    Send,
    Remove,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum NotificationRequestFixtureValue {
    Send(NotificationSendFixture),
    Remove(NotificationRemoveFixture),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationSendFixture {
    id: String,
    category: String,
    urgency: NotificationUrgencyFixture,
    title: String,
    body: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationRemoveFixture {
    id: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum NotificationUrgencyFixture {
    Low,
    Normal,
    Critical,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UriOpenFixture {
    id: String,
    request: UriOpenFixtureRequest,
    responses: Vec<DesktopResultFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UriOpenFixtureRequest {
    uri: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum DesktopResultFixture {
    Success,
    Error { code: BrokerErrorFixture },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpRequestFixture {
    id: String,
    operation: HttpOperationFixture,
    request: HttpRequestFixtureRequest,
    responses: Vec<HttpResultFixture>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum HttpOperationFixture {
    Request,
    RequestStream,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpRequestFixtureRequest {
    method: HttpMethodFixture,
    url: String,
    accept: Option<String>,
    content_type: Option<String>,
    body: Option<ByteFixture>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum HttpMethodFixture {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ByteFixture {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum HttpResultFixture {
    Response {
        status: u16,
        final_url: Option<String>,
        content_type: Option<String>,
        etag: Option<String>,
        #[serde(default)]
        body: Option<ByteFixture>,
    },
    Stream {
        status: u16,
        final_url: Option<String>,
        content_type: Option<String>,
        etag: Option<String>,
        chunks: Vec<ByteFixture>,
        terminal_error: Option<BrokerErrorFixture>,
    },
    Error {
        code: BrokerErrorFixture,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandRunFixture {
    id: String,
    request: CommandRunFixtureRequest,
    responses: Vec<CommandResultFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandRunFixtureRequest {
    command_id: String,
    #[serde(default)]
    values: Vec<CommandValueFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum CommandValueFixture {
    Integer { name: String, value: i64 },
    FixedEnum { name: String, value: String },
    Text { name: String, value: String },
    ApprovedFile { name: String, path: String },
    Url { name: String, value: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum CommandResultFixture {
    Completed {
        #[serde(default)]
        output: Vec<CommandOutputFixture>,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Failed {
        #[serde(default)]
        output: Vec<CommandOutputFixture>,
        code: BrokerErrorFixture,
    },
    Error {
        code: BrokerErrorFixture,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum CommandOutputFixture {
    Stdout { body: ByteFixture },
    Stderr { body: ByteFixture },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemReadFixture {
    id: String,
    operation: FilesystemReadOperationFixture,
    request: FilesystemReadRequestFixture,
    responses: Vec<FilesystemReadResultFixture>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FilesystemReadOperationFixture {
    ReadFile,
    ReadFileStream,
    ListDirectory,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum FilesystemReadRequestFixture {
    Read(FilesystemReadFileFixture),
    List(FilesystemListDirectoryFixture),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemReadFileFixture {
    mount: String,
    path: String,
    offset: u64,
    maximum_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemListDirectoryFixture {
    mount: String,
    path: String,
    maximum_entries: u16,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum FilesystemReadResultFixture {
    File {
        total_size: u64,
        #[serde(default)]
        body: Option<ByteFixture>,
    },
    Directory {
        #[serde(default)]
        entries: Vec<FilesystemEntryFixture>,
        truncated: bool,
    },
    Stream {
        total_size: u64,
        #[serde(default)]
        chunks: Vec<ByteFixture>,
        terminal_error: Option<BrokerErrorFixture>,
    },
    Error {
        code: BrokerErrorFixture,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemEntryFixture {
    name: String,
    kind: FilesystemEntryKindFixture,
    size: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FilesystemEntryKindFixture {
    RegularFile,
    Directory,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemWriteFixture {
    id: String,
    operation: FilesystemWriteOperationFixture,
    request: FilesystemWriteRequestFixture,
    #[serde(default)]
    responses: Vec<FilesystemWriteResultFixture>,
    #[serde(default)]
    open_error: Option<BrokerErrorFixture>,
    #[serde(default)]
    chunks: Vec<FilesystemWriteChunkFixture>,
    #[serde(default)]
    commits: Vec<FilesystemWriteResultFixture>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FilesystemWriteOperationFixture {
    CreateFile,
    ReplaceFile,
    AppendFile,
    DeleteFile,
    Rename,
    CreateDirectory,
    CreateFileStream,
    ReplaceFileStream,
    AppendFileStream,
}

impl FilesystemWriteOperationFixture {
    fn is_stream(self) -> bool {
        matches!(
            self,
            Self::CreateFileStream | Self::ReplaceFileStream | Self::AppendFileStream
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum FilesystemWriteRequestFixture {
    Write(FilesystemWriteFileFixture),
    Path(FilesystemWritePathFixture),
    Rename(FilesystemWriteRenameFixture),
    Stream(FilesystemWriteStreamFixture),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemWriteFileFixture {
    mount: String,
    path: String,
    body: ByteFixture,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemWritePathFixture {
    mount: String,
    path: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemWriteRenameFixture {
    mount: String,
    source: String,
    destination: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemWriteStreamFixture {
    mount: String,
    path: String,
    expected_bytes: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesystemWriteChunkFixture {
    body: ByteFixture,
    result: FilesystemWriteChunkResultFixture,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum FilesystemWriteChunkResultFixture {
    Success,
    Error { code: BrokerErrorFixture },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum FilesystemWriteResultFixture {
    Mutation {
        bytes_written: u64,
        resulting_size: Option<u64>,
    },
    Error {
        code: BrokerErrorFixture,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalConnectionFixture {
    id: String,
    request: LocalConnectFixtureRequest,
    #[serde(default)]
    open_error: Option<BrokerErrorFixture>,
    #[serde(default)]
    initial_events: Vec<LocalPeerEventFixture>,
    #[serde(default)]
    exchanges: Vec<LocalExchangeFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalConnectFixtureRequest {
    endpoint: String,
    protocol: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalExchangeFixture {
    send: ByteFixture,
    result: LocalSendResultFixture,
    #[serde(default)]
    events: Vec<LocalPeerEventFixture>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum LocalSendResultFixture {
    Success,
    Error { code: BrokerErrorFixture },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum LocalPeerEventFixture {
    Frame { body: ByteFixture },
    Closed,
    Error { code: BrokerErrorFixture },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbusCallFixture {
    id: String,
    request: DbusCallFixtureRequest,
    responses: Vec<DbusResultFixture>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbusSubscriptionFixture {
    id: String,
    request: DbusSubscriptionFixtureRequest,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbusCallFixtureRequest {
    bus: DbusBusFixture,
    destination: String,
    path: String,
    interface: String,
    member: String,
    #[serde(default)]
    arguments: Vec<String>,
    reply: DbusReplyFixtureKind,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbusSubscriptionFixtureRequest {
    bus: DbusBusFixture,
    sender: String,
    path: String,
    interface: String,
    member: String,
    signature: String,
    argument_zero: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DbusBusFixture {
    Session,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum DbusReplyFixtureKind {
    Unit,
    VariantString,
    VariantI64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum DbusResultFixture {
    Unit,
    String { value: String },
    I64 { value: i64 },
    Error { code: BrokerErrorFixture },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum BrokerErrorFixture {
    Unavailable,
    Denied,
    OutOfScope,
    InvalidRequest,
    InvalidPhase,
    ActivationRequired,
    QuotaExceeded,
    RateLimited,
    Timeout,
    Cancelled,
    Unsupported,
    BackendFailed,
    Internal,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbusSignalFixture {
    interface_name: String,
    #[serde(default)]
    changed_properties: Vec<DbusPropertyFixture>,
    #[serde(default)]
    invalidated_properties: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DbusPropertyFixture {
    name: String,
    value: DbusValueFixture,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
enum DbusValueFixture {
    U8(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ContextFixtureValue {
    Text(String),
    Boolean(bool),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AppearanceFixture {
    preset: Option<PalettePreset>,
    revision: Option<u64>,
    motion: Option<MotionFixture>,
    colors: PaletteFixture,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PalettePreset {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum MotionFixture {
    Full,
    Reduced,
    Disabled,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PaletteFixture {
    background: Option<String>,
    control: Option<String>,
    control_pressed: Option<String>,
    accent: Option<String>,
    track: Option<String>,
    foreground: Option<String>,
    muted: Option<String>,
    destructive: Option<String>,
    corner_radius: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Step {
    Touch {
        contact_id: u32,
        phase: ContactFixturePhase,
        x: f32,
        y: f32,
        time_ms: u64,
    },
    Advance {
        time_ms: u64,
    },
    Appearance {
        #[serde(flatten)]
        appearance: AppearanceFixture,
    },
    Presentation {
        event: PresentationFixtureEvent,
    },
    Context {
        facts: BTreeMap<String, ContextFixtureValue>,
    },
    DbusSignal {
        subscription: String,
        event: DbusSignalFixture,
    },
    Snapshot {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ContactFixturePhase {
    Down,
    Motion,
    Up,
    Cancel,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum PresentationFixtureEvent {
    Anchor {
        x: f32,
        width: f32,
    },
    Started,
    Ended {
        reason: PresentationFixtureEndReason,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PresentationFixtureEndReason {
    Requested,
    Selection,
    OutsidePress,
    Timeout,
    SourceHidden,
    Replaced,
    Rejected,
}

#[derive(Serialize)]
struct ReplayReport {
    report_version: u32,
    ok: bool,
    item: String,
    width: u32,
    steps: usize,
    guest_render_calls: u64,
    screenshot_renderer: Option<String>,
    events: Vec<EventReport>,
    presentation_commands: Vec<PresentationCommandReport>,
    broker: Option<BrokerReport>,
    snapshots: Vec<SnapshotReport>,
}

#[derive(Clone, Debug, Serialize)]
struct BrokerReport {
    requests: Vec<BrokerRequestReport>,
    resource_events: Vec<BrokerResourceEventReport>,
}

#[derive(Clone, Debug, Serialize)]
struct BrokerRequestReport {
    fixture: String,
    capability: &'static str,
    operation: &'static str,
    result: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct BrokerResourceEventReport {
    fixture: String,
    sequence: u64,
    kind: &'static str,
}

#[derive(Serialize)]
struct EventReport {
    step: usize,
    time_ms: u64,
    contact_id: u32,
    widget_id: u64,
    kind: &'static str,
    value: Option<f32>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum PresentationCommandReport {
    Begin {
        step: usize,
        placement: &'static str,
        target: Option<String>,
        lifecycle: &'static str,
    },
    End {
        step: usize,
        reason: &'static str,
    },
}

#[derive(Serialize)]
struct SnapshotReport {
    name: String,
    step: Option<usize>,
    time_ms: u64,
    screenshot: Option<String>,
    appearance: AppearanceReport,
    primitive_count: usize,
    primitive_kinds: BTreeMap<&'static str, usize>,
    representations: BTreeMap<String, &'static str>,
    semantics: SemanticReport,
}

#[derive(Serialize)]
struct AppearanceReport {
    revision: u64,
    scheme: &'static str,
    motion: &'static str,
    background: String,
    control: String,
    control_pressed: String,
    accent: String,
    track: String,
    foreground: String,
    muted: String,
    destructive: String,
    corner_radius: f32,
}

#[derive(Serialize)]
struct SemanticReport {
    widget_id: Option<u64>,
    role: &'static str,
    label: String,
    value: Option<String>,
    hint: Option<String>,
    bounds: [f32; 4],
    enabled: bool,
    selected: bool,
    children: Vec<SemanticReport>,
}

struct Harness {
    host: PluginHost,
    item: String,
    width: u32,
    viewport: Rect,
    appearance: Appearance,
    now: Duration,
    ui: touchbar_ui::RetainedUi,
    interactions: InteractionState,
    interaction_map: InteractionMap,
    events: Vec<EventReport>,
    presentation_commands: Vec<PresentationCommandReport>,
    snapshots: Vec<SnapshotReport>,
    snapshot_names: BTreeSet<String>,
    broker: Option<ReplayBrokerController>,
    screenshot_directory: Option<PathBuf>,
    rasterizer: Option<crate::replay_gpu::Rasterizer>,
}

pub struct ReplayBrokerController {
    endpoint: Arc<Seqpacket>,
    state: Arc<Mutex<ReplayBrokerState>>,
}

struct ReplayBrokerState {
    identity: ConnectionIdentity,
    permissions: BTreeMap<CapabilityId, CapabilityRequest>,
    granted: BTreeSet<CapabilityId>,
    activations: ActivationLedger,
    generation: u64,
    facts: BTreeMap<String, ContextFactValue>,
    allowed_context: BTreeSet<String>,
    next_resource_id: u64,
    context_subscriptions: BTreeMap<u64, ContextSubscription>,
    clipboard_requests: Vec<PreparedClipboardRequest>,
    clipboard_bindings: GrantBindings,
    clipboard_operation_times: VecDeque<u64>,
    dbus_calls: Vec<PreparedDbusCall>,
    dbus_subscriptions: Vec<PreparedDbusSubscription>,
    dbus_resources: BTreeMap<u64, usize>,
    command_runs: Vec<PreparedCommandRun>,
    filesystem_reads: Vec<PreparedFilesystemRead>,
    filesystem_bindings: GrantBindings,
    filesystem_writes: Vec<PreparedFilesystemWrite>,
    filesystem_write_bindings: GrantBindings,
    filesystem_write_resources: BTreeMap<u64, FilesystemWriteReplayResource>,
    filesystem_write_quota: VecDeque<(u64, u64)>,
    http_requests: Vec<PreparedHttpRequest>,
    local_connections: Vec<PreparedLocalConnection>,
    local_bindings: GrantBindings,
    local_resources: BTreeMap<u64, LocalReplayResource>,
    notification_requests: Vec<PreparedNotificationRequest>,
    notification_send_times: VecDeque<u64>,
    secret_reads: Vec<PreparedSecretRead>,
    secret_bindings: GrantBindings,
    uri_opens: Vec<PreparedUriOpen>,
    replay_time_micros: u64,
    http_validator: HttpRequestBackend<RejectingHttpResolver, RejectingHttpTransport>,
    pending_events: VecDeque<SupervisorMessage>,
    violations: Vec<String>,
    request_report: Vec<BrokerRequestReport>,
    resource_event_report: Vec<BrokerResourceEventReport>,
}

struct PreparedCommandRun {
    id: String,
    request: CommandRunRequest,
    responses: VecDeque<PreparedCommandResult>,
}

struct PreparedClipboardRequest {
    id: String,
    operation: ClipboardOperationFixture,
    request: ClipboardWireRequest,
    responses: VecDeque<BrokerResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ClipboardWireRequest {
    Read(ClipboardReadRequest),
    Write(ClipboardWriteRequest),
}

enum PreparedCommandResult {
    Inline(BrokerResult),
    Stream {
        events: Vec<PreparedCommandEvent>,
        terminal: BrokerResult,
        terminal_kind: &'static str,
    },
}

struct PreparedCommandEvent {
    payload: Vec<u8>,
    kind: &'static str,
}

struct PreparedFilesystemRead {
    id: String,
    operation: FilesystemReadOperationFixture,
    request: FilesystemReadWireRequest,
    responses: VecDeque<PreparedFilesystemReadResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FilesystemReadWireRequest {
    ReadFile(FilesystemReadFile),
    ReadFileStream(FilesystemReadStream),
    ListDirectory(FilesystemListDirectory),
}

enum PreparedFilesystemReadResult {
    Inline(BrokerResult),
    Stream {
        events: Vec<PreparedFilesystemEvent>,
        terminal: BrokerResult,
        terminal_kind: &'static str,
    },
}

struct PreparedFilesystemEvent {
    payload: Vec<u8>,
    kind: &'static str,
}

struct PreparedFilesystemWrite {
    id: String,
    operation: FilesystemWriteOperationFixture,
    request: FilesystemWriteWireRequest,
    responses: VecDeque<BrokerResult>,
    open_error: Option<BrokerErrorCode>,
    chunks: VecDeque<PreparedFilesystemWriteChunk>,
    commits: VecDeque<BrokerResult>,
    consumed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FilesystemWriteWireRequest {
    Write(FilesystemWriteFile),
    Path(FilesystemPath),
    Rename(FilesystemRename),
    Stream(FilesystemWriteStream),
}

struct PreparedFilesystemWriteChunk {
    bytes: Vec<u8>,
    result: BrokerResult,
}

#[derive(Clone)]
struct FilesystemWriteReplayResource {
    fixture_index: usize,
    expected_bytes: u64,
    received_bytes: u64,
    scope: FilesystemWriteScope,
    filesystem_mounts: BTreeMap<String, FilesystemMountBinding>,
    sequence: u64,
}

struct PreparedLocalConnection {
    id: String,
    request: LocalConnect,
    open_error: Option<BrokerErrorCode>,
    initial_events: Vec<PreparedLocalEvent>,
    exchanges: VecDeque<PreparedLocalExchange>,
    consumed: bool,
}

struct PreparedLocalExchange {
    send: Vec<u8>,
    result: BrokerResult,
    events: Vec<PreparedLocalEvent>,
}

struct PreparedLocalEvent {
    result: BrokerResult,
    kind: &'static str,
    terminal: bool,
}

struct LocalReplayResource {
    fixture_index: usize,
    authorization: LocalConnectionAuthorization,
    sequence: u64,
}

struct PreparedDbusCall {
    id: String,
    request: DbusCall,
    responses: VecDeque<BrokerResult>,
}

struct PreparedDbusSubscription {
    id: String,
    request: DbusSubscription,
    resource_id: Option<u64>,
    sequence: u64,
}

struct PreparedHttpRequest {
    id: String,
    operation: HttpOperationFixture,
    request: HttpRequest,
    responses: VecDeque<PreparedHttpResult>,
}

struct PreparedNotificationRequest {
    id: String,
    operation: NotificationOperationFixture,
    request: NotificationWireRequest,
    responses: VecDeque<BrokerResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NotificationWireRequest {
    Send(NotificationSend),
    Remove(NotificationRemove),
}

struct PreparedUriOpen {
    id: String,
    request: UriOpenRequest,
    responses: VecDeque<BrokerResult>,
}

struct PreparedSecretRead {
    id: String,
    request: SecretReadRequest,
    responses: VecDeque<BrokerResult>,
}

enum PreparedHttpResult {
    Inline(BrokerResult),
    Stream {
        metadata: Vec<u8>,
        chunks: Vec<Vec<u8>>,
        terminal: BrokerResult,
    },
}

struct ContextSubscription {
    requested: BTreeSet<String>,
    sequence: u64,
}

impl Scenario {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = read_scenario(path)?;
        serde_json::from_slice(&bytes).context("parse replay scenario")
    }

    pub fn replay_broker(
        &self,
        manifest: &touchbar_package::PluginManifest,
    ) -> Result<Option<(BrokerClient, ReplayBrokerController)>> {
        let uses_context = !self.context.is_empty()
            || self
                .steps
                .iter()
                .any(|step| matches!(step, Step::Context { .. }));
        let uses_dbus = !self.broker.dbus_calls.is_empty()
            || !self.broker.dbus_subscriptions.is_empty()
            || self
                .steps
                .iter()
                .any(|step| matches!(step, Step::DbusSignal { .. }));
        let uses_http = !self.broker.http_requests.is_empty();
        let uses_clipboard = !self.broker.clipboard_requests.is_empty();
        let uses_commands = !self.broker.command_runs.is_empty();
        let uses_filesystem_read = !self.broker.filesystem_reads.is_empty();
        let uses_filesystem_write = !self.broker.filesystem_writes.is_empty();
        let uses_local = !self.broker.local_connections.is_empty();
        let uses_notifications = !self.broker.notification_requests.is_empty();
        let uses_secrets = !self.broker.secret_reads.is_empty();
        let uses_uri_open = !self.broker.uri_opens.is_empty();
        if !uses_context
            && !uses_dbus
            && !uses_http
            && !uses_clipboard
            && !uses_commands
            && !uses_filesystem_read
            && !uses_filesystem_write
            && !uses_local
            && !uses_notifications
            && !uses_secrets
            && !uses_uri_open
        {
            return Ok(None);
        }

        let permissions = CapabilityRegistry::default()
            .normalize(manifest)
            .map_err(|errors| {
                anyhow::anyhow!(
                    "normalize replay permissions: {}",
                    errors
                        .into_iter()
                        .map(|error| error.to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })?
            .into_iter()
            .map(|request| (request.capability.clone(), request))
            .collect::<BTreeMap<_, _>>();
        let identity = ConnectionIdentity {
            instance_id: 1,
            package: PackageInstance {
                source: manifest.plugin.source.clone(),
                version: manifest.plugin.version.clone(),
                digest: "0".repeat(64),
                provenance: Provenance::LocalDevelopment,
                runtime: RuntimeKind::Component,
            },
        };
        let allowed_context = if uses_context {
            let permission = permissions
                .get(&CapabilityId::ContextReadV1)
                .context("replay context requires a declared context.read.v1 permission")?;
            let CapabilityScope::ContextRead(scope) = &permission.scope else {
                bail!("context.read.v1 permission has an inconsistent normalized scope");
            };
            scope.facts.clone()
        } else {
            BTreeSet::new()
        };
        validate_context_facts(&self.context, &allowed_context)?;
        for step in &self.steps {
            if let Step::Context { facts } = step {
                validate_context_facts(facts, &allowed_context)?;
            }
        }

        let mut fixture_ids = BTreeSet::new();
        let clipboard_bindings = uses_clipboard
            .then(replay_clipboard_bindings)
            .unwrap_or_default();
        let mut clipboard_requests = Vec::with_capacity(self.broker.clipboard_requests.len());
        for fixture in &self.broker.clipboard_requests {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "clipboard fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let capability = clipboard_capability(fixture.operation);
            let permission = permissions.get(&capability).with_context(|| {
                format!(
                    "clipboard fixtures require a declared {} permission",
                    capability
                )
            })?;
            let request = clipboard_request(fixture.operation, &fixture.request)?;
            let authorization = validate_clipboard_authority(
                &identity,
                permission,
                &clipboard_bindings,
                fixture.operation,
                &request,
            )?;
            clipboard_requests.push(PreparedClipboardRequest {
                id: fixture.id.clone(),
                operation: fixture.operation,
                request,
                responses: fixture
                    .responses
                    .iter()
                    .map(|response| {
                        clipboard_result(
                            response,
                            fixture.operation,
                            &fixture.request,
                            authorization.maximum_bytes(),
                        )
                    })
                    .collect::<Result<VecDeque<_>>>()?,
            });
        }
        let secret_bindings = if uses_secrets {
            let permission = permissions
                .get(&CapabilityId::SecretReadV1)
                .context("secret fixtures require a declared secret.read.v1 permission")?;
            replay_secret_bindings(permission)?
        } else {
            GrantBindings::default()
        };
        let mut secret_reads = Vec::with_capacity(self.broker.secret_reads.len());
        for fixture in &self.broker.secret_reads {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "secret fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let permission = permissions
                .get(&CapabilityId::SecretReadV1)
                .context("secret fixtures require a declared secret.read.v1 permission")?;
            let request = SecretReadRequest {
                logical_name: fixture.request.logical_name.clone(),
            };
            validate_secret_authority(&identity, permission, &secret_bindings, &request)?;
            secret_reads.push(PreparedSecretRead {
                id: fixture.id.clone(),
                request,
                responses: fixture
                    .responses
                    .iter()
                    .map(secret_result)
                    .collect::<Result<VecDeque<_>>>()?,
            });
        }
        let local_bindings = if uses_local {
            let permission = permissions
                .get(&CapabilityId::LocalConnectV1)
                .context("local fixtures require a declared local.connect.v1 permission")?;
            replay_local_bindings(permission)?
        } else {
            GrantBindings::default()
        };
        let mut local_connections = Vec::with_capacity(self.broker.local_connections.len());
        for fixture in &self.broker.local_connections {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            let permission = permissions
                .get(&CapabilityId::LocalConnectV1)
                .context("local fixtures require a declared local.connect.v1 permission")?;
            local_connections.push(prepare_local_connection(
                fixture,
                &identity,
                permission,
                &local_bindings,
            )?);
        }
        let filesystem_bindings = if uses_filesystem_read {
            let permission = permissions
                .get(&CapabilityId::FilesystemReadV1)
                .context("filesystem fixtures require a declared filesystem.read.v1 permission")?;
            replay_filesystem_bindings(permission)?
        } else {
            GrantBindings::default()
        };
        let mut filesystem_reads = Vec::with_capacity(self.broker.filesystem_reads.len());
        for fixture in &self.broker.filesystem_reads {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "filesystem fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let permission = permissions
                .get(&CapabilityId::FilesystemReadV1)
                .context("filesystem fixtures require a declared filesystem.read.v1 permission")?;
            let request = filesystem_read_request(fixture.operation, &fixture.request)?;
            validate_filesystem_read_authority(
                &identity,
                permission,
                &filesystem_bindings,
                &request,
            )?;
            let responses = fixture
                .responses
                .iter()
                .map(|response| filesystem_read_result(response, &request, permission))
                .collect::<Result<VecDeque<_>>>()?;
            filesystem_reads.push(PreparedFilesystemRead {
                id: fixture.id.clone(),
                operation: fixture.operation,
                request,
                responses,
            });
        }
        let filesystem_write_bindings = if uses_filesystem_write {
            let permission = permissions.get(&CapabilityId::FilesystemWriteV1).context(
                "filesystem-write fixtures require a declared filesystem.write.v1 permission",
            )?;
            replay_filesystem_write_bindings(permission)?
        } else {
            GrantBindings::default()
        };
        let mut filesystem_writes = Vec::with_capacity(self.broker.filesystem_writes.len());
        for fixture in &self.broker.filesystem_writes {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            let permission = permissions.get(&CapabilityId::FilesystemWriteV1).context(
                "filesystem-write fixtures require a declared filesystem.write.v1 permission",
            )?;
            filesystem_writes.push(prepare_filesystem_write(
                fixture,
                &identity,
                permission,
                &filesystem_write_bindings,
            )?);
        }
        let mut command_runs = Vec::with_capacity(self.broker.command_runs.len());
        for fixture in &self.broker.command_runs {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "command fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let permission = permissions
                .get(&CapabilityId::CommandRunV1)
                .context("command fixtures require a declared command.run.v1 permission")?;
            let request = command_request(&fixture.request);
            validate_command_authority(&identity, permission, &request)?;
            let maximum_output_bytes = command_output_limit(permission, &request.command_id)?;
            let responses = fixture
                .responses
                .iter()
                .map(|response| command_result(response, maximum_output_bytes))
                .collect::<Result<VecDeque<_>>>()?;
            command_runs.push(PreparedCommandRun {
                id: fixture.id.clone(),
                request,
                responses,
            });
        }
        let mut http_requests = Vec::with_capacity(self.broker.http_requests.len());
        for fixture in &self.broker.http_requests {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "HTTP request fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let permission = permissions
                .get(&CapabilityId::HttpRequestV1)
                .context("HTTP fixtures require a declared http.request.v1 permission")?;
            let CapabilityScope::HttpRequest(scope) = &permission.scope else {
                bail!("http.request.v1 permission has an inconsistent normalized scope");
            };
            let request = http_request(&fixture.request);
            validate_http_authority(&identity, permission, fixture.operation, &request)?;
            let responses = fixture
                .responses
                .iter()
                .map(|response| {
                    http_result(
                        response,
                        fixture.operation,
                        &request.url,
                        scope.maximum_response_bytes,
                    )
                })
                .collect::<Result<VecDeque<_>>>()?;
            http_requests.push(PreparedHttpRequest {
                id: fixture.id.clone(),
                operation: fixture.operation,
                request,
                responses,
            });
        }

        let mut notification_requests = Vec::with_capacity(self.broker.notification_requests.len());
        for fixture in &self.broker.notification_requests {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "notification fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let permission = permissions.get(&CapabilityId::NotificationSendV1).context(
                "notification fixtures require a declared notification.send.v1 permission",
            )?;
            let request = notification_request(fixture.operation, &fixture.request)?;
            validate_notification_authority(&identity, permission, fixture.operation, &request)?;
            notification_requests.push(PreparedNotificationRequest {
                id: fixture.id.clone(),
                operation: fixture.operation,
                request,
                responses: fixture.responses.iter().map(desktop_result).collect(),
            });
        }

        let mut uri_opens = Vec::with_capacity(self.broker.uri_opens.len());
        for fixture in &self.broker.uri_opens {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "URI-open fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let permission = permissions
                .get(&CapabilityId::UriOpenV1)
                .context("URI-open fixtures require a declared uri.open.v1 permission")?;
            let request = UriOpenRequest {
                uri: fixture.request.uri.clone(),
            };
            validate_uri_open_authority(&identity, permission, &request)?;
            uri_opens.push(PreparedUriOpen {
                id: fixture.id.clone(),
                request,
                responses: fixture.responses.iter().map(desktop_result).collect(),
            });
        }

        let mut dbus_calls = Vec::with_capacity(self.broker.dbus_calls.len());
        for fixture in &self.broker.dbus_calls {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            if fixture.responses.is_empty() {
                bail!(
                    "D-Bus call fixture `{}` requires at least one response",
                    fixture.id
                );
            }
            let permission = permissions
                .get(&CapabilityId::DbusCallV1)
                .context("D-Bus call fixtures require a declared dbus.call.v1 permission")?;
            let request = dbus_call(&fixture.request);
            validate_dbus_call_authority(&identity, permission, &request)?;
            let responses = fixture
                .responses
                .iter()
                .map(|response| dbus_result(response, request.reply))
                .collect::<Result<VecDeque<_>>>()?;
            dbus_calls.push(PreparedDbusCall {
                id: fixture.id.clone(),
                request,
                responses,
            });
        }
        let mut dbus_subscriptions = Vec::with_capacity(self.broker.dbus_subscriptions.len());
        for fixture in &self.broker.dbus_subscriptions {
            validate_fixture_id(&fixture.id, &mut fixture_ids)?;
            let permission = permissions.get(&CapabilityId::DbusSubscribeV1).context(
                "D-Bus subscription fixtures require a declared dbus.subscribe.v1 permission",
            )?;
            let request = dbus_subscription(&fixture.request);
            validate_dbus_subscription_authority(&identity, permission, &request)?;
            dbus_subscriptions.push(PreparedDbusSubscription {
                id: fixture.id.clone(),
                request,
                resource_id: None,
                sequence: 0,
            });
        }
        for step in &self.steps {
            if let Step::DbusSignal { subscription, .. } = step
                && !dbus_subscriptions
                    .iter()
                    .any(|fixture| fixture.id == *subscription)
            {
                bail!("D-Bus signal references unknown fixture `{subscription}`");
            }
        }

        let facts = self
            .context
            .iter()
            .map(|(key, value)| (key.clone(), context_value(value)))
            .collect();
        let mut granted = BTreeSet::new();
        if uses_context {
            granted.insert(CapabilityId::ContextReadV1);
        }
        if !dbus_calls.is_empty() {
            granted.insert(CapabilityId::DbusCallV1);
        }
        if !dbus_subscriptions.is_empty() {
            granted.insert(CapabilityId::DbusSubscribeV1);
        }
        if !http_requests.is_empty() {
            granted.insert(CapabilityId::HttpRequestV1);
        }
        if !command_runs.is_empty() {
            granted.insert(CapabilityId::CommandRunV1);
        }
        if !filesystem_reads.is_empty() {
            granted.insert(CapabilityId::FilesystemReadV1);
        }
        if !filesystem_writes.is_empty() {
            granted.insert(CapabilityId::FilesystemWriteV1);
        }
        if !local_connections.is_empty() {
            granted.insert(CapabilityId::LocalConnectV1);
        }
        for fixture in &clipboard_requests {
            granted.insert(clipboard_capability(fixture.operation));
        }
        if !notification_requests.is_empty() {
            granted.insert(CapabilityId::NotificationSendV1);
        }
        if !secret_reads.is_empty() {
            granted.insert(CapabilityId::SecretReadV1);
        }
        if !uri_opens.is_empty() {
            granted.insert(CapabilityId::UriOpenV1);
        }
        let state = Arc::new(Mutex::new(ReplayBrokerState {
            identity,
            permissions,
            granted,
            activations: ActivationLedger::new(MAX_STEPS),
            generation: 1,
            facts,
            allowed_context,
            next_resource_id: 1,
            context_subscriptions: BTreeMap::new(),
            clipboard_requests,
            clipboard_bindings,
            clipboard_operation_times: VecDeque::new(),
            dbus_calls,
            dbus_subscriptions,
            dbus_resources: BTreeMap::new(),
            command_runs,
            filesystem_reads,
            filesystem_bindings,
            filesystem_writes,
            filesystem_write_bindings,
            filesystem_write_resources: BTreeMap::new(),
            filesystem_write_quota: VecDeque::new(),
            http_requests,
            local_connections,
            local_bindings,
            local_resources: BTreeMap::new(),
            notification_requests,
            notification_send_times: VecDeque::new(),
            secret_reads,
            secret_bindings,
            uri_opens,
            replay_time_micros: 0,
            http_validator: HttpRequestBackend::with_parts(
                RejectingHttpResolver,
                RejectingHttpTransport,
            ),
            pending_events: VecDeque::new(),
            violations: Vec::new(),
            request_report: Vec::new(),
            resource_event_report: Vec::new(),
        }));
        let (client_endpoint, supervisor_endpoint) = Seqpacket::pair()?;
        let endpoint = Arc::new(supervisor_endpoint);
        let worker_endpoint = Arc::clone(&endpoint);
        let worker_state = Arc::clone(&state);
        thread::Builder::new()
            .name("touchbar-replay-broker".into())
            .spawn(move || run_replay_broker(worker_endpoint, worker_state))
            .context("start replay broker")?;
        let client = BrokerClient::connect(client_endpoint)?;
        Ok(Some((client, ReplayBrokerController { endpoint, state })))
    }
}

impl ReplayBrokerController {
    fn set_time(&self, now: Duration) -> Result<()> {
        let micros = u64::try_from(now.as_micros()).context("replay clock exceeds u64")?;
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("replay broker state is poisoned"))?
            .replay_time_micros = micros;
        Ok(())
    }

    fn publish_context(&self, facts: &BTreeMap<String, ContextFixtureValue>) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("replay broker state is poisoned"))?;
        validate_context_facts(facts, &state.allowed_context)?;
        for (key, value) in facts {
            state.facts.insert(key.clone(), context_value(value));
        }
        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        let all_facts = state.facts.clone();
        let mut events = Vec::with_capacity(state.context_subscriptions.len());
        for (resource_id, subscription) in &mut state.context_subscriptions {
            subscription.sequence = subscription.sequence.saturating_add(1);
            let snapshot = context_snapshot(generation, &all_facts, &subscription.requested);
            events.push((
                *resource_id,
                subscription.sequence,
                snapshot.encode().context("encode replay context update")?,
            ));
        }
        drop(state);
        for (resource_id, sequence, payload) in events {
            self.endpoint
                .send_supervisor(&SupervisorMessage::ResourceEvent {
                    resource_id,
                    sequence,
                    result: BrokerResult::Success { payload },
                })?;
        }
        Ok(())
    }

    fn publish_dbus_signal(&self, fixture_id: &str, event: &DbusSignalFixture) -> Result<()> {
        let (resource_id, sequence, payload) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("replay broker state is poisoned"))?;
            let fixture_index = state
                .dbus_subscriptions
                .iter()
                .position(|fixture| fixture.id == fixture_id)
                .with_context(|| format!("unknown D-Bus subscription fixture `{fixture_id}`"))?;
            let resource_id = state.dbus_subscriptions[fixture_index]
                .resource_id
                .with_context(|| format!("D-Bus subscription `{fixture_id}` is not open"))?;
            if !state.dbus_resources.contains_key(&resource_id) {
                bail!("D-Bus subscription `{fixture_id}` has already been closed");
            }
            let fixture = &mut state.dbus_subscriptions[fixture_index];
            if fixture.request.argument_zero.as_deref() != Some(event.interface_name.as_str()) {
                bail!(
                    "D-Bus signal `{fixture_id}` interface does not match the subscription argument_zero"
                );
            }
            fixture.sequence = fixture.sequence.saturating_add(1);
            let sequence = fixture.sequence;
            let payload = dbus_signal(event)
                .encode()
                .with_context(|| format!("encode D-Bus signal fixture `{fixture_id}`"))?;
            state.resource_event_report.push(BrokerResourceEventReport {
                fixture: fixture_id.to_owned(),
                sequence,
                kind: "dbus-properties-changed",
            });
            (resource_id, sequence, payload)
        };
        self.endpoint
            .send_supervisor(&SupervisorMessage::ResourceEvent {
                resource_id,
                sequence,
                result: BrokerResult::Success { payload },
            })?;
        Ok(())
    }

    fn finish(&self) -> Result<BrokerReport> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("replay broker state is poisoned"))?;
        if !state.violations.is_empty() {
            bail!("replay broker violations: {}", state.violations.join("; "));
        }
        let unused_clipboard = state
            .clipboard_requests
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_clipboard.is_empty() {
            bail!(
                "unused clipboard fixture responses: {}",
                unused_clipboard.join(", ")
            );
        }
        let unused_calls = state
            .dbus_calls
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_calls.is_empty() {
            bail!(
                "unused D-Bus call fixture responses: {}",
                unused_calls.join(", ")
            );
        }
        let unused_http = state
            .http_requests
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_http.is_empty() {
            bail!("unused HTTP fixture responses: {}", unused_http.join(", "));
        }
        let unused_commands = state
            .command_runs
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_commands.is_empty() {
            bail!(
                "unused command fixture responses: {}",
                unused_commands.join(", ")
            );
        }
        let unused_filesystem = state
            .filesystem_reads
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_filesystem.is_empty() {
            bail!(
                "unused filesystem fixture responses: {}",
                unused_filesystem.join(", ")
            );
        }
        let unopened_filesystem_writes = state
            .filesystem_writes
            .iter()
            .filter(|fixture| fixture.operation.is_stream() && !fixture.consumed)
            .map(|fixture| fixture.id.as_str())
            .collect::<Vec<_>>();
        if !unopened_filesystem_writes.is_empty() {
            bail!(
                "unopened filesystem-write stream fixtures: {}",
                unopened_filesystem_writes.join(", ")
            );
        }
        let unused_filesystem_writes = state
            .filesystem_writes
            .iter()
            .filter_map(|fixture| {
                let remaining =
                    fixture.responses.len() + fixture.chunks.len() + fixture.commits.len();
                (remaining != 0).then(|| format!("{} ({remaining})", fixture.id))
            })
            .collect::<Vec<_>>();
        if !unused_filesystem_writes.is_empty() {
            bail!(
                "unused filesystem-write fixture interactions: {}",
                unused_filesystem_writes.join(", ")
            );
        }
        if !state.filesystem_write_resources.is_empty() {
            bail!(
                "unclosed filesystem-write resources: {}",
                state
                    .filesystem_write_resources
                    .keys()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let unopened_local = state
            .local_connections
            .iter()
            .filter(|fixture| !fixture.consumed)
            .map(|fixture| fixture.id.as_str())
            .collect::<Vec<_>>();
        if !unopened_local.is_empty() {
            bail!(
                "unopened local connection fixtures: {}",
                unopened_local.join(", ")
            );
        }
        let unused_local = state
            .local_connections
            .iter()
            .filter(|fixture| !fixture.exchanges.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.exchanges.len()))
            .collect::<Vec<_>>();
        if !unused_local.is_empty() {
            bail!(
                "unused local exchange fixtures: {}",
                unused_local.join(", ")
            );
        }
        let unused_notifications = state
            .notification_requests
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_notifications.is_empty() {
            bail!(
                "unused notification fixture responses: {}",
                unused_notifications.join(", ")
            );
        }
        let unused_secrets = state
            .secret_reads
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_secrets.is_empty() {
            bail!(
                "unused secret fixture responses: {}",
                unused_secrets.join(", ")
            );
        }
        let unused_uri_opens = state
            .uri_opens
            .iter()
            .filter(|fixture| !fixture.responses.is_empty())
            .map(|fixture| format!("{} ({})", fixture.id, fixture.responses.len()))
            .collect::<Vec<_>>();
        if !unused_uri_opens.is_empty() {
            bail!(
                "unused URI-open fixture responses: {}",
                unused_uri_opens.join(", ")
            );
        }
        let unopened = state
            .dbus_subscriptions
            .iter()
            .filter(|fixture| fixture.resource_id.is_none())
            .map(|fixture| fixture.id.as_str())
            .collect::<Vec<_>>();
        if !unopened.is_empty() {
            bail!(
                "unopened D-Bus subscription fixtures: {}",
                unopened.join(", ")
            );
        }
        Ok(BrokerReport {
            requests: state.request_report.clone(),
            resource_events: state.resource_event_report.clone(),
        })
    }
}

fn run_replay_broker(endpoint: Arc<Seqpacket>, state: Arc<Mutex<ReplayBrokerState>>) {
    while let Ok(message) = endpoint.recv_host() {
        let response = match message {
            HostMessage::GetCapabilities { request_id } => {
                let states = state.lock().map_or_else(
                    |_| Vec::new(),
                    |state| {
                        state
                            .granted
                            .iter()
                            .filter_map(|capability| {
                                state.permissions.get(capability).map(|permission| {
                                    CapabilityState {
                                        capability: capability.to_string(),
                                        required: permission.required,
                                        status: WireCapabilityStatus::Granted,
                                    }
                                })
                            })
                            .collect()
                    },
                );
                SupervisorMessage::Capabilities {
                    request_id,
                    generation: 1,
                    states,
                }
            }
            HostMessage::Request {
                request_id,
                phase,
                capability,
                operation,
                payload,
                activation,
            } => SupervisorMessage::Response {
                request_id,
                result: replay_request(
                    &state,
                    request_id,
                    phase,
                    &capability,
                    &operation,
                    &payload,
                    activation,
                ),
            },
            HostMessage::Cancel { request_id, .. } => SupervisorMessage::Response {
                request_id,
                result: BrokerResult::Error(BrokerErrorCode::Cancelled),
            },
            HostMessage::Close {
                request_id,
                resource_id,
            } => {
                let result = state
                    .lock()
                    .ok()
                    .and_then(|mut state| {
                        let context = state.context_subscriptions.remove(&resource_id).is_some();
                        let dbus = state.dbus_resources.remove(&resource_id).is_some();
                        let local = state.local_resources.remove(&resource_id).is_some();
                        let filesystem_write = state
                            .filesystem_write_resources
                            .remove(&resource_id)
                            .is_some();
                        (context || dbus || local || filesystem_write).then_some(())
                    })
                    .map_or(BrokerResult::Error(BrokerErrorCode::InvalidRequest), |()| {
                        BrokerResult::Success {
                            payload: Vec::new(),
                        }
                    });
                SupervisorMessage::Response { request_id, result }
            }
        };
        if endpoint.send_supervisor(&response).is_err() {
            return;
        }
        let pending = match state.lock() {
            Ok(mut state) => state.pending_events.drain(..).collect::<Vec<_>>(),
            Err(_) => return,
        };
        for event in pending {
            if endpoint.send_supervisor(&event).is_err() {
                return;
            }
        }
    }
}

fn replay_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    phase: CallbackPhase,
    capability: &str,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    if matches!(phase, CallbackPhase::Items | CallbackPhase::Render) {
        return BrokerResult::Error(BrokerErrorCode::InvalidPhase);
    }
    match capability {
        "clipboard.read.v1" | "clipboard.write.v1" => clipboard_request_message(
            state, request_id, capability, operation, payload, activation,
        ),
        "context.read.v1" => context_request(state, operation, payload),
        "dbus.call.v1" => dbus_call_request(state, request_id, operation, payload, activation),
        "dbus.subscribe.v1" => {
            dbus_subscription_request(state, request_id, operation, payload, activation)
        }
        "http.request.v1" => {
            http_request_request(state, request_id, operation, payload, activation)
        }
        "command.run.v1" => command_run_request(state, request_id, operation, payload, activation),
        "filesystem.read.v1" => {
            filesystem_read_request_message(state, request_id, operation, payload, activation)
        }
        "filesystem.write.v1" => {
            filesystem_write_request_message(state, request_id, operation, payload, activation)
        }
        "local.connect.v1" => {
            local_connect_request(state, request_id, operation, payload, activation)
        }
        "notification.send.v1" => {
            notification_request_message(state, request_id, operation, payload, activation)
        }
        "secret.read.v1" => secret_read_request(state, request_id, operation, payload, activation),
        "uri.open.v1" => uri_open_request(state, request_id, operation, payload, activation),
        _ => BrokerResult::Error(BrokerErrorCode::Denied),
    }
}

fn context_request(
    state: &Mutex<ReplayBrokerState>,
    operation: &str,
    payload: &[u8],
) -> BrokerResult {
    let result = (|| {
        let request =
            ContextReadRequest::decode(payload).map_err(|_| BrokerErrorCode::InvalidRequest)?;
        let mut state = state.lock().map_err(|_| BrokerErrorCode::Internal)?;
        if !state.granted.contains(&CapabilityId::ContextReadV1) {
            return Err(BrokerErrorCode::Denied);
        }
        if !request.facts.is_subset(&state.allowed_context) {
            return Err(BrokerErrorCode::OutOfScope);
        }
        let snapshot = context_snapshot(state.generation, &state.facts, &request.facts);
        match operation {
            "snapshot" => snapshot.encode().map_err(|_| BrokerErrorCode::Internal),
            "subscribe" => {
                let resource_id = state.next_resource_id;
                state.next_resource_id = state
                    .next_resource_id
                    .checked_add(1)
                    .ok_or(BrokerErrorCode::Internal)?;
                state.context_subscriptions.insert(
                    resource_id,
                    ContextSubscription {
                        requested: request.facts,
                        sequence: 0,
                    },
                );
                ContextSubscriptionOpened {
                    resource_id,
                    snapshot,
                }
                .encode()
                .map_err(|_| BrokerErrorCode::Internal)
            }
            _ => Err(BrokerErrorCode::InvalidRequest),
        }
    })();
    match result {
        Ok(payload) => BrokerResult::Success { payload },
        Err(error) => BrokerResult::Error(error),
    }
}

fn clipboard_request_message(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    capability_name: &str,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let expected_operation = match (capability_name, operation) {
        ("clipboard.read.v1", CLIPBOARD_READ_OPERATION) => ClipboardOperationFixture::Read,
        ("clipboard.write.v1", CLIPBOARD_WRITE_OPERATION) => ClipboardOperationFixture::Write,
        _ => return BrokerResult::Error(BrokerErrorCode::InvalidRequest),
    };
    let capability = clipboard_capability(expected_operation);
    let Some(permission) = state.permissions.get(&capability).cloned() else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability,
        authorized_scope: permission.scope,
        bindings: state.clipboard_bindings.clone(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let authorization = match authorize_clipboard_request(&request, &mut state.activations, now) {
        Ok(authorization) => authorization,
        Err(error) => {
            state.violations.push(format!(
                "{capability_name} request {request_id} failed production authorization: {error:?}"
            ));
            return BrokerResult::Error(error);
        }
    };
    let replay_time = state.replay_time_micros;
    if let Err(error) = reserve_replay_rate(
        &mut state.clipboard_operation_times,
        replay_time,
        authorization.maximum_operations_per_minute(),
    ) {
        state.violations.push(format!(
            "{capability_name} request {request_id} exceeded the manifest rolling-minute rate"
        ));
        return BrokerResult::Error(error);
    }
    let wire = match decode_clipboard_wire(expected_operation, payload) {
        Ok(wire) => wire,
        Err(_) => {
            state.violations.push(format!(
                "{capability_name} request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    let Some(fixture) = state.clipboard_requests.iter_mut().find(|fixture| {
        fixture.operation == expected_operation
            && fixture.request == wire
            && !fixture.responses.is_empty()
    }) else {
        state.violations.push(format!(
            "{capability_name} request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched clipboard fixture has a response");
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: clipboard_capability_name(expected_operation),
        operation: clipboard_operation(expected_operation),
        result: broker_result_name(&result),
    });
    result
}

fn notification_request_message(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state
        .permissions
        .get(&CapabilityId::NotificationSendV1)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::NotificationSendV1,
        authorized_scope: permission.scope,
        bindings: GrantBindings::default(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let authorization = match authorize_notification_request(&request) {
        Ok(authorization) => authorization,
        Err(error) => {
            state.violations.push(format!(
                "notification.send.v1 request {request_id} failed production authorization: {error:?}"
            ));
            return BrokerResult::Error(error);
        }
    };
    if let NotificationAuthorization::Send { maximum_per_minute } = authorization {
        let now = state.replay_time_micros;
        if let Err(error) =
            reserve_replay_rate(&mut state.notification_send_times, now, maximum_per_minute)
        {
            state.violations.push(format!(
                "notification.send.v1 request {request_id} exceeded the manifest rolling-minute rate"
            ));
            return BrokerResult::Error(error);
        }
    }
    let wire = match decode_notification_wire(operation, payload) {
        Ok(wire) => wire,
        Err(_) => {
            state.violations.push(format!(
                "notification.send.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    let expected_operation = match operation {
        NOTIFICATION_SEND_OPERATION => NotificationOperationFixture::Send,
        NOTIFICATION_REMOVE_OPERATION => NotificationOperationFixture::Remove,
        _ => return BrokerResult::Error(BrokerErrorCode::InvalidRequest),
    };
    let Some(fixture) = state.notification_requests.iter_mut().find(|fixture| {
        fixture.operation == expected_operation
            && fixture.request == wire
            && !fixture.responses.is_empty()
    }) else {
        state.violations.push(format!(
            "notification.send.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched notification fixture has a response");
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: "notification.send.v1",
        operation: notification_operation(expected_operation),
        result: broker_result_name(&result),
    });
    result
}

fn reserve_replay_rate(
    send_times: &mut VecDeque<u64>,
    now_micros: u64,
    maximum_per_minute: u16,
) -> std::result::Result<(), BrokerErrorCode> {
    while send_times
        .front()
        .is_some_and(|time| time.saturating_add(60_000_000) <= now_micros)
    {
        send_times.pop_front();
    }
    if send_times.len() >= usize::from(maximum_per_minute) {
        return Err(BrokerErrorCode::RateLimited);
    }
    send_times.push_back(now_micros);
    Ok(())
}

fn reserve_replay_byte_quota(
    reservations: &mut VecDeque<(u64, u64)>,
    now_micros: u64,
    bytes: u64,
    maximum_per_hour: u64,
) -> std::result::Result<(), BrokerErrorCode> {
    while reservations
        .front()
        .is_some_and(|(time, _)| time.saturating_add(3_600_000_000) <= now_micros)
    {
        reservations.pop_front();
    }
    let used = reservations.iter().try_fold(0_u64, |total, (_, bytes)| {
        total
            .checked_add(*bytes)
            .ok_or(BrokerErrorCode::QuotaExceeded)
    })?;
    if used
        .checked_add(bytes)
        .ok_or(BrokerErrorCode::QuotaExceeded)?
        > maximum_per_hour
    {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    if bytes != 0 {
        reservations.push_back((now_micros, bytes));
    }
    Ok(())
}

fn secret_read_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state.permissions.get(&CapabilityId::SecretReadV1).cloned() else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::SecretReadV1,
        authorized_scope: permission.scope,
        bindings: state.secret_bindings.clone(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    if let Err(error) = authorize_secret_read_request(&request, &mut state.activations, now) {
        state.violations.push(format!(
            "secret.read.v1 request {request_id} failed production authorization: {error:?}"
        ));
        return BrokerResult::Error(error);
    }
    let wire = match SecretReadRequest::decode(payload) {
        Ok(wire) => wire,
        Err(_) => {
            state.violations.push(format!(
                "secret.read.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    if operation != SECRET_READ_OPERATION {
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    }
    let Some(fixture) = state
        .secret_reads
        .iter_mut()
        .find(|fixture| fixture.request == wire && !fixture.responses.is_empty())
    else {
        state.violations.push(format!(
            "secret.read.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched secret fixture has a response");
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: "secret.read.v1",
        operation: SECRET_READ_OPERATION,
        result: broker_result_name(&result),
    });
    result
}

fn uri_open_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state.permissions.get(&CapabilityId::UriOpenV1).cloned() else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::UriOpenV1,
        authorized_scope: permission.scope,
        bindings: GrantBindings::default(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    if let Err(error) =
        UriOpenBackend::new(RejectingUriTransport).authorize(&request, &mut state.activations, now)
    {
        state.violations.push(format!(
            "uri.open.v1 request {request_id} failed production authorization: {error:?}"
        ));
        return BrokerResult::Error(error);
    }
    let wire = match UriOpenRequest::decode(payload) {
        Ok(wire) => wire,
        Err(_) => {
            state.violations.push(format!(
                "uri.open.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    if operation != URI_OPEN_OPERATION {
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    }
    let Some(fixture) = state
        .uri_opens
        .iter_mut()
        .find(|fixture| fixture.request == wire && !fixture.responses.is_empty())
    else {
        state.violations.push(format!(
            "uri.open.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched URI-open fixture has a response");
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: "uri.open.v1",
        operation: URI_OPEN_OPERATION,
        result: broker_result_name(&result),
    });
    result
}

fn dbus_call_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state.permissions.get(&CapabilityId::DbusCallV1).cloned() else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::DbusCallV1,
        authorized_scope: permission.scope,
        bindings: GrantBindings::default(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let validator = DbusCallBackend::new(RejectingDbusTransport);
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    if let Err(error) = validator.authorize(&request, &mut state.activations, now) {
        state.violations.push(format!(
            "dbus.call.v1 request {request_id} failed production authorization: {error:?}"
        ));
        return BrokerResult::Error(error);
    }
    let call = match DbusCall::decode(payload) {
        Ok(call) => call,
        Err(_) => {
            state.violations.push(format!(
                "dbus.call.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    let Some(fixture) = state
        .dbus_calls
        .iter_mut()
        .find(|fixture| fixture.request == call && !fixture.responses.is_empty())
    else {
        state.violations.push(format!(
            "dbus.call.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched fixture has a response");
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: "dbus.call.v1",
        operation: "call",
        result: broker_result_name(&result),
    });
    result
}

fn dbus_subscription_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state
        .permissions
        .get(&CapabilityId::DbusSubscribeV1)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::DbusSubscribeV1,
        authorized_scope: permission.scope,
        bindings: GrantBindings::default(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let validator = DbusSubscriptionBackend::new(RejectingDbusTransport);
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    if let Err(error) =
        ResourceBackend::authorize(&validator, &request, &mut state.activations, now)
    {
        state.violations.push(format!(
            "dbus.subscribe.v1 request {request_id} failed production authorization: {error:?}"
        ));
        return BrokerResult::Error(error);
    }
    let subscription = match DbusSubscription::decode(payload) {
        Ok(subscription) => subscription,
        Err(_) => {
            state.violations.push(format!(
                "dbus.subscribe.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    let Some(index) = state
        .dbus_subscriptions
        .iter()
        .position(|fixture| fixture.request == subscription && fixture.resource_id.is_none())
    else {
        state.violations.push(format!(
            "dbus.subscribe.v1 request {request_id} did not match an unopened exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let resource_id = state.next_resource_id;
    state.next_resource_id = match resource_id.checked_add(1) {
        Some(next) => next,
        None => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    state.dbus_subscriptions[index].resource_id = Some(resource_id);
    state.dbus_resources.insert(resource_id, index);
    let fixture_id = state.dbus_subscriptions[index].id.clone();
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: "dbus.subscribe.v1",
        operation: "subscribe",
        result: "opened",
    });
    match (DbusSubscriptionOpened { resource_id }).encode() {
        Ok(payload) => BrokerResult::Success { payload },
        Err(_) => BrokerResult::Error(BrokerErrorCode::Internal),
    }
}

fn local_connect_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    match operation {
        LOCAL_CONNECT_OPERATION => local_open_request(state, request_id, payload, activation),
        LOCAL_SEND_OPERATION => local_send_request(state, request_id, payload, activation),
        _ => BrokerResult::Error(BrokerErrorCode::InvalidRequest),
    }
}

fn local_open_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state
        .permissions
        .get(&CapabilityId::LocalConnectV1)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::LocalConnectV1,
        authorized_scope: permission.scope,
        bindings: state.local_bindings.clone(),
        activation,
        operation: LOCAL_CONNECT_OPERATION.into(),
        payload: payload.to_vec(),
    };
    let (wire, authorization) = match authorize_local_connect_request(&request) {
        Ok(value) => value,
        Err(error) => {
            state.violations.push(format!(
                "local.connect.v1 request {request_id} failed production authorization: {error:?}"
            ));
            return BrokerResult::Error(error);
        }
    };
    let Some(index) = state
        .local_connections
        .iter()
        .position(|fixture| fixture.request == wire && !fixture.consumed)
    else {
        state.violations.push(format!(
            "local.connect.v1 request {request_id} did not match an unopened exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    state.local_connections[index].consumed = true;
    let fixture_id = state.local_connections[index].id.clone();
    if let Some(error) = state.local_connections[index].open_error {
        let result = BrokerResult::Error(error);
        state.request_report.push(BrokerRequestReport {
            fixture: fixture_id,
            capability: "local.connect.v1",
            operation: LOCAL_CONNECT_OPERATION,
            result: broker_result_name(&result),
        });
        return result;
    }
    let resource_id = state.next_resource_id;
    state.next_resource_id = match resource_id.checked_add(1) {
        Some(next) => next,
        None => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let maximum_frame_bytes = authorization.maximum_frame_bytes();
    state.local_resources.insert(
        resource_id,
        LocalReplayResource {
            fixture_index: index,
            authorization,
            sequence: 0,
        },
    );
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id.clone(),
        capability: "local.connect.v1",
        operation: LOCAL_CONNECT_OPERATION,
        result: "opened",
    });
    let events = std::mem::take(&mut state.local_connections[index].initial_events);
    queue_local_events(&mut state, &fixture_id, resource_id, events);
    match (LocalConnectionOpened {
        resource_id,
        maximum_frame_bytes,
    })
    .encode()
    {
        Ok(payload) => BrokerResult::Success { payload },
        Err(_) => BrokerResult::Error(BrokerErrorCode::Internal),
    }
}

fn local_send_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state
        .permissions
        .get(&CapabilityId::LocalConnectV1)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let frame = match LocalSendFrame::decode(payload) {
        Ok(frame) => frame,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::InvalidRequest),
    };
    let Some(resource) = state.local_resources.get(&frame.resource_id) else {
        return BrokerResult::Error(BrokerErrorCode::Unavailable);
    };
    let fixture_index = resource.fixture_index;
    let authorization = resource.authorization.clone();
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::LocalConnectV1,
        authorized_scope: permission.scope,
        bindings: state.local_bindings.clone(),
        activation,
        operation: LOCAL_SEND_OPERATION.into(),
        payload: payload.to_vec(),
    };
    let frame = match authorize_local_send_request(&request, &authorization) {
        Ok(frame) => frame,
        Err(error) => {
            state.violations.push(format!(
                "local.connect.v1 send {request_id} failed production authorization: {error:?}"
            ));
            return BrokerResult::Error(error);
        }
    };
    let fixture = &mut state.local_connections[fixture_index];
    let Some(exchange) = fixture.exchanges.pop_front() else {
        state.violations.push(format!(
            "local.connect.v1 send {request_id} had no remaining exchange fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    if exchange.send != frame.bytes {
        state.violations.push(format!(
            "local.connect.v1 send {request_id} did not match the next exact frame fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    }
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id.clone(),
        capability: "local.connect.v1",
        operation: LOCAL_SEND_OPERATION,
        result: broker_result_name(&exchange.result),
    });
    queue_local_events(&mut state, &fixture_id, frame.resource_id, exchange.events);
    exchange.result
}

fn queue_local_events(
    state: &mut ReplayBrokerState,
    fixture_id: &str,
    resource_id: u64,
    events: Vec<PreparedLocalEvent>,
) {
    for event in events {
        let Some(resource) = state.local_resources.get_mut(&resource_id) else {
            return;
        };
        resource.sequence = resource.sequence.saturating_add(1);
        let sequence = resource.sequence;
        let terminal = event.terminal;
        queue_resource_event(
            state,
            fixture_id,
            resource_id,
            sequence,
            event.kind,
            event.result,
        );
        if terminal {
            state.local_resources.remove(&resource_id);
            return;
        }
    }
}

fn filesystem_read_request_message(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state
        .permissions
        .get(&CapabilityId::FilesystemReadV1)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::FilesystemReadV1,
        authorized_scope: permission.scope,
        bindings: state.filesystem_bindings.clone(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let validator = FilesystemReadBackend::new();
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let authorization = if operation == FILESYSTEM_READ_STREAM_OPERATION {
        ResourceBackend::authorize(&validator, &request, &mut state.activations, now)
    } else {
        Backend::authorize(&validator, &request, &mut state.activations, now)
    };
    if let Err(error) = authorization {
        state.violations.push(format!(
            "filesystem.read.v1 request {request_id} failed production authorization: {error:?}"
        ));
        return BrokerResult::Error(error);
    }
    let wire = match decode_filesystem_read_wire(operation, payload) {
        Ok(wire) => wire,
        Err(_) => {
            state.violations.push(format!(
                "filesystem.read.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    let Some(fixture) = state.filesystem_reads.iter_mut().find(|fixture| {
        filesystem_operation(fixture.operation) == operation
            && fixture.request == wire
            && !fixture.responses.is_empty()
    }) else {
        state.violations.push(format!(
            "filesystem.read.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched filesystem fixture has a response");
    match result {
        PreparedFilesystemReadResult::Inline(result) => {
            state.request_report.push(BrokerRequestReport {
                fixture: fixture_id,
                capability: "filesystem.read.v1",
                operation: filesystem_wire_operation(&wire),
                result: broker_result_name(&result),
            });
            result
        }
        PreparedFilesystemReadResult::Stream {
            events,
            terminal,
            terminal_kind,
        } => {
            let resource_id = state.next_resource_id;
            state.next_resource_id = match resource_id.checked_add(1) {
                Some(next) => next,
                None => return BrokerResult::Error(BrokerErrorCode::Internal),
            };
            state.request_report.push(BrokerRequestReport {
                fixture: fixture_id.clone(),
                capability: "filesystem.read.v1",
                operation: FILESYSTEM_READ_STREAM_OPERATION,
                result: "opened",
            });
            let mut sequence = 0_u64;
            for event in events {
                sequence = sequence.saturating_add(1);
                queue_resource_event(
                    &mut state,
                    &fixture_id,
                    resource_id,
                    sequence,
                    event.kind,
                    BrokerResult::Success {
                        payload: event.payload,
                    },
                );
            }
            sequence = sequence.saturating_add(1);
            queue_resource_event(
                &mut state,
                &fixture_id,
                resource_id,
                sequence,
                terminal_kind,
                terminal,
            );
            match (FilesystemStreamOpened { resource_id }).encode() {
                Ok(payload) => BrokerResult::Success { payload },
                Err(_) => BrokerResult::Error(BrokerErrorCode::Internal),
            }
        }
    }
}

fn filesystem_write_request_message(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    if operation == FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION {
        return filesystem_write_chunk_request(state, request_id, payload, activation);
    }
    if operation == FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION {
        return filesystem_write_commit_request(state, request_id, payload, activation);
    }

    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state
        .permissions
        .get(&CapabilityId::FilesystemWriteV1)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::FilesystemWriteV1,
        authorized_scope: permission.scope.clone(),
        bindings: state.filesystem_write_bindings.clone(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let is_stream = matches!(
        operation,
        FILESYSTEM_CREATE_FILE_STREAM_OPERATION
            | FILESYSTEM_REPLACE_FILE_STREAM_OPERATION
            | FILESYSTEM_APPEND_FILE_STREAM_OPERATION
    );
    let written_bytes = if is_stream {
        match authorize_filesystem_write_stream_begin_request(&request) {
            Ok(stream) => stream.expected_bytes,
            Err(error) => {
                state.violations.push(format!(
                    "filesystem.write.v1 request {request_id} failed production authorization: {error:?}"
                ));
                return BrokerResult::Error(error);
            }
        }
    } else {
        match authorize_filesystem_write_mutation_request(&request) {
            Ok(bytes) => bytes,
            Err(error) => {
                state.violations.push(format!(
                    "filesystem.write.v1 request {request_id} failed production authorization: {error:?}"
                ));
                return BrokerResult::Error(error);
            }
        }
    };
    let CapabilityScope::FilesystemWrite(scope) = &permission.scope else {
        return BrokerResult::Error(BrokerErrorCode::Internal);
    };
    let quota_maximum = scope.maximum_total_bytes_per_hour;
    let now = state.replay_time_micros;
    if let Err(error) = reserve_replay_byte_quota(
        &mut state.filesystem_write_quota,
        now,
        written_bytes,
        quota_maximum,
    ) {
        state.violations.push(format!(
            "filesystem.write.v1 request {request_id} exceeded the production rolling-hour quota"
        ));
        return BrokerResult::Error(error);
    }
    let wire = match decode_filesystem_write_wire(operation, payload) {
        Ok(wire) => wire,
        Err(_) => {
            state.violations.push(format!(
                "filesystem.write.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    let Some(fixture_index) = state.filesystem_writes.iter().position(|fixture| {
        filesystem_write_operation(fixture.operation) == operation
            && fixture.request == wire
            && if is_stream {
                !fixture.consumed
            } else {
                !fixture.responses.is_empty()
            }
    }) else {
        state.violations.push(format!(
            "filesystem.write.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };

    if !is_stream {
        let fixture = &mut state.filesystem_writes[fixture_index];
        let fixture_operation = filesystem_write_operation(fixture.operation);
        let fixture_id = fixture.id.clone();
        let result = fixture
            .responses
            .pop_front()
            .expect("matched filesystem-write fixture has a response");
        state.request_report.push(BrokerRequestReport {
            fixture: fixture_id,
            capability: "filesystem.write.v1",
            operation: fixture_operation,
            result: broker_result_name(&result),
        });
        return result;
    }

    let (fixture_id, fixture_operation, open_error) = {
        let fixture = &mut state.filesystem_writes[fixture_index];
        fixture.consumed = true;
        (
            fixture.id.clone(),
            filesystem_write_operation(fixture.operation),
            fixture.open_error.take(),
        )
    };
    if let Some(error) = open_error {
        let result = BrokerResult::Error(error);
        state.request_report.push(BrokerRequestReport {
            fixture: fixture_id,
            capability: "filesystem.write.v1",
            operation: fixture_operation,
            result: broker_result_name(&result),
        });
        return result;
    }
    let FilesystemWriteWireRequest::Stream(stream) = wire else {
        return BrokerResult::Error(BrokerErrorCode::Internal);
    };
    let resource_id = state.next_resource_id;
    state.next_resource_id = match resource_id.checked_add(1) {
        Some(next) => next,
        None => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let filesystem_mounts = state.filesystem_write_bindings.filesystem_mounts.clone();
    state.filesystem_write_resources.insert(
        resource_id,
        FilesystemWriteReplayResource {
            fixture_index,
            expected_bytes: stream.expected_bytes,
            received_bytes: 0,
            scope: scope.clone(),
            filesystem_mounts,
            sequence: 0,
        },
    );
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: "filesystem.write.v1",
        operation: fixture_operation,
        result: "opened",
    });
    match (FilesystemWriteStreamOpened {
        resource_id,
        maximum_chunk_bytes: MAX_FILE_WRITE_STREAM_CHUNK_BYTES as u32,
    })
    .encode()
    {
        Ok(payload) => BrokerResult::Success { payload },
        Err(_) => BrokerResult::Error(BrokerErrorCode::Internal),
    }
}

fn filesystem_write_chunk_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let chunk = match FilesystemWriteStreamChunk::decode(payload) {
        Ok(chunk) => chunk,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::InvalidRequest),
    };
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(resource) = state
        .filesystem_write_resources
        .get(&chunk.resource_id)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Unavailable);
    };
    let request = filesystem_write_resource_request(
        &state,
        request_id,
        FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
        payload,
        activation,
        &resource,
    );
    let authorization = match authorize_filesystem_write_stream_command(
        &request,
        chunk.resource_id,
        resource.expected_bytes,
        resource.received_bytes,
        &resource.scope,
        &resource.filesystem_mounts,
    ) {
        Ok(FilesystemWriteStreamCommandAuthorization::Chunk { end_offset, .. }) => end_offset,
        Ok(FilesystemWriteStreamCommandAuthorization::Commit) => {
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
        Err(error) => {
            state.violations.push(format!(
                "filesystem.write.v1 request {request_id} failed production stream authorization: {error:?}"
            ));
            return BrokerResult::Error(error);
        }
    };
    let fixture = &mut state.filesystem_writes[resource.fixture_index];
    let Some(expected) = fixture.chunks.front() else {
        state.violations.push(format!(
            "filesystem.write.v1 request {request_id} had no remaining exact chunk fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    if expected.bytes != chunk.bytes {
        state.violations.push(format!(
            "filesystem.write.v1 request {request_id} did not match the next exact chunk fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    }
    let fixture_id = fixture.id.clone();
    let result = fixture
        .chunks
        .pop_front()
        .expect("matched filesystem-write chunk exists")
        .result;
    if matches!(result, BrokerResult::Success { .. })
        && let Some(resource) = state.filesystem_write_resources.get_mut(&chunk.resource_id)
    {
        resource.received_bytes = authorization;
    }
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id,
        capability: "filesystem.write.v1",
        operation: FILESYSTEM_WRITE_STREAM_CHUNK_OPERATION,
        result: broker_result_name(&result),
    });
    result
}

fn filesystem_write_commit_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let commit = match FilesystemWriteStreamCommit::decode(payload) {
        Ok(commit) => commit,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::InvalidRequest),
    };
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(resource) = state
        .filesystem_write_resources
        .get(&commit.resource_id)
        .cloned()
    else {
        return BrokerResult::Error(BrokerErrorCode::Unavailable);
    };
    let request = filesystem_write_resource_request(
        &state,
        request_id,
        FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
        payload,
        activation,
        &resource,
    );
    match authorize_filesystem_write_stream_command(
        &request,
        commit.resource_id,
        resource.expected_bytes,
        resource.received_bytes,
        &resource.scope,
        &resource.filesystem_mounts,
    ) {
        Ok(FilesystemWriteStreamCommandAuthorization::Commit) => {}
        Ok(FilesystemWriteStreamCommandAuthorization::Chunk { .. }) => {
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
        Err(error) => {
            state.violations.push(format!(
                "filesystem.write.v1 request {request_id} failed production stream authorization: {error:?}"
            ));
            return BrokerResult::Error(error);
        }
    }
    let fixture = &mut state.filesystem_writes[resource.fixture_index];
    let Some(result) = fixture.commits.pop_front() else {
        state.violations.push(format!(
            "filesystem.write.v1 request {request_id} had no remaining exact commit fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    state.request_report.push(BrokerRequestReport {
        fixture: fixture_id.clone(),
        capability: "filesystem.write.v1",
        operation: FILESYSTEM_WRITE_STREAM_COMMIT_OPERATION,
        result: broker_result_name(&result),
    });
    if let BrokerResult::Success { payload } = &result {
        let sequence = resource.sequence.saturating_add(1);
        queue_resource_event(
            &mut state,
            &fixture_id,
            commit.resource_id,
            sequence,
            "filesystem-write-complete",
            BrokerResult::Success {
                payload: payload.clone(),
            },
        );
        state.filesystem_write_resources.remove(&commit.resource_id);
    }
    result
}

fn filesystem_write_resource_request(
    state: &ReplayBrokerState,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
    resource: &FilesystemWriteReplayResource,
) -> BackendRequest {
    BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::FilesystemWriteV1,
        authorized_scope: CapabilityScope::FilesystemWrite(resource.scope.clone()),
        bindings: GrantBindings {
            filesystem_mounts: state.filesystem_write_bindings.filesystem_mounts.clone(),
            ..GrantBindings::default()
        },
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    }
}

fn command_run_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state.permissions.get(&CapabilityId::CommandRunV1).cloned() else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::CommandRunV1,
        authorized_scope: permission.scope,
        bindings: GrantBindings::default(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let validator = CommandRunBackend::new(PathBuf::new());
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    if let Err(error) =
        ResourceBackend::authorize(&validator, &request, &mut state.activations, now)
    {
        state.violations.push(format!(
            "command.run.v1 request {request_id} failed production authorization: {error:?}"
        ));
        return BrokerResult::Error(error);
    }
    let wire = match CommandRunRequest::decode(payload) {
        Ok(wire) => canonicalize_command_request(wire),
        Err(_) => {
            state.violations.push(format!(
                "command.run.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    if operation != COMMAND_RUN_OPERATION {
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    }
    let Some(fixture) = state
        .command_runs
        .iter_mut()
        .find(|fixture| fixture.request == wire && !fixture.responses.is_empty())
    else {
        state.violations.push(format!(
            "command.run.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched command fixture has a response");
    match result {
        PreparedCommandResult::Inline(result) => {
            state.request_report.push(BrokerRequestReport {
                fixture: fixture_id,
                capability: "command.run.v1",
                operation: COMMAND_RUN_OPERATION,
                result: broker_result_name(&result),
            });
            result
        }
        PreparedCommandResult::Stream {
            events,
            terminal,
            terminal_kind,
        } => {
            let resource_id = state.next_resource_id;
            state.next_resource_id = match resource_id.checked_add(1) {
                Some(next) => next,
                None => return BrokerResult::Error(BrokerErrorCode::Internal),
            };
            state.request_report.push(BrokerRequestReport {
                fixture: fixture_id.clone(),
                capability: "command.run.v1",
                operation: COMMAND_RUN_OPERATION,
                result: "opened",
            });
            let mut sequence = 0_u64;
            for event in events {
                sequence = sequence.saturating_add(1);
                queue_resource_event(
                    &mut state,
                    &fixture_id,
                    resource_id,
                    sequence,
                    event.kind,
                    BrokerResult::Success {
                        payload: event.payload,
                    },
                );
            }
            sequence = sequence.saturating_add(1);
            queue_resource_event(
                &mut state,
                &fixture_id,
                resource_id,
                sequence,
                terminal_kind,
                terminal,
            );
            match (CommandOpened { resource_id }).encode() {
                Ok(payload) => BrokerResult::Success { payload },
                Err(_) => BrokerResult::Error(BrokerErrorCode::Internal),
            }
        }
    }
}

fn http_request_request(
    state: &Mutex<ReplayBrokerState>,
    request_id: u64,
    operation: &str,
    payload: &[u8],
    activation: Option<ActivationContext>,
) -> BrokerResult {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let Some(permission) = state.permissions.get(&CapabilityId::HttpRequestV1).cloned() else {
        return BrokerResult::Error(BrokerErrorCode::Denied);
    };
    let request = BackendRequest {
        identity: state.identity.clone(),
        request_id,
        capability: CapabilityId::HttpRequestV1,
        authorized_scope: permission.scope,
        bindings: GrantBindings::default(),
        activation,
        operation: operation.to_owned(),
        payload: payload.to_vec(),
    };
    let now = match monotonic_micros() {
        Ok(now) => now,
        Err(_) => return BrokerResult::Error(BrokerErrorCode::Internal),
    };
    let authorization = {
        let ReplayBrokerState {
            http_validator,
            activations,
            ..
        } = &mut *state;
        match operation {
            HTTP_REQUEST_OPERATION => {
                Backend::authorize(http_validator, &request, activations, now)
            }
            HTTP_STREAM_OPERATION => {
                ResourceBackend::authorize(http_validator, &request, activations, now)
            }
            _ => Err(BrokerErrorCode::InvalidRequest),
        }
    };
    if let Err(error) = authorization {
        state.violations.push(format!(
            "http.request.v1 request {request_id} failed production authorization: {error:?}"
        ));
        return BrokerResult::Error(error);
    }
    let wire = match HttpRequest::decode(payload) {
        Ok(wire) => wire,
        Err(_) => {
            state.violations.push(format!(
                "http.request.v1 request {request_id} had an invalid payload"
            ));
            return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
        }
    };
    let expected_operation = match operation {
        HTTP_REQUEST_OPERATION => HttpOperationFixture::Request,
        HTTP_STREAM_OPERATION => HttpOperationFixture::RequestStream,
        _ => return BrokerResult::Error(BrokerErrorCode::InvalidRequest),
    };
    let Some(fixture) = state.http_requests.iter_mut().find(|fixture| {
        fixture.operation == expected_operation
            && fixture.request == wire
            && !fixture.responses.is_empty()
    }) else {
        state.violations.push(format!(
            "http.request.v1 request {request_id} did not match a remaining exact fixture"
        ));
        return BrokerResult::Error(BrokerErrorCode::InvalidRequest);
    };
    let fixture_id = fixture.id.clone();
    let result = fixture
        .responses
        .pop_front()
        .expect("matched HTTP fixture has a response");
    match result {
        PreparedHttpResult::Inline(result) => {
            state.request_report.push(BrokerRequestReport {
                fixture: fixture_id,
                capability: "http.request.v1",
                operation: if expected_operation == HttpOperationFixture::Request {
                    HTTP_REQUEST_OPERATION
                } else {
                    HTTP_STREAM_OPERATION
                },
                result: broker_result_name(&result),
            });
            result
        }
        PreparedHttpResult::Stream {
            metadata,
            chunks,
            terminal,
        } => {
            let resource_id = state.next_resource_id;
            state.next_resource_id = match resource_id.checked_add(1) {
                Some(next) => next,
                None => return BrokerResult::Error(BrokerErrorCode::Internal),
            };
            state.request_report.push(BrokerRequestReport {
                fixture: fixture_id.clone(),
                capability: "http.request.v1",
                operation: HTTP_STREAM_OPERATION,
                result: "opened",
            });
            let mut sequence = 1_u64;
            queue_resource_event(
                &mut state,
                &fixture_id,
                resource_id,
                sequence,
                "http-metadata",
                BrokerResult::Success { payload: metadata },
            );
            for payload in chunks {
                sequence = sequence.saturating_add(1);
                queue_resource_event(
                    &mut state,
                    &fixture_id,
                    resource_id,
                    sequence,
                    "http-chunk",
                    BrokerResult::Success { payload },
                );
            }
            sequence = sequence.saturating_add(1);
            let kind = match &terminal {
                BrokerResult::Success { .. } => "http-complete",
                BrokerResult::Error(_) => "http-error",
            };
            queue_resource_event(
                &mut state,
                &fixture_id,
                resource_id,
                sequence,
                kind,
                terminal,
            );
            match (HttpStreamOpened { resource_id }).encode() {
                Ok(payload) => BrokerResult::Success { payload },
                Err(_) => BrokerResult::Error(BrokerErrorCode::Internal),
            }
        }
    }
}

fn queue_resource_event(
    state: &mut ReplayBrokerState,
    fixture: &str,
    resource_id: u64,
    sequence: u64,
    kind: &'static str,
    result: BrokerResult,
) {
    state.resource_event_report.push(BrokerResourceEventReport {
        fixture: fixture.to_owned(),
        sequence,
        kind,
    });
    state
        .pending_events
        .push_back(SupervisorMessage::ResourceEvent {
            resource_id,
            sequence,
            result,
        });
}

fn replay_clipboard_bindings() -> GrantBindings {
    GrantBindings {
        clipboard: Some(ClipboardBinding::WaylandDataControl {
            socket: PathBuf::from("/touchbar-replay/nonexistent-wayland-clipboard"),
        }),
        ..Default::default()
    }
}

fn clipboard_capability(operation: ClipboardOperationFixture) -> CapabilityId {
    match operation {
        ClipboardOperationFixture::Read => CapabilityId::ClipboardReadV1,
        ClipboardOperationFixture::Write => CapabilityId::ClipboardWriteV1,
    }
}

fn clipboard_capability_name(operation: ClipboardOperationFixture) -> &'static str {
    match operation {
        ClipboardOperationFixture::Read => "clipboard.read.v1",
        ClipboardOperationFixture::Write => "clipboard.write.v1",
    }
}

fn clipboard_operation(operation: ClipboardOperationFixture) -> &'static str {
    match operation {
        ClipboardOperationFixture::Read => CLIPBOARD_READ_OPERATION,
        ClipboardOperationFixture::Write => CLIPBOARD_WRITE_OPERATION,
    }
}

fn clipboard_request(
    operation: ClipboardOperationFixture,
    request: &ClipboardRequestFixtureValue,
) -> Result<ClipboardWireRequest> {
    match (operation, request) {
        (ClipboardOperationFixture::Read, ClipboardRequestFixtureValue::Read(request)) => {
            Ok(ClipboardWireRequest::Read(ClipboardReadRequest {
                mime_type: request.mime_type.clone(),
            }))
        }
        (ClipboardOperationFixture::Write, ClipboardRequestFixtureValue::Write(request)) => {
            Ok(ClipboardWireRequest::Write(ClipboardWriteRequest {
                mime_type: request.mime_type.clone(),
                bytes: byte_fixture(&request.body),
            }))
        }
        _ => bail!("clipboard fixture request does not match its operation"),
    }
}

fn encode_clipboard_wire(request: &ClipboardWireRequest) -> Result<Vec<u8>> {
    match request {
        ClipboardWireRequest::Read(request) => {
            request.encode().context("encode clipboard-read fixture")
        }
        ClipboardWireRequest::Write(request) => {
            request.encode().context("encode clipboard-write fixture")
        }
    }
}

fn decode_clipboard_wire(
    operation: ClipboardOperationFixture,
    payload: &[u8],
) -> std::result::Result<ClipboardWireRequest, touchbar_broker_schema::SchemaError> {
    match operation {
        ClipboardOperationFixture::Read => {
            ClipboardReadRequest::decode(payload).map(ClipboardWireRequest::Read)
        }
        ClipboardOperationFixture::Write => {
            ClipboardWriteRequest::decode(payload).map(ClipboardWireRequest::Write)
        }
    }
}

fn validate_clipboard_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    bindings: &GrantBindings,
    operation: ClipboardOperationFixture,
    wire: &ClipboardWireRequest,
) -> Result<ClipboardAuthorization> {
    let now = monotonic_micros().context("read monotonic clock for clipboard fixture")?;
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: clipboard_capability(operation),
        authorized_scope: permission.scope.clone(),
        bindings: bindings.clone(),
        activation: Some(ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 1,
            item_id: "fixture".into(),
            widget_id: 1,
            input_sequence: 1,
            deadline_monotonic_micros: now.saturating_add(1_000_000),
        }),
        operation: clipboard_operation(operation).into(),
        payload: encode_clipboard_wire(wire)?,
    };
    authorize_clipboard_request(&request, &mut ActivationLedger::new(1), now)
        .map_err(|error| anyhow::anyhow!("clipboard fixture is not authorized: {error:?}"))
}

fn clipboard_result(
    result: &ClipboardResultFixture,
    operation: ClipboardOperationFixture,
    request: &ClipboardRequestFixtureValue,
    maximum_bytes: usize,
) -> Result<BrokerResult> {
    match result {
        ClipboardResultFixture::Error { code } => Ok(BrokerResult::Error(broker_error(*code))),
        ClipboardResultFixture::Value { mime_type, body }
            if operation == ClipboardOperationFixture::Read =>
        {
            let ClipboardRequestFixtureValue::Read(request) = request else {
                bail!("clipboard read fixture has an inconsistent request shape");
            };
            if mime_type != &request.mime_type {
                bail!("clipboard read fixture response MIME must match the requested MIME");
            }
            let bytes = byte_fixture(body);
            if bytes.len() > maximum_bytes {
                bail!(
                    "clipboard read fixture returns {} bytes, exceeding the manifest maximum_bytes limit of {maximum_bytes}",
                    bytes.len()
                );
            }
            Ok(BrokerResult::Success {
                payload: ClipboardValue {
                    mime_type: mime_type.clone(),
                    bytes,
                }
                .encode()
                .context("encode clipboard value fixture")?,
            })
        }
        ClipboardResultFixture::Success if operation == ClipboardOperationFixture::Write => {
            Ok(BrokerResult::Success {
                payload: Vec::new(),
            })
        }
        _ => bail!("clipboard fixture response kind does not match the request operation"),
    }
}

fn replay_secret_bindings(permission: &CapabilityRequest) -> Result<GrantBindings> {
    let CapabilityScope::SecretRead(scope) = &permission.scope else {
        bail!("secret.read.v1 permission has an inconsistent normalized scope");
    };
    let secrets = scope
        .logical_names
        .iter()
        .enumerate()
        .map(|(index, logical_name)| {
            (
                logical_name.clone(),
                SecretBinding::SecretServiceItem {
                    object_path: format!("/org/freedesktop/secrets/touchbar_replay/item_{index}"),
                },
            )
        })
        .collect();
    Ok(GrantBindings {
        secrets,
        ..Default::default()
    })
}

fn validate_secret_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    bindings: &GrantBindings,
    wire: &SecretReadRequest,
) -> Result<()> {
    let now = monotonic_micros().context("read monotonic clock for secret fixture")?;
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::SecretReadV1,
        authorized_scope: permission.scope.clone(),
        bindings: bindings.clone(),
        activation: Some(ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 1,
            item_id: "fixture".into(),
            widget_id: 1,
            input_sequence: 1,
            deadline_monotonic_micros: now.saturating_add(1_000_000),
        }),
        operation: SECRET_READ_OPERATION.into(),
        payload: wire.encode().context("encode secret-read fixture")?,
    };
    authorize_secret_read_request(&request, &mut ActivationLedger::new(1), now)
        .map_err(|error| anyhow::anyhow!("secret fixture is not authorized: {error:?}"))
}

fn secret_result(result: &SecretResultFixture) -> Result<BrokerResult> {
    match result {
        SecretResultFixture::Error { code } => Ok(BrokerResult::Error(broker_error(*code))),
        SecretResultFixture::Value { content_type, body } => {
            let bytes = byte_fixture(body);
            validate_secret_value(bytes.len(), content_type)
                .map_err(|error| anyhow::anyhow!("secret fixture value is invalid: {error:?}"))?;
            Ok(BrokerResult::Success {
                payload: SecretValue {
                    bytes,
                    content_type: content_type.clone(),
                }
                .encode()
                .context("encode secret value fixture")?,
            })
        }
    }
}

fn notification_request(
    operation: NotificationOperationFixture,
    request: &NotificationRequestFixtureValue,
) -> Result<NotificationWireRequest> {
    match (operation, request) {
        (NotificationOperationFixture::Send, NotificationRequestFixtureValue::Send(request)) => {
            Ok(NotificationWireRequest::Send(NotificationSend {
                id: request.id.clone(),
                category: request.category.clone(),
                urgency: match request.urgency {
                    NotificationUrgencyFixture::Low => NotificationUrgencyValue::Low,
                    NotificationUrgencyFixture::Normal => NotificationUrgencyValue::Normal,
                    NotificationUrgencyFixture::Critical => NotificationUrgencyValue::Critical,
                },
                title: request.title.clone(),
                body: request.body.clone(),
            }))
        }
        (
            NotificationOperationFixture::Remove,
            NotificationRequestFixtureValue::Remove(request),
        ) => Ok(NotificationWireRequest::Remove(NotificationRemove {
            id: request.id.clone(),
        })),
        _ => bail!("notification fixture request does not match its operation"),
    }
}

fn notification_operation(operation: NotificationOperationFixture) -> &'static str {
    match operation {
        NotificationOperationFixture::Send => NOTIFICATION_SEND_OPERATION,
        NotificationOperationFixture::Remove => NOTIFICATION_REMOVE_OPERATION,
    }
}

fn encode_notification_wire(request: &NotificationWireRequest) -> Result<Vec<u8>> {
    match request {
        NotificationWireRequest::Send(request) => {
            request.encode().context("encode notification-send fixture")
        }
        NotificationWireRequest::Remove(request) => request
            .encode()
            .context("encode notification-remove fixture"),
    }
}

fn decode_notification_wire(
    operation: &str,
    payload: &[u8],
) -> std::result::Result<NotificationWireRequest, touchbar_broker_schema::SchemaError> {
    match operation {
        NOTIFICATION_SEND_OPERATION => {
            NotificationSend::decode(payload).map(NotificationWireRequest::Send)
        }
        NOTIFICATION_REMOVE_OPERATION => {
            NotificationRemove::decode(payload).map(NotificationWireRequest::Remove)
        }
        _ => Err(touchbar_broker_schema::SchemaError::Malformed),
    }
}

fn validate_notification_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    operation: NotificationOperationFixture,
    wire: &NotificationWireRequest,
) -> Result<()> {
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::NotificationSendV1,
        authorized_scope: permission.scope.clone(),
        bindings: GrantBindings::default(),
        activation: None,
        operation: notification_operation(operation).into(),
        payload: encode_notification_wire(wire)?,
    };
    authorize_notification_request(&request)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("notification fixture is not authorized: {error:?}"))
}

fn validate_uri_open_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    wire: &UriOpenRequest,
) -> Result<()> {
    let now = monotonic_micros().context("read monotonic clock for URI-open fixture")?;
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::UriOpenV1,
        authorized_scope: permission.scope.clone(),
        bindings: GrantBindings::default(),
        activation: Some(ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 1,
            item_id: "fixture".into(),
            widget_id: 1,
            input_sequence: 1,
            deadline_monotonic_micros: now.saturating_add(1_000_000),
        }),
        operation: URI_OPEN_OPERATION.into(),
        payload: wire.encode().context("encode URI-open fixture")?,
    };
    UriOpenBackend::new(RejectingUriTransport)
        .authorize(&request, &mut ActivationLedger::new(1), now)
        .map_err(|error| anyhow::anyhow!("URI-open fixture is not authorized: {error:?}"))
}

fn desktop_result(result: &DesktopResultFixture) -> BrokerResult {
    match result {
        DesktopResultFixture::Success => BrokerResult::Success {
            payload: Vec::new(),
        },
        DesktopResultFixture::Error { code } => BrokerResult::Error(broker_error(*code)),
    }
}

fn validate_dbus_call_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    call: &DbusCall,
) -> Result<()> {
    let now = monotonic_micros().context("read monotonic clock for D-Bus fixture")?;
    let activation = ActivationContext {
        origin: ActivationOrigin::Physical,
        surface_instance: 1,
        item_id: "fixture".into(),
        widget_id: 1,
        input_sequence: 1,
        deadline_monotonic_micros: now.saturating_add(1_000_000),
    };
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::DbusCallV1,
        authorized_scope: permission.scope.clone(),
        bindings: GrantBindings::default(),
        activation: Some(activation),
        operation: DBUS_CALL_OPERATION.into(),
        payload: call.encode().context("encode D-Bus call fixture")?,
    };
    DbusCallBackend::new(RejectingDbusTransport)
        .authorize(&request, &mut ActivationLedger::new(1), now)
        .map_err(|error| anyhow::anyhow!("D-Bus call fixture is not authorized: {error:?}"))
}

fn validate_dbus_subscription_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    subscription: &DbusSubscription,
) -> Result<()> {
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::DbusSubscribeV1,
        authorized_scope: permission.scope.clone(),
        bindings: GrantBindings::default(),
        activation: None,
        operation: "subscribe".into(),
        payload: subscription
            .encode()
            .context("encode D-Bus subscription fixture")?,
    };
    ResourceBackend::authorize(
        &DbusSubscriptionBackend::new(RejectingDbusTransport),
        &request,
        &mut ActivationLedger::new(1),
        monotonic_micros().context("read monotonic clock for D-Bus fixture")?,
    )
    .map_err(|error| anyhow::anyhow!("D-Bus subscription fixture is not authorized: {error:?}"))
}

fn replay_local_bindings(permission: &CapabilityRequest) -> Result<GrantBindings> {
    let CapabilityScope::LocalConnect(scope) = &permission.scope else {
        bail!("local.connect.v1 permission has an inconsistent normalized scope");
    };
    let local_endpoints = scope
        .endpoints
        .iter()
        .map(|endpoint| {
            (
                endpoint.label.clone(),
                LocalEndpointBinding::UnixStream {
                    path: PathBuf::from("/__touchbar_replay_never_connect__.sock"),
                },
            )
        })
        .collect();
    Ok(GrantBindings {
        local_endpoints,
        ..GrantBindings::default()
    })
}

fn prepare_local_connection(
    fixture: &LocalConnectionFixture,
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    bindings: &GrantBindings,
) -> Result<PreparedLocalConnection> {
    let request = LocalConnect {
        endpoint: fixture.request.endpoint.clone(),
        protocol: fixture.request.protocol.clone(),
    };
    let backend_request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::LocalConnectV1,
        authorized_scope: permission.scope.clone(),
        bindings: bindings.clone(),
        activation: None,
        operation: LOCAL_CONNECT_OPERATION.into(),
        payload: request
            .encode()
            .context("encode local connection fixture")?,
    };
    let (_, authorization) =
        authorize_local_connect_request(&backend_request).map_err(|error| {
            anyhow::anyhow!("local connection fixture is not authorized: {error:?}")
        })?;
    if fixture.open_error.is_some()
        && (!fixture.initial_events.is_empty() || !fixture.exchanges.is_empty())
    {
        bail!("local connection fixture with open_error cannot contain events or exchanges");
    }
    let CapabilityScope::LocalConnect(scope) = &permission.scope else {
        bail!("local.connect.v1 permission has an inconsistent normalized scope");
    };
    let maximum_frame_bytes = authorization.maximum_frame_bytes();
    let mut traffic_bytes = 0_u64;
    let mut incoming_frames = 0_u16;
    let (initial_events, mut terminated) = prepare_local_events(
        &fixture.initial_events,
        maximum_frame_bytes,
        scope.maximum_bytes_per_minute,
        &mut traffic_bytes,
        &mut incoming_frames,
    )?;
    let mut exchanges = VecDeque::with_capacity(fixture.exchanges.len());
    for exchange in &fixture.exchanges {
        if terminated {
            bail!("local connection fixture has an exchange after a terminal peer event");
        }
        let send = byte_fixture(&exchange.send);
        LocalSendFrame {
            resource_id: 1,
            bytes: send.clone(),
        }
        .encode()
        .context("encode local send fixture")?;
        if send.len() > maximum_frame_bytes as usize {
            bail!(
                "local send fixture is {} bytes, exceeding maximum_frame_bytes {maximum_frame_bytes}",
                send.len()
            );
        }
        reserve_local_fixture_traffic(
            &mut traffic_bytes,
            send.len() as u64,
            scope.maximum_bytes_per_minute,
        )?;
        let result = match exchange.result {
            LocalSendResultFixture::Success => BrokerResult::Success {
                payload: Vec::new(),
            },
            LocalSendResultFixture::Error { code } => BrokerResult::Error(broker_error(code)),
        };
        let (events, exchange_terminated) = prepare_local_events(
            &exchange.events,
            maximum_frame_bytes,
            scope.maximum_bytes_per_minute,
            &mut traffic_bytes,
            &mut incoming_frames,
        )?;
        terminated |= exchange_terminated;
        exchanges.push_back(PreparedLocalExchange {
            send,
            result,
            events,
        });
    }
    Ok(PreparedLocalConnection {
        id: fixture.id.clone(),
        request,
        open_error: fixture.open_error.map(broker_error),
        initial_events,
        exchanges,
        consumed: false,
    })
}

fn prepare_local_events(
    fixtures: &[LocalPeerEventFixture],
    maximum_frame_bytes: u32,
    maximum_bytes_per_minute: u64,
    traffic_bytes: &mut u64,
    incoming_frames: &mut u16,
) -> Result<(Vec<PreparedLocalEvent>, bool)> {
    let mut events = Vec::with_capacity(fixtures.len());
    let mut terminated = false;
    for fixture in fixtures {
        if terminated {
            bail!("local peer fixture contains an event after a terminal event");
        }
        let event = match fixture {
            LocalPeerEventFixture::Frame { body } => {
                let bytes = byte_fixture(body);
                if bytes.len() > maximum_frame_bytes as usize {
                    bail!(
                        "local frame fixture is {} bytes, exceeding maximum_frame_bytes {maximum_frame_bytes}",
                        bytes.len()
                    );
                }
                *incoming_frames = incoming_frames
                    .checked_add(1)
                    .context("local fixture incoming frame count overflow")?;
                if *incoming_frames > LOCAL_MAXIMUM_EVENTS_PER_SECOND {
                    bail!(
                        "local fixture exceeds the production {LOCAL_MAXIMUM_EVENTS_PER_SECOND} event/second limit"
                    );
                }
                reserve_local_fixture_traffic(
                    traffic_bytes,
                    bytes.len() as u64,
                    maximum_bytes_per_minute,
                )?;
                PreparedLocalEvent {
                    result: BrokerResult::Success {
                        payload: LocalFrameEvent { bytes }
                            .encode()
                            .context("encode local frame fixture")?,
                    },
                    kind: "local-frame",
                    terminal: false,
                }
            }
            LocalPeerEventFixture::Closed => {
                terminated = true;
                PreparedLocalEvent {
                    result: BrokerResult::Success {
                        payload: Vec::new(),
                    },
                    kind: "local-closed",
                    terminal: true,
                }
            }
            LocalPeerEventFixture::Error { code } => {
                terminated = true;
                PreparedLocalEvent {
                    result: BrokerResult::Error(broker_error(*code)),
                    kind: "local-error",
                    terminal: true,
                }
            }
        };
        events.push(event);
    }
    Ok((events, terminated))
}

fn reserve_local_fixture_traffic(
    total: &mut u64,
    bytes: u64,
    maximum_bytes_per_minute: u64,
) -> Result<()> {
    *total = total
        .checked_add(bytes)
        .context("local fixture traffic byte count overflow")?;
    if *total > maximum_bytes_per_minute {
        bail!(
            "local fixture traffic is {total} bytes, exceeding maximum_bytes_per_minute {maximum_bytes_per_minute}"
        );
    }
    Ok(())
}

fn replay_filesystem_bindings(permission: &CapabilityRequest) -> Result<GrantBindings> {
    let CapabilityScope::FilesystemRead(scope) = &permission.scope else {
        bail!("filesystem.read.v1 permission has an inconsistent normalized scope");
    };
    let filesystem_mounts = scope
        .mounts
        .iter()
        .map(|mount| {
            (
                mount.label.clone(),
                FilesystemMountBinding::Path {
                    path: PathBuf::from("/__touchbar_replay_never_open__"),
                },
            )
        })
        .collect();
    Ok(GrantBindings {
        filesystem_mounts,
        ..GrantBindings::default()
    })
}

fn replay_filesystem_write_bindings(permission: &CapabilityRequest) -> Result<GrantBindings> {
    let CapabilityScope::FilesystemWrite(scope) = &permission.scope else {
        bail!("filesystem.write.v1 permission has an inconsistent normalized scope");
    };
    let filesystem_mounts = scope
        .mounts
        .iter()
        .map(|mount| {
            (
                mount.label.clone(),
                FilesystemMountBinding::Path {
                    path: PathBuf::from("/__touchbar_replay_never_open__"),
                },
            )
        })
        .collect();
    Ok(GrantBindings {
        filesystem_mounts,
        ..GrantBindings::default()
    })
}

fn prepare_filesystem_write(
    fixture: &FilesystemWriteFixture,
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    bindings: &GrantBindings,
) -> Result<PreparedFilesystemWrite> {
    let wire = filesystem_write_request(fixture.operation, &fixture.request)?;
    validate_filesystem_write_authority(identity, permission, bindings, fixture.operation, &wire)?;
    let CapabilityScope::FilesystemWrite(scope) = &permission.scope else {
        bail!("filesystem.write.v1 permission has an inconsistent normalized scope");
    };

    if fixture.operation.is_stream() {
        if !fixture.responses.is_empty() {
            bail!(
                "filesystem-write stream fixture `{}` cannot declare inline responses",
                fixture.id
            );
        }
        if fixture.open_error.is_some()
            && (!fixture.chunks.is_empty() || !fixture.commits.is_empty())
        {
            bail!(
                "filesystem-write stream fixture `{}` cannot declare traffic after an open error",
                fixture.id
            );
        }
        let FilesystemWriteWireRequest::Stream(stream) = &wire else {
            bail!(
                "filesystem-write stream fixture `{}` has an inconsistent request",
                fixture.id
            );
        };
        let mut successful_bytes = 0_u64;
        let mut chunks = VecDeque::with_capacity(fixture.chunks.len());
        for chunk in &fixture.chunks {
            let bytes = byte_fixture(&chunk.body);
            if bytes.is_empty() || bytes.len() > MAX_FILE_WRITE_STREAM_CHUNK_BYTES {
                bail!(
                    "filesystem-write stream fixture `{}` has a chunk outside the production 1..={MAX_FILE_WRITE_STREAM_CHUNK_BYTES} byte limit",
                    fixture.id
                );
            }
            let result = match chunk.result {
                FilesystemWriteChunkResultFixture::Success => {
                    successful_bytes = successful_bytes
                        .checked_add(bytes.len() as u64)
                        .context("filesystem-write stream fixture byte count overflow")?;
                    BrokerResult::Success {
                        payload: Vec::new(),
                    }
                }
                FilesystemWriteChunkResultFixture::Error { code } => {
                    BrokerResult::Error(broker_error(code))
                }
            };
            chunks.push_back(PreparedFilesystemWriteChunk { bytes, result });
        }
        if !fixture.commits.is_empty() && successful_bytes != stream.expected_bytes {
            bail!(
                "filesystem-write stream fixture `{}` commits after {successful_bytes} successful bytes, but expected_bytes is {}",
                fixture.id,
                stream.expected_bytes
            );
        }
        if successful_bytes > stream.expected_bytes {
            bail!(
                "filesystem-write stream fixture `{}` supplies more successful bytes than expected_bytes",
                fixture.id
            );
        }
        let commits = fixture
            .commits
            .iter()
            .map(|result| filesystem_write_result(result, fixture.operation, &wire, scope))
            .collect::<Result<VecDeque<_>>>()?;
        Ok(PreparedFilesystemWrite {
            id: fixture.id.clone(),
            operation: fixture.operation,
            request: wire,
            responses: VecDeque::new(),
            open_error: fixture.open_error.map(broker_error),
            chunks,
            commits,
            consumed: false,
        })
    } else {
        if fixture.responses.is_empty() {
            bail!(
                "filesystem-write fixture `{}` requires at least one response",
                fixture.id
            );
        }
        if fixture.open_error.is_some() || !fixture.chunks.is_empty() || !fixture.commits.is_empty()
        {
            bail!(
                "inline filesystem-write fixture `{}` cannot declare stream fields",
                fixture.id
            );
        }
        let responses = fixture
            .responses
            .iter()
            .map(|result| filesystem_write_result(result, fixture.operation, &wire, scope))
            .collect::<Result<VecDeque<_>>>()?;
        Ok(PreparedFilesystemWrite {
            id: fixture.id.clone(),
            operation: fixture.operation,
            request: wire,
            responses,
            open_error: None,
            chunks: VecDeque::new(),
            commits: VecDeque::new(),
            consumed: false,
        })
    }
}

fn filesystem_write_request(
    operation: FilesystemWriteOperationFixture,
    request: &FilesystemWriteRequestFixture,
) -> Result<FilesystemWriteWireRequest> {
    match (operation, request) {
        (
            FilesystemWriteOperationFixture::CreateFile
            | FilesystemWriteOperationFixture::ReplaceFile
            | FilesystemWriteOperationFixture::AppendFile,
            FilesystemWriteRequestFixture::Write(request),
        ) => Ok(FilesystemWriteWireRequest::Write(FilesystemWriteFile {
            mount: request.mount.clone(),
            path: request.path.clone(),
            bytes: byte_fixture(&request.body),
        })),
        (
            FilesystemWriteOperationFixture::DeleteFile
            | FilesystemWriteOperationFixture::CreateDirectory,
            FilesystemWriteRequestFixture::Path(request),
        ) => Ok(FilesystemWriteWireRequest::Path(FilesystemPath {
            mount: request.mount.clone(),
            path: request.path.clone(),
        })),
        (
            FilesystemWriteOperationFixture::Rename,
            FilesystemWriteRequestFixture::Rename(request),
        ) => Ok(FilesystemWriteWireRequest::Rename(FilesystemRename {
            mount: request.mount.clone(),
            source: request.source.clone(),
            destination: request.destination.clone(),
        })),
        (
            FilesystemWriteOperationFixture::CreateFileStream
            | FilesystemWriteOperationFixture::ReplaceFileStream
            | FilesystemWriteOperationFixture::AppendFileStream,
            FilesystemWriteRequestFixture::Stream(request),
        ) => Ok(FilesystemWriteWireRequest::Stream(FilesystemWriteStream {
            mount: request.mount.clone(),
            path: request.path.clone(),
            expected_bytes: request.expected_bytes,
        })),
        _ => bail!("filesystem-write fixture request shape does not match its operation"),
    }
}

fn filesystem_write_operation(operation: FilesystemWriteOperationFixture) -> &'static str {
    match operation {
        FilesystemWriteOperationFixture::CreateFile => FILESYSTEM_CREATE_FILE_OPERATION,
        FilesystemWriteOperationFixture::ReplaceFile => FILESYSTEM_REPLACE_FILE_OPERATION,
        FilesystemWriteOperationFixture::AppendFile => FILESYSTEM_APPEND_FILE_OPERATION,
        FilesystemWriteOperationFixture::DeleteFile => FILESYSTEM_DELETE_FILE_OPERATION,
        FilesystemWriteOperationFixture::Rename => FILESYSTEM_RENAME_OPERATION,
        FilesystemWriteOperationFixture::CreateDirectory => FILESYSTEM_CREATE_DIRECTORY_OPERATION,
        FilesystemWriteOperationFixture::CreateFileStream => {
            FILESYSTEM_CREATE_FILE_STREAM_OPERATION
        }
        FilesystemWriteOperationFixture::ReplaceFileStream => {
            FILESYSTEM_REPLACE_FILE_STREAM_OPERATION
        }
        FilesystemWriteOperationFixture::AppendFileStream => {
            FILESYSTEM_APPEND_FILE_STREAM_OPERATION
        }
    }
}

fn encode_filesystem_write_wire(request: &FilesystemWriteWireRequest) -> Result<Vec<u8>> {
    match request {
        FilesystemWriteWireRequest::Write(request) => request.encode(),
        FilesystemWriteWireRequest::Path(request) => request.encode(),
        FilesystemWriteWireRequest::Rename(request) => request.encode(),
        FilesystemWriteWireRequest::Stream(request) => request.encode(),
    }
    .context("encode filesystem-write fixture request")
}

fn decode_filesystem_write_wire(
    operation: &str,
    payload: &[u8],
) -> Result<FilesystemWriteWireRequest> {
    match operation {
        FILESYSTEM_CREATE_FILE_OPERATION
        | FILESYSTEM_REPLACE_FILE_OPERATION
        | FILESYSTEM_APPEND_FILE_OPERATION => FilesystemWriteFile::decode(payload)
            .map(FilesystemWriteWireRequest::Write)
            .context("decode filesystem-write file request"),
        FILESYSTEM_DELETE_FILE_OPERATION | FILESYSTEM_CREATE_DIRECTORY_OPERATION => {
            FilesystemPath::decode(payload)
                .map(FilesystemWriteWireRequest::Path)
                .context("decode filesystem-write path request")
        }
        FILESYSTEM_RENAME_OPERATION => FilesystemRename::decode(payload)
            .map(FilesystemWriteWireRequest::Rename)
            .context("decode filesystem-write rename request"),
        FILESYSTEM_CREATE_FILE_STREAM_OPERATION
        | FILESYSTEM_REPLACE_FILE_STREAM_OPERATION
        | FILESYSTEM_APPEND_FILE_STREAM_OPERATION => FilesystemWriteStream::decode(payload)
            .map(FilesystemWriteWireRequest::Stream)
            .context("decode filesystem-write stream request"),
        _ => bail!("unknown filesystem-write operation `{operation}`"),
    }
}

fn validate_filesystem_write_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    bindings: &GrantBindings,
    operation: FilesystemWriteOperationFixture,
    wire: &FilesystemWriteWireRequest,
) -> Result<()> {
    validate_filesystem_write_authority_for_operation(
        identity,
        permission,
        bindings,
        filesystem_write_operation(operation),
        wire,
    )
}

fn validate_filesystem_write_authority_for_operation(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    bindings: &GrantBindings,
    operation: &str,
    wire: &FilesystemWriteWireRequest,
) -> Result<()> {
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::FilesystemWriteV1,
        authorized_scope: permission.scope.clone(),
        bindings: bindings.clone(),
        activation: None,
        operation: operation.into(),
        payload: encode_filesystem_write_wire(wire)?,
    };
    let result = if matches!(wire, FilesystemWriteWireRequest::Stream(_)) {
        authorize_filesystem_write_stream_begin_request(&request).map(|_| ())
    } else {
        authorize_filesystem_write_mutation_request(&request).map(|_| ())
    };
    result.map_err(|error| anyhow::anyhow!("filesystem-write fixture is not authorized: {error:?}"))
}

fn filesystem_write_result(
    result: &FilesystemWriteResultFixture,
    operation: FilesystemWriteOperationFixture,
    wire: &FilesystemWriteWireRequest,
    scope: &FilesystemWriteScope,
) -> Result<BrokerResult> {
    let (bytes_written, resulting_size) = match result {
        FilesystemWriteResultFixture::Mutation {
            bytes_written,
            resulting_size,
        } => (bytes_written, resulting_size),
        FilesystemWriteResultFixture::Error { code } => {
            return Ok(BrokerResult::Error(broker_error(*code)));
        }
    };
    let expected_bytes = match wire {
        FilesystemWriteWireRequest::Write(write) => write.bytes.len() as u64,
        FilesystemWriteWireRequest::Stream(stream) => stream.expected_bytes,
        FilesystemWriteWireRequest::Path(_) | FilesystemWriteWireRequest::Rename(_) => 0,
    };
    if *bytes_written != expected_bytes {
        bail!(
            "filesystem-write fixture mutation reports {bytes_written} bytes_written; expected {expected_bytes}"
        );
    }
    match operation {
        FilesystemWriteOperationFixture::CreateFile
        | FilesystemWriteOperationFixture::ReplaceFile
        | FilesystemWriteOperationFixture::CreateFileStream
        | FilesystemWriteOperationFixture::ReplaceFileStream => {
            if *resulting_size != Some(expected_bytes) {
                bail!(
                    "filesystem-write create/replace receipt must report resulting_size {expected_bytes}"
                );
            }
        }
        FilesystemWriteOperationFixture::AppendFile
        | FilesystemWriteOperationFixture::AppendFileStream => {
            let Some(size) = *resulting_size else {
                bail!("filesystem-write append receipt requires resulting_size");
            };
            if size < expected_bytes || size > scope.maximum_file_bytes {
                bail!(
                    "filesystem-write append receipt resulting_size must be between bytes_written and maximum_file_bytes"
                );
            }
        }
        FilesystemWriteOperationFixture::DeleteFile
        | FilesystemWriteOperationFixture::Rename
        | FilesystemWriteOperationFixture::CreateDirectory => {
            if resulting_size.is_some() {
                bail!("filesystem-write metadata receipt must omit resulting_size");
            }
        }
    }
    Ok(BrokerResult::Success {
        payload: FilesystemMutationResult {
            bytes_written: *bytes_written,
            resulting_size: *resulting_size,
        }
        .encode()
        .context("encode filesystem-write mutation fixture")?,
    })
}

fn filesystem_read_request(
    operation: FilesystemReadOperationFixture,
    request: &FilesystemReadRequestFixture,
) -> Result<FilesystemReadWireRequest> {
    match (operation, request) {
        (FilesystemReadOperationFixture::ReadFile, FilesystemReadRequestFixture::Read(request)) => {
            Ok(FilesystemReadWireRequest::ReadFile(FilesystemReadFile {
                mount: request.mount.clone(),
                path: request.path.clone(),
                offset: request.offset,
                maximum_bytes: request.maximum_bytes,
            }))
        }
        (
            FilesystemReadOperationFixture::ReadFileStream,
            FilesystemReadRequestFixture::Read(request),
        ) => Ok(FilesystemReadWireRequest::ReadFileStream(
            FilesystemReadStream {
                mount: request.mount.clone(),
                path: request.path.clone(),
                offset: request.offset,
                maximum_bytes: request.maximum_bytes,
            },
        )),
        (
            FilesystemReadOperationFixture::ListDirectory,
            FilesystemReadRequestFixture::List(request),
        ) => Ok(FilesystemReadWireRequest::ListDirectory(
            FilesystemListDirectory {
                mount: request.mount.clone(),
                path: request.path.clone(),
                maximum_entries: request.maximum_entries,
            },
        )),
        _ => bail!("filesystem fixture request shape does not match its operation"),
    }
}

fn filesystem_operation(operation: FilesystemReadOperationFixture) -> &'static str {
    match operation {
        FilesystemReadOperationFixture::ReadFile => FILESYSTEM_READ_FILE_OPERATION,
        FilesystemReadOperationFixture::ReadFileStream => FILESYSTEM_READ_STREAM_OPERATION,
        FilesystemReadOperationFixture::ListDirectory => FILESYSTEM_LIST_DIRECTORY_OPERATION,
    }
}

fn filesystem_wire_operation(request: &FilesystemReadWireRequest) -> &'static str {
    match request {
        FilesystemReadWireRequest::ReadFile(_) => FILESYSTEM_READ_FILE_OPERATION,
        FilesystemReadWireRequest::ReadFileStream(_) => FILESYSTEM_READ_STREAM_OPERATION,
        FilesystemReadWireRequest::ListDirectory(_) => FILESYSTEM_LIST_DIRECTORY_OPERATION,
    }
}

fn encode_filesystem_read_wire(request: &FilesystemReadWireRequest) -> Result<Vec<u8>> {
    match request {
        FilesystemReadWireRequest::ReadFile(request) => request.encode(),
        FilesystemReadWireRequest::ReadFileStream(request) => request.encode(),
        FilesystemReadWireRequest::ListDirectory(request) => request.encode(),
    }
    .context("encode filesystem fixture request")
}

fn decode_filesystem_read_wire(
    operation: &str,
    payload: &[u8],
) -> Result<FilesystemReadWireRequest> {
    match operation {
        FILESYSTEM_READ_FILE_OPERATION => FilesystemReadFile::decode(payload)
            .map(FilesystemReadWireRequest::ReadFile)
            .context("decode filesystem read-file request"),
        FILESYSTEM_READ_STREAM_OPERATION => FilesystemReadStream::decode(payload)
            .map(FilesystemReadWireRequest::ReadFileStream)
            .context("decode filesystem read-file-stream request"),
        FILESYSTEM_LIST_DIRECTORY_OPERATION => FilesystemListDirectory::decode(payload)
            .map(FilesystemReadWireRequest::ListDirectory)
            .context("decode filesystem list-directory request"),
        _ => bail!("unknown filesystem read operation `{operation}`"),
    }
}

fn validate_filesystem_read_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    bindings: &GrantBindings,
    wire: &FilesystemReadWireRequest,
) -> Result<()> {
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::FilesystemReadV1,
        authorized_scope: permission.scope.clone(),
        bindings: bindings.clone(),
        activation: None,
        operation: filesystem_wire_operation(wire).into(),
        payload: encode_filesystem_read_wire(wire)?,
    };
    let validator = FilesystemReadBackend::new();
    let now = monotonic_micros().context("read monotonic clock for filesystem fixture")?;
    let result = match wire {
        FilesystemReadWireRequest::ReadFileStream(_) => {
            ResourceBackend::authorize(&validator, &request, &mut ActivationLedger::new(1), now)
        }
        FilesystemReadWireRequest::ReadFile(_) | FilesystemReadWireRequest::ListDirectory(_) => {
            Backend::authorize(&validator, &request, &mut ActivationLedger::new(1), now)
        }
    };
    result.map_err(|error| anyhow::anyhow!("filesystem fixture is not authorized: {error:?}"))
}

fn filesystem_read_result(
    result: &FilesystemReadResultFixture,
    request: &FilesystemReadWireRequest,
    permission: &CapabilityRequest,
) -> Result<PreparedFilesystemReadResult> {
    let CapabilityScope::FilesystemRead(scope) = &permission.scope else {
        bail!("filesystem.read.v1 permission has an inconsistent normalized scope");
    };
    match (result, request) {
        (FilesystemReadResultFixture::Error { code }, _) => Ok(
            PreparedFilesystemReadResult::Inline(BrokerResult::Error(broker_error(*code))),
        ),
        (
            FilesystemReadResultFixture::File { total_size, body },
            FilesystemReadWireRequest::ReadFile(read),
        ) => {
            let bytes = body.as_ref().map(byte_fixture).unwrap_or_default();
            validate_filesystem_file_result(
                *total_size,
                read.offset,
                read.maximum_bytes,
                bytes.len() as u64,
                scope.maximum_file_bytes,
                false,
            )?;
            let end_offset = read
                .offset
                .checked_add(bytes.len() as u64)
                .context("filesystem fixture end offset overflow")?;
            let payload = FilesystemFileChunk {
                offset: read.offset,
                total_size: *total_size,
                eof: end_offset >= *total_size,
                bytes,
            }
            .encode()
            .context("encode filesystem file fixture")?;
            Ok(PreparedFilesystemReadResult::Inline(
                BrokerResult::Success { payload },
            ))
        }
        (
            FilesystemReadResultFixture::Directory { entries, truncated },
            FilesystemReadWireRequest::ListDirectory(list),
        ) => {
            if entries.len() > usize::from(list.maximum_entries) {
                bail!(
                    "filesystem directory fixture has {} entries, exceeding the request maximum_entries limit of {}",
                    entries.len(),
                    list.maximum_entries
                );
            }
            let entries = filesystem_entries(entries, &scope.kinds)?;
            let payload = FilesystemDirectoryEntries {
                entries,
                truncated: *truncated,
            }
            .encode()
            .context("encode filesystem directory fixture")?;
            Ok(PreparedFilesystemReadResult::Inline(
                BrokerResult::Success { payload },
            ))
        }
        (
            FilesystemReadResultFixture::Stream {
                total_size,
                chunks,
                terminal_error,
            },
            FilesystemReadWireRequest::ReadFileStream(read),
        ) => {
            let chunks = chunks.iter().map(byte_fixture).collect::<Vec<_>>();
            let total_bytes = chunks.iter().try_fold(0_u64, |total, chunk| {
                total
                    .checked_add(chunk.len() as u64)
                    .context("filesystem stream fixture byte count overflow")
            })?;
            validate_filesystem_file_result(
                *total_size,
                read.offset,
                read.maximum_bytes,
                total_bytes,
                scope.maximum_file_bytes,
                terminal_error.is_none(),
            )?;
            let mut events = Vec::with_capacity(chunks.len() + 1);
            events.push(PreparedFilesystemEvent {
                payload: FilesystemStreamEvent::Metadata {
                    offset: read.offset,
                    total_size: *total_size,
                }
                .encode()
                .context("encode filesystem stream metadata fixture")?,
                kind: "filesystem-metadata",
            });
            let mut offset = read.offset;
            for bytes in chunks {
                let length = bytes.len() as u64;
                events.push(PreparedFilesystemEvent {
                    payload: FilesystemStreamEvent::Chunk { offset, bytes }
                        .encode()
                        .context("encode filesystem stream chunk fixture")?,
                    kind: "filesystem-chunk",
                });
                offset = offset
                    .checked_add(length)
                    .context("filesystem stream fixture offset overflow")?;
            }
            let (terminal, terminal_kind) = match terminal_error {
                Some(error) => (
                    BrokerResult::Error(broker_error(*error)),
                    "filesystem-error",
                ),
                None => (
                    BrokerResult::Success {
                        payload: FilesystemStreamEvent::Complete {
                            end_offset: offset,
                            total_bytes,
                            eof: offset >= *total_size,
                        }
                        .encode()
                        .context("encode filesystem stream completion fixture")?,
                    },
                    "filesystem-complete",
                ),
            };
            Ok(PreparedFilesystemReadResult::Stream {
                events,
                terminal,
                terminal_kind,
            })
        }
        _ => bail!("filesystem fixture response kind does not match the request operation"),
    }
}

fn validate_filesystem_file_result(
    total_size: u64,
    offset: u64,
    maximum_bytes: u64,
    response_bytes: u64,
    maximum_file_bytes: u64,
    require_complete_read: bool,
) -> Result<()> {
    if total_size > maximum_file_bytes {
        bail!(
            "filesystem fixture file size is {total_size} bytes, exceeding the manifest maximum_file_bytes limit of {maximum_file_bytes}"
        );
    }
    if response_bytes > maximum_bytes {
        bail!(
            "filesystem fixture returns {response_bytes} bytes, exceeding the request maximum_bytes limit of {maximum_bytes}"
        );
    }
    let available = total_size.saturating_sub(offset);
    if response_bytes > available {
        bail!("filesystem fixture returns bytes beyond its declared total_size");
    }
    if require_complete_read && response_bytes != maximum_bytes.min(available) {
        bail!(
            "successful filesystem stream fixture must return exactly the requested bytes or reach EOF"
        );
    }
    Ok(())
}

fn filesystem_entries(
    fixtures: &[FilesystemEntryFixture],
    allowed_kinds: &BTreeSet<FileKind>,
) -> Result<Vec<FilesystemEntry>> {
    let mut previous: Option<&str> = None;
    let mut entries = Vec::with_capacity(fixtures.len());
    for entry in fixtures {
        if entry.name.is_empty()
            || Path::new(&entry.name).components().count() != 1
            || !matches!(
                Path::new(&entry.name).components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            bail!("filesystem directory fixture entry names must be single relative components");
        }
        if previous.is_some_and(|previous| previous >= entry.name.as_str()) {
            bail!("filesystem directory fixture entries must be uniquely sorted by name");
        }
        previous = Some(&entry.name);
        let kind = match entry.kind {
            FilesystemEntryKindFixture::RegularFile => {
                if !allowed_kinds.contains(&FileKind::RegularFile) {
                    bail!("filesystem directory fixture contains an out-of-scope regular file");
                }
                FilesystemEntryKind::RegularFile
            }
            FilesystemEntryKindFixture::Directory => {
                if !allowed_kinds.contains(&FileKind::Directory) {
                    bail!("filesystem directory fixture contains an out-of-scope directory");
                }
                FilesystemEntryKind::Directory
            }
        };
        entries.push(FilesystemEntry {
            name: entry.name.clone(),
            kind,
            size: entry.size,
        });
    }
    Ok(entries)
}

fn validate_command_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    wire: &CommandRunRequest,
) -> Result<()> {
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::CommandRunV1,
        authorized_scope: permission.scope.clone(),
        bindings: GrantBindings::default(),
        activation: None,
        operation: COMMAND_RUN_OPERATION.into(),
        payload: wire.encode().context("encode command fixture request")?,
    };
    ResourceBackend::authorize(
        &CommandRunBackend::new(PathBuf::new()),
        &request,
        &mut ActivationLedger::new(1),
        monotonic_micros().context("read monotonic clock for command fixture")?,
    )
    .map_err(|error| anyhow::anyhow!("command fixture is not authorized: {error:?}"))
}

fn command_output_limit(permission: &CapabilityRequest, command_id: &str) -> Result<u64> {
    let CapabilityScope::CommandRun(scope) = &permission.scope else {
        bail!("command.run.v1 permission has an inconsistent normalized scope");
    };
    let mut rules = scope.commands.iter().filter(|rule| rule.id == command_id);
    let rule = rules
        .next()
        .with_context(|| format!("command fixture `{command_id}` is outside the manifest scope"))?;
    if rules.next().is_some() {
        bail!("command fixture `{command_id}` is ambiguous in the manifest scope");
    }
    Ok(rule.maximum_output_bytes)
}

fn command_request(request: &CommandRunFixtureRequest) -> CommandRunRequest {
    let mut values = request
        .values
        .iter()
        .map(|value| match value {
            CommandValueFixture::Integer { name, value } => CommandValue::Integer {
                name: name.clone(),
                value: *value,
            },
            CommandValueFixture::FixedEnum { name, value } => CommandValue::FixedEnum {
                name: name.clone(),
                value: value.clone(),
            },
            CommandValueFixture::Text { name, value } => CommandValue::Text {
                name: name.clone(),
                value: value.clone(),
            },
            CommandValueFixture::ApprovedFile { name, path } => CommandValue::ApprovedFile {
                name: name.clone(),
                path: path.clone(),
            },
            CommandValueFixture::Url { name, value } => CommandValue::Url {
                name: name.clone(),
                value: value.clone(),
            },
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| command_value_name(left).cmp(command_value_name(right)));
    CommandRunRequest {
        command_id: request.command_id.clone(),
        values,
    }
}

fn command_value_name(value: &CommandValue) -> &str {
    match value {
        CommandValue::Integer { name, .. }
        | CommandValue::FixedEnum { name, .. }
        | CommandValue::Text { name, .. }
        | CommandValue::ApprovedFile { name, .. }
        | CommandValue::Url { name, .. } => name,
    }
}

fn canonicalize_command_request(mut request: CommandRunRequest) -> CommandRunRequest {
    request
        .values
        .sort_by(|left, right| command_value_name(left).cmp(command_value_name(right)));
    request
}

fn command_result(
    result: &CommandResultFixture,
    maximum_output_bytes: u64,
) -> Result<PreparedCommandResult> {
    match result {
        CommandResultFixture::Error { code } => Ok(PreparedCommandResult::Inline(
            BrokerResult::Error(broker_error(*code)),
        )),
        CommandResultFixture::Completed {
            output,
            exit_code,
            signal,
        } => {
            let (events, stdout_bytes, stderr_bytes) =
                command_output(output, maximum_output_bytes)?;
            let terminal = CommandEvent::Exited {
                exit_code: *exit_code,
                signal: *signal,
                stdout_bytes,
                stderr_bytes,
            }
            .encode()
            .context("encode command exit fixture")?;
            Ok(PreparedCommandResult::Stream {
                events,
                terminal: BrokerResult::Success { payload: terminal },
                terminal_kind: "command-exited",
            })
        }
        CommandResultFixture::Failed { output, code } => {
            let (events, _, _) = command_output(output, maximum_output_bytes)?;
            Ok(PreparedCommandResult::Stream {
                events,
                terminal: BrokerResult::Error(broker_error(*code)),
                terminal_kind: "command-error",
            })
        }
    }
}

fn command_output(
    output: &[CommandOutputFixture],
    maximum_output_bytes: u64,
) -> Result<(Vec<PreparedCommandEvent>, u64, u64)> {
    let mut total_bytes = 0_u64;
    let mut stdout_bytes = 0_u64;
    let mut stderr_bytes = 0_u64;
    let mut events = Vec::with_capacity(output.len());
    for output in output {
        let (stream_bytes, event, kind) = match output {
            CommandOutputFixture::Stdout { body } => {
                let bytes = byte_fixture(body);
                let length = bytes.len() as u64;
                (length, CommandEvent::Stdout(bytes), "command-stdout")
            }
            CommandOutputFixture::Stderr { body } => {
                let bytes = byte_fixture(body);
                let length = bytes.len() as u64;
                (length, CommandEvent::Stderr(bytes), "command-stderr")
            }
        };
        total_bytes = total_bytes
            .checked_add(stream_bytes)
            .context("command fixture output byte count overflow")?;
        if total_bytes > maximum_output_bytes {
            bail!(
                "command fixture output is {total_bytes} bytes, exceeding the manifest maximum_output_bytes limit of {maximum_output_bytes}"
            );
        }
        match output {
            CommandOutputFixture::Stdout { .. } => stdout_bytes += stream_bytes,
            CommandOutputFixture::Stderr { .. } => stderr_bytes += stream_bytes,
        }
        events.push(PreparedCommandEvent {
            payload: event.encode().context("encode command output fixture")?,
            kind,
        });
    }
    Ok((events, stdout_bytes, stderr_bytes))
}

fn validate_http_authority(
    identity: &ConnectionIdentity,
    permission: &CapabilityRequest,
    operation: HttpOperationFixture,
    wire: &HttpRequest,
) -> Result<()> {
    let operation = http_operation(operation);
    let request = BackendRequest {
        identity: identity.clone(),
        request_id: 1,
        capability: CapabilityId::HttpRequestV1,
        authorized_scope: permission.scope.clone(),
        bindings: GrantBindings::default(),
        activation: None,
        operation: operation.into(),
        payload: wire.encode().context("encode HTTP request fixture")?,
    };
    let validator = HttpRequestBackend::with_parts(RejectingHttpResolver, RejectingHttpTransport);
    let result = match operation {
        HTTP_REQUEST_OPERATION => Backend::authorize(
            &validator,
            &request,
            &mut ActivationLedger::new(1),
            monotonic_micros().context("read monotonic clock for HTTP fixture")?,
        ),
        HTTP_STREAM_OPERATION => ResourceBackend::authorize(
            &validator,
            &request,
            &mut ActivationLedger::new(1),
            monotonic_micros().context("read monotonic clock for HTTP fixture")?,
        ),
        _ => unreachable!(),
    };
    result.map_err(|error| anyhow::anyhow!("HTTP fixture is not authorized: {error:?}"))
}

fn http_operation(operation: HttpOperationFixture) -> &'static str {
    match operation {
        HttpOperationFixture::Request => HTTP_REQUEST_OPERATION,
        HttpOperationFixture::RequestStream => HTTP_STREAM_OPERATION,
    }
}

fn http_request(request: &HttpRequestFixtureRequest) -> HttpRequest {
    HttpRequest {
        method: match request.method {
            HttpMethodFixture::Get => HttpRequestMethod::Get,
            HttpMethodFixture::Head => HttpRequestMethod::Head,
            HttpMethodFixture::Post => HttpRequestMethod::Post,
            HttpMethodFixture::Put => HttpRequestMethod::Put,
            HttpMethodFixture::Patch => HttpRequestMethod::Patch,
            HttpMethodFixture::Delete => HttpRequestMethod::Delete,
        },
        url: request.url.clone(),
        accept: request.accept.clone(),
        content_type: request.content_type.clone(),
        body: request.body.as_ref().map(byte_fixture).unwrap_or_default(),
    }
}

fn byte_fixture(body: &ByteFixture) -> Vec<u8> {
    match body {
        ByteFixture::Text(value) => value.as_bytes().to_vec(),
        ByteFixture::Bytes(value) => value.clone(),
    }
}

fn http_result(
    result: &HttpResultFixture,
    operation: HttpOperationFixture,
    request_url: &str,
    maximum_response_bytes: u64,
) -> Result<PreparedHttpResult> {
    match result {
        HttpResultFixture::Error { code } => Ok(PreparedHttpResult::Inline(BrokerResult::Error(
            broker_error(*code),
        ))),
        HttpResultFixture::Response {
            status,
            final_url,
            content_type,
            etag,
            body,
        } => {
            if operation != HttpOperationFixture::Request {
                bail!("HTTP `response` fixture requires operation `request`");
            }
            let final_url = exact_http_final_url(final_url.as_deref(), request_url)?;
            let body = body.as_ref().map(byte_fixture).unwrap_or_default();
            validate_http_fixture_response_size(body.len() as u64, maximum_response_bytes)?;
            let payload = HttpResponse {
                status: *status,
                final_url,
                content_type: content_type.clone(),
                etag: etag.clone(),
                body,
            }
            .encode()
            .context("encode HTTP response fixture")?;
            Ok(PreparedHttpResult::Inline(BrokerResult::Success {
                payload,
            }))
        }
        HttpResultFixture::Stream {
            status,
            final_url,
            content_type,
            etag,
            chunks,
            terminal_error,
        } => {
            if operation != HttpOperationFixture::RequestStream {
                bail!("HTTP `stream` fixture requires operation `request-stream`");
            }
            let final_url = exact_http_final_url(final_url.as_deref(), request_url)?;
            let metadata = HttpStreamEvent::Metadata {
                status: *status,
                final_url,
                content_type: content_type.clone(),
                etag: etag.clone(),
            }
            .encode()
            .context("encode HTTP stream metadata fixture")?;
            let mut total_bytes = 0_u64;
            let chunks = chunks
                .iter()
                .map(|chunk| {
                    let chunk = byte_fixture(chunk);
                    total_bytes = total_bytes
                        .checked_add(chunk.len() as u64)
                        .context("HTTP stream fixture byte count overflow")?;
                    validate_http_fixture_response_size(total_bytes, maximum_response_bytes)?;
                    HttpStreamEvent::Chunk(chunk)
                        .encode()
                        .context("encode HTTP stream chunk fixture")
                })
                .collect::<Result<Vec<_>>>()?;
            let terminal = match terminal_error {
                Some(error) => BrokerResult::Error(broker_error(*error)),
                None => BrokerResult::Success {
                    payload: HttpStreamEvent::Complete { total_bytes }
                        .encode()
                        .context("encode HTTP stream completion fixture")?,
                },
            };
            Ok(PreparedHttpResult::Stream {
                metadata,
                chunks,
                terminal,
            })
        }
    }
}

fn validate_http_fixture_response_size(
    actual_bytes: u64,
    maximum_response_bytes: u64,
) -> Result<()> {
    if actual_bytes > maximum_response_bytes {
        bail!(
            "HTTP fixture response is {actual_bytes} bytes, exceeding the manifest maximum_response_bytes limit of {maximum_response_bytes}"
        );
    }
    Ok(())
}

fn exact_http_final_url(final_url: Option<&str>, request_url: &str) -> Result<String> {
    let final_url = final_url.unwrap_or(request_url);
    if final_url != request_url {
        bail!("HTTP replay v1 does not simulate redirects; final_url must equal the request URL");
    }
    Ok(final_url.to_owned())
}

#[derive(Clone, Copy)]
struct RejectingDbusTransport;

impl DbusTransport for RejectingDbusTransport {
    fn call(
        &self,
        _call: &DbusCall,
        _allow_service_activation: bool,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<DbusReply, BrokerErrorCode> {
        Err(BrokerErrorCode::Unavailable)
    }
}

impl DbusSubscriptionTransport for RejectingDbusTransport {
    fn open(
        &self,
        _subscription: &DbusSubscription,
        _events: ResourceEventSink,
    ) -> std::result::Result<Box<dyn ResourceHandle>, BrokerErrorCode> {
        Err(BrokerErrorCode::Unavailable)
    }
}

#[derive(Clone, Copy)]
struct RejectingHttpResolver;

impl HttpResolver for RejectingHttpResolver {
    fn resolve(
        &self,
        _host: &str,
        _port: u16,
    ) -> std::result::Result<Vec<std::net::SocketAddr>, BrokerErrorCode> {
        Err(BrokerErrorCode::Unavailable)
    }
}

#[derive(Clone, Copy)]
struct RejectingHttpTransport;

impl HttpTransport for RejectingHttpTransport {
    fn send(
        &self,
        _request: &HttpRequest,
        _url: &url::Url,
        _addresses: &[std::net::SocketAddr],
        _maximum_response_bytes: u64,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<HttpTransportResponse, BrokerErrorCode> {
        Err(BrokerErrorCode::Unavailable)
    }
}

#[derive(Clone, Copy)]
struct RejectingUriTransport;

impl UriOpenTransport for RejectingUriTransport {
    fn open(
        &self,
        _uri: &str,
        _cancellation: &CancellationToken,
    ) -> std::result::Result<(), BrokerErrorCode> {
        Err(BrokerErrorCode::Unavailable)
    }
}

fn validate_fixture_id(id: &str, ids: &mut BTreeSet<String>) -> Result<()> {
    validate_snapshot_name(id).context("invalid broker fixture ID")?;
    if !ids.insert(id.to_owned()) {
        bail!("duplicate broker fixture ID `{id}`");
    }
    Ok(())
}

fn dbus_call(request: &DbusCallFixtureRequest) -> DbusCall {
    DbusCall {
        bus: dbus_bus(request.bus),
        destination: request.destination.clone(),
        path: request.path.clone(),
        interface: request.interface.clone(),
        member: request.member.clone(),
        arguments: request.arguments.clone(),
        reply: match request.reply {
            DbusReplyFixtureKind::Unit => DbusReplyKind::Unit,
            DbusReplyFixtureKind::VariantString => DbusReplyKind::VariantString,
            DbusReplyFixtureKind::VariantI64 => DbusReplyKind::VariantI64,
        },
    }
}

fn dbus_subscription(request: &DbusSubscriptionFixtureRequest) -> DbusSubscription {
    DbusSubscription {
        bus: dbus_bus(request.bus),
        sender: request.sender.clone(),
        path: request.path.clone(),
        interface: request.interface.clone(),
        member: request.member.clone(),
        signature: request.signature.clone(),
        argument_zero: request.argument_zero.clone(),
    }
}

fn dbus_bus(bus: DbusBusFixture) -> DbusBus {
    match bus {
        DbusBusFixture::Session => DbusBus::Session,
        DbusBusFixture::System => DbusBus::System,
    }
}

fn dbus_result(fixture: &DbusResultFixture, expected: DbusReplyKind) -> Result<BrokerResult> {
    let reply = match fixture {
        DbusResultFixture::Unit if expected == DbusReplyKind::Unit => DbusReply::Unit,
        DbusResultFixture::String { value } if expected == DbusReplyKind::VariantString => {
            DbusReply::String(value.clone())
        }
        DbusResultFixture::I64 { value } if expected == DbusReplyKind::VariantI64 => {
            DbusReply::I64(*value)
        }
        DbusResultFixture::Error { code } => {
            return Ok(BrokerResult::Error(broker_error(*code)));
        }
        _ => bail!("D-Bus fixture response does not match the request reply kind"),
    };
    Ok(BrokerResult::Success {
        payload: reply.encode().context("encode D-Bus fixture response")?,
    })
}

fn dbus_signal(fixture: &DbusSignalFixture) -> DbusPropertiesChanged {
    DbusPropertiesChanged {
        interface_name: fixture.interface_name.clone(),
        changed_properties: fixture
            .changed_properties
            .iter()
            .map(|property| DbusProperty {
                name: property.name.clone(),
                value: dbus_value(&property.value),
            })
            .collect(),
        invalidated_properties: fixture.invalidated_properties.clone(),
    }
}

fn dbus_value(value: &DbusValueFixture) -> DbusValue {
    match value {
        DbusValueFixture::U8(value) => DbusValue::U8(*value),
        DbusValueFixture::Bool(value) => DbusValue::Bool(*value),
        DbusValueFixture::I16(value) => DbusValue::I16(*value),
        DbusValueFixture::U16(value) => DbusValue::U16(*value),
        DbusValueFixture::I32(value) => DbusValue::I32(*value),
        DbusValueFixture::U32(value) => DbusValue::U32(*value),
        DbusValueFixture::I64(value) => DbusValue::I64(*value),
        DbusValueFixture::U64(value) => DbusValue::U64(*value),
        DbusValueFixture::F64(value) => DbusValue::F64(*value),
        DbusValueFixture::String(value) => DbusValue::String(value.clone()),
    }
}

fn broker_error(error: BrokerErrorFixture) -> BrokerErrorCode {
    match error {
        BrokerErrorFixture::Unavailable => BrokerErrorCode::Unavailable,
        BrokerErrorFixture::Denied => BrokerErrorCode::Denied,
        BrokerErrorFixture::OutOfScope => BrokerErrorCode::OutOfScope,
        BrokerErrorFixture::InvalidRequest => BrokerErrorCode::InvalidRequest,
        BrokerErrorFixture::InvalidPhase => BrokerErrorCode::InvalidPhase,
        BrokerErrorFixture::ActivationRequired => BrokerErrorCode::ActivationRequired,
        BrokerErrorFixture::QuotaExceeded => BrokerErrorCode::QuotaExceeded,
        BrokerErrorFixture::RateLimited => BrokerErrorCode::RateLimited,
        BrokerErrorFixture::Timeout => BrokerErrorCode::Timeout,
        BrokerErrorFixture::Cancelled => BrokerErrorCode::Cancelled,
        BrokerErrorFixture::Unsupported => BrokerErrorCode::Unsupported,
        BrokerErrorFixture::BackendFailed => BrokerErrorCode::BackendFailed,
        BrokerErrorFixture::Internal => BrokerErrorCode::Internal,
    }
}

fn broker_result_name(result: &BrokerResult) -> &'static str {
    match result {
        BrokerResult::Success { .. } => "success",
        BrokerResult::Error(BrokerErrorCode::Unavailable) => "unavailable",
        BrokerResult::Error(BrokerErrorCode::Denied) => "denied",
        BrokerResult::Error(BrokerErrorCode::OutOfScope) => "out-of-scope",
        BrokerResult::Error(BrokerErrorCode::InvalidRequest) => "invalid-request",
        BrokerResult::Error(BrokerErrorCode::InvalidPhase) => "invalid-phase",
        BrokerResult::Error(BrokerErrorCode::ActivationRequired) => "activation-required",
        BrokerResult::Error(BrokerErrorCode::QuotaExceeded) => "quota-exceeded",
        BrokerResult::Error(BrokerErrorCode::RateLimited) => "rate-limited",
        BrokerResult::Error(BrokerErrorCode::Timeout) => "timeout",
        BrokerResult::Error(BrokerErrorCode::Cancelled) => "cancelled",
        BrokerResult::Error(BrokerErrorCode::Unsupported) => "unsupported",
        BrokerResult::Error(BrokerErrorCode::BackendFailed) => "backend-failed",
        BrokerResult::Error(BrokerErrorCode::Internal) => "internal",
    }
}

fn context_snapshot(
    generation: u64,
    facts: &BTreeMap<String, ContextFactValue>,
    requested: &BTreeSet<String>,
) -> ContextSnapshot {
    ContextSnapshot {
        generation,
        facts: requested
            .iter()
            .filter_map(|key| {
                facts.get(key).cloned().map(|value| ContextFact {
                    key: key.clone(),
                    value,
                })
            })
            .collect(),
    }
}

fn validate_context_facts(
    facts: &BTreeMap<String, ContextFixtureValue>,
    allowed: &BTreeSet<String>,
) -> Result<()> {
    for (key, value) in facts {
        if !matches!(key.as_str(), "application.id" | "workspace.id") {
            bail!("unsupported replay context fact `{key}`");
        }
        if !allowed.contains(key) {
            bail!("replay context fact `{key}` is outside the manifest permission scope");
        }
        if let ContextFixtureValue::Text(value) = value
            && (value.is_empty()
                || value.len() > MAX_CONTEXT_VALUE_BYTES
                || value.chars().any(char::is_control))
        {
            bail!("replay context fact `{key}` has an invalid text value");
        }
    }
    Ok(())
}

fn context_value(value: &ContextFixtureValue) -> ContextFactValue {
    match value {
        ContextFixtureValue::Text(value) => ContextFactValue::Text(value.clone()),
        ContextFixtureValue::Boolean(value) => ContextFactValue::Boolean(*value),
    }
}

pub fn run(
    host: PluginHost,
    items: &[HostedItem],
    scenario: Scenario,
    broker: Option<ReplayBrokerController>,
    screenshot_directory: Option<&Path>,
) -> Result<()> {
    validate_scenario(&scenario, items)?;
    let screenshot_directory = screenshot_directory
        .map(|directory| {
            let directory = directory
                .canonicalize()
                .with_context(|| format!("open screenshot directory {}", directory.display()))?;
            if !directory.is_dir() {
                bail!("screenshot output must be a directory");
            }
            Ok(directory)
        })
        .transpose()?;
    let requested_screenshots = scenario
        .steps
        .iter()
        .filter(|step| matches!(step, Step::Snapshot { .. }))
        .count();
    if requested_screenshots > MAX_RASTER_SNAPSHOTS {
        bail!("a replay may request at most {MAX_RASTER_SNAPSHOTS} raster snapshots");
    }

    let mut host = host;
    let appearance = apply_appearance(Appearance::default(), &scenario.appearance, false)?;
    let viewport = Rect::new(0.0, 0.0, scenario.width as f32, 60.0);
    let ui = host.render_at(&scenario.item, viewport, appearance, Duration::ZERO)?;
    let rasterizer = screenshot_directory
        .as_ref()
        .map(|_| crate::replay_gpu::Rasterizer::new(scenario.width, 60, appearance.motion))
        .transpose()?;
    let step_count = scenario.steps.len();
    let mut harness = Harness {
        host,
        item: scenario.item.clone(),
        width: scenario.width,
        viewport,
        appearance,
        now: Duration::ZERO,
        ui,
        interactions: InteractionState::default(),
        interaction_map: InteractionMap::default(),
        events: Vec::new(),
        presentation_commands: Vec::new(),
        snapshots: Vec::new(),
        snapshot_names: BTreeSet::new(),
        broker,
        screenshot_directory,
        rasterizer,
    };
    harness.snapshot("initial".into(), None, false)?;

    for (index, step) in scenario.steps.into_iter().enumerate() {
        let number = index + 1;
        let (snapshot_name, raster) = match step {
            Step::Touch {
                contact_id,
                phase,
                x,
                y,
                time_ms,
            } => {
                harness.set_time(time_ms)?;
                validate_coordinate(x, "touch x")?;
                validate_coordinate(y, "touch y")?;
                let phase = match phase {
                    ContactFixturePhase::Down => ContactPhase::Down,
                    ContactFixturePhase::Motion => ContactPhase::Motion,
                    ContactFixturePhase::Up => ContactPhase::Up,
                    ContactFixturePhase::Cancel => ContactPhase::Cancel,
                };
                let events = harness.interactions.handle(
                    &harness.interaction_map,
                    Contact {
                        id: contact_id,
                        phase,
                        position: Point::new(x, y),
                        time: harness.now,
                    },
                );
                harness.dispatch(number, contact_id, events)?;
                (format!("step-{number}"), false)
            }
            Step::Advance { time_ms } => {
                harness.set_time(time_ms)?;
                let events = harness.interactions.tick(harness.now);
                harness.dispatch(number, 0, events)?;
                (format!("step-{number}"), false)
            }
            Step::Appearance { appearance } => {
                harness.appearance = apply_appearance(harness.appearance, &appearance, true)?;
                if let Some(rasterizer) = &harness.rasterizer {
                    rasterizer.set_motion_policy(harness.appearance.motion);
                }
                harness.rebuild()?;
                (format!("step-{number}"), false)
            }
            Step::Presentation { event } => {
                let update = harness
                    .host
                    .handle_presentation_event(to_presentation_event(event)?)?;
                let mut rerender = update.rerender;
                harness.record_presentation(number, update.presentation);
                rerender |= harness.drain_broker()?;
                if rerender {
                    harness.rebuild()?;
                }
                (format!("step-{number}"), false)
            }
            Step::Context { facts } => {
                let broker = harness
                    .broker
                    .as_ref()
                    .context("context step requires a replay context fixture")?;
                broker.publish_context(&facts)?;
                if harness.drain_broker()? {
                    harness.rebuild()?;
                }
                (format!("step-{number}"), false)
            }
            Step::DbusSignal {
                subscription,
                event,
            } => {
                let broker = harness
                    .broker
                    .as_ref()
                    .context("D-Bus signal step requires replay broker fixtures")?;
                broker.publish_dbus_signal(&subscription, &event)?;
                if harness.drain_broker()? {
                    harness.rebuild()?;
                }
                (format!("step-{number}"), false)
            }
            Step::Snapshot { name } => {
                validate_snapshot_name(&name)?;
                (name, true)
            }
        };
        harness.snapshot(snapshot_name, Some(number), raster)?;
    }

    let screenshot_renderer = harness
        .rasterizer
        .as_ref()
        .map(|rasterizer| rasterizer.renderer_name().to_owned());
    let broker = harness
        .broker
        .as_ref()
        .map(ReplayBrokerController::finish)
        .transpose()?;
    let report = ReplayReport {
        report_version: 1,
        ok: true,
        item: harness.item,
        width: harness.width,
        steps: step_count,
        guest_render_calls: harness.host.guest_render_calls(),
        screenshot_renderer,
        events: harness.events,
        presentation_commands: harness.presentation_commands,
        broker,
        snapshots: harness.snapshots,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

impl Harness {
    fn set_time(&mut self, time_ms: u64) -> Result<()> {
        if time_ms > MAX_TIME_MS {
            bail!("replay time must not exceed {MAX_TIME_MS} milliseconds");
        }
        let next = Duration::from_millis(time_ms);
        if next < self.now {
            bail!("replay time must be monotonic");
        }
        if let Some(broker) = &self.broker {
            broker.set_time(next)?;
        }
        self.now = next;
        Ok(())
    }

    fn rebuild(&mut self) -> Result<()> {
        self.ui = self
            .host
            .render_at(&self.item, self.viewport, self.appearance, self.now)?;
        Ok(())
    }

    fn dispatch(&mut self, step: usize, fallback_contact: u32, events: Vec<UiEvent>) -> Result<()> {
        let mut rerender = false;
        for event in events {
            let (widget_id, kind, value, contact_id) = match event {
                UiEvent::Pressed { id } => (id.0, InputKind::Pressed, None, fallback_contact),
                UiEvent::Activated { id } => (id.0, InputKind::Activated, None, fallback_contact),
                UiEvent::LongPressed { id, contact } => {
                    (id.0, InputKind::LongPressed, None, contact)
                }
                UiEvent::ValueChanged { id, value } => {
                    (id.0, InputKind::ValueChanged, Some(value), fallback_contact)
                }
                UiEvent::Released { id } => (id.0, InputKind::Released, None, fallback_contact),
                UiEvent::Cancelled { id } => (id.0, InputKind::Cancelled, None, fallback_contact),
            };
            self.events.push(EventReport {
                step,
                time_ms: self.now.as_millis() as u64,
                contact_id,
                widget_id,
                kind: input_kind_name(kind),
                value,
            });
            let update = self.host.handle_event(&InputEvent {
                item_id: self.item.clone(),
                widget_id,
                kind,
                value,
                contact_id: Some(contact_id),
                activation: (kind == InputKind::Activated).then_some(InputActivation {
                    origin: ActivationOrigin::Physical,
                    input_sequence: step as u64,
                }),
            })?;
            rerender |= update.rerender;
            self.record_presentation(step, update.presentation);
        }
        rerender |= self.drain_broker()?;
        if rerender {
            self.rebuild()?;
        }
        Ok(())
    }

    fn drain_broker(&mut self) -> Result<bool> {
        if self.broker.is_none() {
            return Ok(false);
        }
        let descriptor = self
            .host
            .broker_event_fd()
            .context("replay broker is unavailable")?;
        let mut rerender = false;
        let mut timeout = 100;
        loop {
            let mut poll_descriptor = libc::pollfd {
                fd: descriptor,
                events: libc::POLLIN,
                revents: 0,
            };
            // The fake broker executes in a separate thread so it exercises
            // the real sequenced-packet ABI. After the first response, wait
            // through one short quiet period so an immediately produced
            // resource stream cannot race the following snapshot.
            let ready = unsafe { libc::poll(&mut poll_descriptor, 1, timeout) };
            if ready < 0 {
                return Err(std::io::Error::last_os_error()).context("wait for replay broker");
            }
            if ready == 0 {
                return Ok(rerender);
            }
            rerender |= self.host.dispatch_broker_events()?;
            timeout = 5;
        }
    }

    fn record_presentation(&mut self, step: usize, command: Option<ComponentPresentationCommand>) {
        let Some(command) = command else {
            return;
        };
        self.presentation_commands.push(match command {
            ComponentPresentationCommand::Begin {
                placement,
                lifecycle,
            } => {
                let (placement, target) = match placement {
                    ComponentPresentationPlacement::Anchored => ("anchored", None),
                    ComponentPresentationPlacement::InPlace => ("in-place", None),
                    ComponentPresentationPlacement::Slot(target) => ("slot", Some(target)),
                    ComponentPresentationPlacement::Region(target) => ("region", Some(target)),
                    ComponentPresentationPlacement::FullBar => ("full-bar", None),
                };
                PresentationCommandReport::Begin {
                    step,
                    placement,
                    target,
                    lifecycle: match lifecycle {
                        ComponentPresentationLifecycle::Persistent => "persistent",
                        ComponentPresentationLifecycle::Transient => "transient",
                    },
                }
            }
            ComponentPresentationCommand::End(reason) => PresentationCommandReport::End {
                step,
                reason: match reason {
                    ComponentPresentationDismissal::Requested => "requested",
                    ComponentPresentationDismissal::Selection => "selection",
                    ComponentPresentationDismissal::Timeout => "timeout",
                },
            },
        });
    }

    fn snapshot(&mut self, name: String, step: Option<usize>, raster: bool) -> Result<()> {
        if !self.snapshot_names.insert(name.clone()) {
            bail!("duplicate replay snapshot name `{name}`");
        }
        let resolved = self.ui.resolve(self.viewport, self.appearance.theme);
        self.interaction_map = resolved.interactions;
        let screenshot = match (
            raster,
            self.screenshot_directory.as_ref(),
            self.rasterizer.as_mut(),
        ) {
            (true, Some(directory), Some(rasterizer)) => {
                let file_name = format!("{name}.png");
                rasterizer.write_png(&resolved.scene, self.now, &directory.join(&file_name))?;
                Some(file_name)
            }
            _ => None,
        };
        let mut primitive_kinds = BTreeMap::new();
        for primitive in &resolved.scene.primitives {
            *primitive_kinds
                .entry(primitive_name(primitive))
                .or_insert(0) += 1;
        }
        self.snapshots.push(SnapshotReport {
            name,
            step,
            time_ms: self.now.as_millis() as u64,
            screenshot,
            appearance: appearance_report(self.appearance),
            primitive_count: resolved.scene.primitives.len(),
            primitive_kinds,
            representations: resolved
                .inspector
                .representations
                .into_iter()
                .map(|(id, representation)| (id.0.to_string(), representation_name(representation)))
                .collect(),
            semantics: semantic_report(resolved.inspector.semantics),
        });
        Ok(())
    }
}

fn read_scenario(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_SCENARIO_BYTES {
        bail!("replay scenario must be a regular file no larger than {MAX_SCENARIO_BYTES} bytes");
    }
    fs::read(path).with_context(|| format!("read {}", path.display()))
}

fn validate_scenario(scenario: &Scenario, items: &[HostedItem]) -> Result<()> {
    if scenario.version != 1 {
        bail!(
            "unsupported replay scenario version {}; expected 1",
            scenario.version
        );
    }
    if !(1..=touchbar_package::MAX_TOUCHBAR_WIDTH).contains(&scenario.width) {
        bail!(
            "replay width must be between 1 and {}",
            touchbar_package::MAX_TOUCHBAR_WIDTH
        );
    }
    if !items.iter().any(|item| item.id == scenario.item) {
        bail!("component does not export replay item {}", scenario.item);
    }
    if scenario.steps.len() > MAX_STEPS {
        bail!("replay scenario may contain at most {MAX_STEPS} steps");
    }
    Ok(())
}

fn validate_coordinate(value: f32, label: &str) -> Result<()> {
    if !value.is_finite() || !(-4096.0..=8192.0).contains(&value) {
        bail!("{label} must be finite and between -4096 and 8192");
    }
    Ok(())
}

fn validate_snapshot_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("snapshot name must be 1-64 ASCII letters, digits, dashes, or underscores");
    }
    Ok(())
}

fn apply_appearance(
    current: Appearance,
    fixture: &AppearanceFixture,
    update: bool,
) -> Result<Appearance> {
    let (mut scheme, mut theme) = match fixture.preset {
        Some(PalettePreset::Dark) => (ColorScheme::Dark, Theme::default()),
        Some(PalettePreset::Light) => (ColorScheme::Light, light_theme()),
        None => (current.scheme, current.theme),
    };
    if fixture.preset.is_none() {
        scheme = current.scheme;
    }
    macro_rules! color {
        ($field:ident) => {
            if let Some(value) = &fixture.colors.$field {
                theme.$field = parse_color(value)
                    .with_context(|| format!("parse appearance color {}", stringify!($field)))?;
            }
        };
    }
    color!(background);
    color!(control);
    color!(control_pressed);
    color!(accent);
    color!(track);
    color!(foreground);
    color!(muted);
    color!(destructive);
    if let Some(radius) = fixture.colors.corner_radius {
        if !radius.is_finite() || !(0.0..=30.0).contains(&radius) {
            bail!("appearance corner_radius must be finite and between 0 and 30");
        }
        theme.corner_radius = radius;
    }
    Ok(Appearance {
        revision: fixture.revision.unwrap_or_else(|| {
            if update {
                current.revision.saturating_add(1)
            } else {
                current.revision
            }
        }),
        scheme,
        motion: fixture
            .motion
            .map_or(current.motion, |motion| match motion {
                MotionFixture::Full => MotionPolicy::Full,
                MotionFixture::Reduced => MotionPolicy::Reduced,
                MotionFixture::Disabled => MotionPolicy::Disabled,
            }),
        theme,
    })
}

fn light_theme() -> Theme {
    Theme {
        background: Color::rgb(0.94, 0.94, 0.96),
        control: Color::rgb(0.82, 0.82, 0.86),
        control_pressed: Color::rgb(0.70, 0.70, 0.76),
        accent: Color::rgb(0.16, 0.42, 0.95),
        track: Color::rgb(0.72, 0.72, 0.77),
        foreground: Color::rgb(0.04, 0.04, 0.06),
        muted: Color::rgb(0.34, 0.34, 0.39),
        destructive: Color::rgb(0.82, 0.12, 0.16),
        corner_radius: 9.0,
    }
}

fn parse_color(value: &str) -> Result<Color> {
    let digits = value.strip_prefix('#').context("color must begin with #")?;
    if digits.len() != 6 && digits.len() != 8 {
        bail!("color must use #RRGGBB or #RRGGBBAA");
    }
    let component = |start| {
        u8::from_str_radix(&digits[start..start + 2], 16)
            .context("color contains a non-hexadecimal component")
    };
    let red = component(0)?;
    let green = component(2)?;
    let blue = component(4)?;
    let alpha = if digits.len() == 8 {
        component(6)?
    } else {
        255
    };
    Ok(Color::rgba(
        f32::from(red) / 255.0,
        f32::from(green) / 255.0,
        f32::from(blue) / 255.0,
        f32::from(alpha) / 255.0,
    ))
}

fn to_presentation_event(event: PresentationFixtureEvent) -> Result<ComponentPresentationEvent> {
    Ok(match event {
        PresentationFixtureEvent::Anchor { x, width } => {
            validate_coordinate(x, "presentation anchor x")?;
            let maximum = touchbar_package::MAX_TOUCHBAR_WIDTH as f32;
            if !width.is_finite() || width <= 0.0 || width > maximum {
                bail!("presentation anchor width must be finite and between 0 and {maximum}");
            }
            ComponentPresentationEvent::Anchor { x, width }
        }
        PresentationFixtureEvent::Started => ComponentPresentationEvent::Started,
        PresentationFixtureEvent::Ended { reason } => {
            ComponentPresentationEvent::Ended(match reason {
                PresentationFixtureEndReason::Requested => {
                    ComponentPresentationEndReason::Requested
                }
                PresentationFixtureEndReason::Selection => {
                    ComponentPresentationEndReason::Selection
                }
                PresentationFixtureEndReason::OutsidePress => {
                    ComponentPresentationEndReason::OutsidePress
                }
                PresentationFixtureEndReason::Timeout => ComponentPresentationEndReason::Timeout,
                PresentationFixtureEndReason::SourceHidden => {
                    ComponentPresentationEndReason::SourceHidden
                }
                PresentationFixtureEndReason::Replaced => ComponentPresentationEndReason::Replaced,
                PresentationFixtureEndReason::Rejected => ComponentPresentationEndReason::Rejected,
            })
        }
    })
}

fn input_kind_name(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Pressed => "pressed",
        InputKind::Activated => "activated",
        InputKind::LongPressed => "long-pressed",
        InputKind::ValueChanged => "value-changed",
        InputKind::Released => "released",
        InputKind::Cancelled => "cancelled",
    }
}

fn representation_name(representation: Representation) -> &'static str {
    match representation {
        Representation::Hidden => "hidden",
        Representation::Minimal => "minimal",
        Representation::Compact => "compact",
        Representation::Full => "full",
    }
}

fn primitive_name(primitive: &Primitive) -> &'static str {
    match primitive {
        Primitive::RoundedRect { .. } => "rounded-rect",
        Primitive::Text { .. } => "text",
        Primitive::Icon { .. } => "icon",
        Primitive::Image { .. } => "image",
        Primitive::PushOpacity { .. } => "push-opacity",
        Primitive::PopOpacity => "pop-opacity",
        Primitive::PushMotion { .. } => "push-motion",
        Primitive::PopMotion => "pop-motion",
        Primitive::CustomGles { .. } => "custom-gles",
        Primitive::ShaderEffect { .. } => "shader-effect",
        Primitive::Line { .. } => "line",
        Primitive::LinearGradientRect { .. } => "linear-gradient-rect",
        Primitive::TriangleMesh { .. } => "triangle-mesh",
        Primitive::PushClip { .. } => "push-clip",
        Primitive::PopClip => "pop-clip",
    }
}

fn appearance_report(appearance: Appearance) -> AppearanceReport {
    AppearanceReport {
        revision: appearance.revision,
        scheme: match appearance.scheme {
            ColorScheme::Dark => "dark",
            ColorScheme::Light => "light",
        },
        motion: match appearance.motion {
            MotionPolicy::Full => "full",
            MotionPolicy::Reduced => "reduced",
            MotionPolicy::Disabled => "disabled",
        },
        background: color_string(appearance.theme.background),
        control: color_string(appearance.theme.control),
        control_pressed: color_string(appearance.theme.control_pressed),
        accent: color_string(appearance.theme.accent),
        track: color_string(appearance.theme.track),
        foreground: color_string(appearance.theme.foreground),
        muted: color_string(appearance.theme.muted),
        destructive: color_string(appearance.theme.destructive),
        corner_radius: appearance.theme.corner_radius,
    }
}

fn color_string(color: Color) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        channel(color.red),
        channel(color.green),
        channel(color.blue),
        channel(color.alpha)
    )
}

fn semantic_report(node: SemanticNode) -> SemanticReport {
    SemanticReport {
        widget_id: node.id.map(|id| id.0),
        role: semantic_role_name(node.role),
        label: node.label,
        value: node.value,
        hint: node.hint,
        bounds: [
            node.bounds.x,
            node.bounds.y,
            node.bounds.width,
            node.bounds.height,
        ],
        enabled: node.enabled,
        selected: node.selected,
        children: node.children.into_iter().map(semantic_report).collect(),
    }
}

fn semantic_role_name(role: SemanticRole) -> &'static str {
    match role {
        SemanticRole::Group => "group",
        SemanticRole::Label => "label",
        SemanticRole::Image => "image",
        SemanticRole::Button => "button",
        SemanticRole::Toggle => "toggle",
        SemanticRole::Slider => "slider",
        SemanticRole::Progress => "progress",
        SemanticRole::Canvas => "canvas",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_are_strict_and_canonical() {
        assert_eq!(color_string(parse_color("#12aBcD80").unwrap()), "#12abcd80");
        for invalid in ["12abcd", "#abc", "#abcdefgh", "#0000000000"] {
            assert!(parse_color(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn appearance_updates_preserve_unspecified_roles() {
        let current = Appearance::default();
        let fixture = AppearanceFixture {
            colors: PaletteFixture {
                accent: Some("#336699".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let updated = apply_appearance(current, &fixture, true).unwrap();
        assert_eq!(updated.revision, current.revision + 1);
        assert_eq!(updated.theme.background, current.theme.background);
        assert_eq!(color_string(updated.theme.accent), "#336699ff");
    }

    #[test]
    fn context_fixture_is_limited_to_the_manifest_scope() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/component-plugin/tests/context-replay.json"
        ))
        .unwrap();
        scenario.context.insert(
            "workspace.id".into(),
            ContextFixtureValue::Text("outside-scope".into()),
        );
        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope fixture was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("outside the manifest permission scope")
        );
    }

    #[test]
    fn dbus_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/broker-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/broker-component-plugin/tests/replay.json"
        ))
        .unwrap();
        scenario.broker.dbus_calls[0].request.destination = "org.example.Escape".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope D-Bus fixture was accepted");
        };

        assert!(error.to_string().contains("not authorized: OutOfScope"));
    }

    #[test]
    fn dbus_fixture_reply_must_match_the_declared_wire_type() {
        let result = dbus_result(
            &DbusResultFixture::I64 { value: 4 },
            DbusReplyKind::VariantString,
        );

        assert!(result.is_err());
    }

    #[test]
    fn command_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../plugins/command-deck/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../plugins/command-deck/tests/replay.json"
        ))
        .unwrap();
        scenario.broker.command_runs[0].request.command_id = "undeclared-command".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope command fixture was accepted");
        };

        assert!(error.to_string().contains("not authorized: OutOfScope"));
    }

    #[test]
    fn command_fixture_output_is_limited_by_manifest_bytes() {
        let response = CommandResultFixture::Completed {
            output: vec![CommandOutputFixture::Stdout {
                body: ByteFixture::Text("12345".into()),
            }],
            exit_code: Some(0),
            signal: None,
        };
        let error = command_result(&response, 4)
            .err()
            .expect("oversized command output fixture should fail");

        assert!(error.to_string().contains("maximum_output_bytes"));
    }

    #[test]
    fn command_fixture_exit_shape_uses_the_production_schema() {
        let response = CommandResultFixture::Completed {
            output: Vec::new(),
            exit_code: None,
            signal: None,
        };
        let error = command_result(&response, 1024)
            .err()
            .expect("command completion without exit code or signal should fail");

        assert!(error.to_string().contains("encode command exit fixture"));
    }

    #[test]
    fn filesystem_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/filesystem-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/filesystem-component-plugin/tests/replay.json"
        ))
        .unwrap();
        let FilesystemReadRequestFixture::List(request) =
            &mut scenario.broker.filesystem_reads[0].request
        else {
            panic!("expected list fixture");
        };
        request.path = "../escape".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope filesystem fixture was accepted");
        };

        assert!(error.to_string().contains("not authorized: OutOfScope"));
    }

    #[test]
    fn successful_filesystem_stream_fixture_covers_the_requested_range() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/filesystem-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let permission = CapabilityRegistry::default()
            .normalize(&manifest)
            .unwrap()
            .remove(0);
        let request = FilesystemReadWireRequest::ReadFileStream(FilesystemReadStream {
            mount: "gallery".into(),
            path: "demo.txt".into(),
            offset: 0,
            maximum_bytes: 5,
        });
        let result = FilesystemReadResultFixture::Stream {
            total_size: 10,
            chunks: vec![ByteFixture::Text("123".into())],
            terminal_error: None,
        };
        let error = filesystem_read_result(&result, &request, &permission)
            .err()
            .expect("short successful filesystem stream should fail");

        assert!(error.to_string().contains("exactly the requested bytes"));
    }

    #[test]
    fn filesystem_inline_fixture_derives_offset_size_and_eof() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/filesystem-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let permission = CapabilityRegistry::default()
            .normalize(&manifest)
            .unwrap()
            .remove(0);
        let request = FilesystemReadWireRequest::ReadFile(FilesystemReadFile {
            mount: "gallery".into(),
            path: "demo.txt".into(),
            offset: 2,
            maximum_bytes: 3,
        });
        let result = FilesystemReadResultFixture::File {
            total_size: 5,
            body: Some(ByteFixture::Text("345".into())),
        };
        let PreparedFilesystemReadResult::Inline(BrokerResult::Success { payload }) =
            filesystem_read_result(&result, &request, &permission).unwrap()
        else {
            panic!("expected inline filesystem result");
        };

        assert_eq!(
            FilesystemFileChunk::decode(&payload).unwrap(),
            FilesystemFileChunk {
                offset: 2,
                total_size: 5,
                eof: true,
                bytes: b"345".to_vec(),
            }
        );
    }

    #[test]
    fn filesystem_fixture_response_kind_must_match_the_operation() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/filesystem-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let permission = CapabilityRegistry::default()
            .normalize(&manifest)
            .unwrap()
            .remove(0);
        let request = FilesystemReadWireRequest::ListDirectory(FilesystemListDirectory {
            mount: "gallery".into(),
            path: String::new(),
            maximum_entries: 4,
        });
        let result = FilesystemReadResultFixture::File {
            total_size: 0,
            body: None,
        };

        assert!(filesystem_read_result(&result, &request, &permission).is_err());
    }

    #[test]
    fn filesystem_directory_fixture_is_sorted_like_production() {
        let result = filesystem_entries(
            &[
                FilesystemEntryFixture {
                    name: "z.txt".into(),
                    kind: FilesystemEntryKindFixture::RegularFile,
                    size: 1,
                },
                FilesystemEntryFixture {
                    name: "a.txt".into(),
                    kind: FilesystemEntryKindFixture::RegularFile,
                    size: 1,
                },
            ],
            &BTreeSet::from([FileKind::RegularFile]),
        );

        assert!(result.is_err());
    }

    #[test]
    fn filesystem_write_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/filesystem-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/filesystem-component-plugin/tests/write-replay.json"
        ))
        .unwrap();
        let FilesystemWriteRequestFixture::Write(request) =
            &mut scenario.broker.filesystem_writes[0].request
        else {
            panic!("expected inline filesystem-write fixture")
        };
        request.path = "../escape.txt".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope filesystem-write fixture was accepted");
        };

        assert!(
            error
                .to_string()
                .contains("filesystem-write fixture is not authorized")
        );
    }

    #[test]
    fn filesystem_write_stream_fixture_requires_exact_committed_size() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/filesystem-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let permissions = CapabilityRegistry::default().normalize(&manifest).unwrap();
        let permission = permissions
            .iter()
            .find(|permission| permission.capability == CapabilityId::FilesystemWriteV1)
            .unwrap();
        let fixture: FilesystemWriteFixture = serde_json::from_str(
            r#"{
                "id":"short-stream",
                "operation":"create-file-stream",
                "request":{"mount":"workspace","path":"short.txt","expected_bytes":4},
                "chunks":[{"body":"abc","result":{"kind":"success"}}],
                "commits":[{"kind":"mutation","bytes_written":4,"resulting_size":4}]
            }"#,
        )
        .unwrap();
        let bindings = replay_filesystem_write_bindings(permission).unwrap();
        let identity = ConnectionIdentity {
            instance_id: 1,
            package: PackageInstance {
                source: manifest.plugin.source.clone(),
                version: manifest.plugin.version.clone(),
                digest: "0".repeat(64),
                provenance: Provenance::LocalDevelopment,
                runtime: RuntimeKind::Component,
            },
        };

        let Err(error) = prepare_filesystem_write(&fixture, &identity, permission, &bindings)
        else {
            panic!("short stream fixture should fail");
        };
        assert!(
            error
                .to_string()
                .contains("commits after 3 successful bytes")
        );
    }

    #[test]
    fn local_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../plugins/command-deck/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../plugins/command-deck/tests/custom-replay.json"
        ))
        .unwrap();
        scenario.broker.local_connections[0].request.endpoint = "unapproved".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope local connection fixture was accepted");
        };

        assert!(error.to_string().contains("not authorized: OutOfScope"));
    }

    #[test]
    fn local_fixture_rejects_events_after_peer_termination() {
        let mut traffic = 0;
        let mut frames = 0;
        let result = prepare_local_events(
            &[
                LocalPeerEventFixture::Closed,
                LocalPeerEventFixture::Frame {
                    body: ByteFixture::Text("late".into()),
                },
            ],
            4096,
            8192,
            &mut traffic,
            &mut frames,
        );

        assert!(result.is_err());
    }

    #[test]
    fn local_fixture_enforces_aggregate_traffic_scope() {
        let mut traffic = 0;
        let mut frames = 0;
        let result = prepare_local_events(
            &[LocalPeerEventFixture::Frame {
                body: ByteFixture::Text("12345".into()),
            }],
            4096,
            4,
            &mut traffic,
            &mut frames,
        );

        assert!(
            result
                .err()
                .expect("over-quota local fixture should fail")
                .to_string()
                .contains("maximum_bytes_per_minute")
        );
    }

    #[test]
    fn clipboard_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/clipboard-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/clipboard-component-plugin/tests/replay.json"
        ))
        .unwrap();
        let ClipboardRequestFixtureValue::Read(request) =
            &mut scenario.broker.clipboard_requests[0].request
        else {
            panic!("expected clipboard-read fixture")
        };
        request.mime_type = "image/png".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope clipboard fixture was accepted");
        };

        assert!(
            error
                .to_string()
                .contains("clipboard fixture is not authorized: OutOfScope")
        );
    }

    #[test]
    fn clipboard_fixture_response_mime_must_be_exact() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/clipboard-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/clipboard-component-plugin/tests/replay.json"
        ))
        .unwrap();
        let ClipboardResultFixture::Value { mime_type, .. } =
            &mut scenario.broker.clipboard_requests[0].responses[0]
        else {
            panic!("expected clipboard value fixture")
        };
        *mime_type = "text/html".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("clipboard response with the wrong MIME was accepted");
        };

        assert!(error.to_string().contains("response MIME must match"));
    }

    #[test]
    fn clipboard_fixture_response_obeys_manifest_byte_limit() {
        let result = ClipboardResultFixture::Value {
            mime_type: "text/plain".into(),
            body: ByteFixture::Text("12345".into()),
        };
        let request = ClipboardRequestFixtureValue::Read(ClipboardReadFixture {
            mime_type: "text/plain".into(),
        });
        let error = clipboard_result(&result, ClipboardOperationFixture::Read, &request, 4)
            .expect_err("oversized clipboard response should fail");

        assert!(error.to_string().contains("maximum_bytes"));
    }

    #[test]
    fn clipboard_fixture_operation_and_request_shape_must_match() {
        let request = ClipboardRequestFixtureValue::Read(ClipboardReadFixture {
            mime_type: "text/plain".into(),
        });
        let error = clipboard_request(ClipboardOperationFixture::Write, &request)
            .expect_err("mismatched clipboard operation should fail");

        assert!(error.to_string().contains("does not match its operation"));
    }

    #[test]
    fn notification_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/desktop-action-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/desktop-action-component-plugin/tests/replay.json"
        ))
        .unwrap();
        let NotificationRequestFixtureValue::Send(request) =
            &mut scenario.broker.notification_requests[0].request
        else {
            panic!("expected notification-send fixture")
        };
        request.category = "security".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope notification fixture was accepted");
        };

        assert!(
            error
                .to_string()
                .contains("notification fixture is not authorized: InvalidRequest")
        );
    }

    #[test]
    fn notification_fixture_operation_and_request_shape_must_match() {
        let request = NotificationRequestFixtureValue::Remove(NotificationRemoveFixture {
            id: "demo-ready".into(),
        });
        let error = notification_request(NotificationOperationFixture::Send, &request)
            .expect_err("mismatched operation should fail");

        assert!(error.to_string().contains("does not match its operation"));
    }

    #[test]
    fn replay_notification_rate_uses_the_scenario_clock() {
        let mut sends = VecDeque::new();
        reserve_replay_rate(&mut sends, 0, 2).unwrap();
        reserve_replay_rate(&mut sends, 59_999_999, 2).unwrap();
        assert_eq!(
            reserve_replay_rate(&mut sends, 59_999_999, 2),
            Err(BrokerErrorCode::RateLimited)
        );
        reserve_replay_rate(&mut sends, 60_000_000, 2).unwrap();
        assert_eq!(sends, VecDeque::from([59_999_999, 60_000_000]));
    }

    #[test]
    fn uri_open_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/desktop-action-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/desktop-action-component-plugin/tests/replay.json"
        ))
        .unwrap();
        scenario.broker.uri_opens[0].request.uri = "file:///etc/passwd".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("unsafe URI-open fixture was accepted");
        };

        assert!(
            error
                .to_string()
                .contains("URI-open fixture is not authorized")
        );
    }

    #[test]
    fn secret_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/secret-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/secret-component-plugin/tests/replay.json"
        ))
        .unwrap();
        scenario.broker.secret_reads[0].request.logical_name = "other-token".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope secret fixture was accepted");
        };

        assert!(
            error
                .to_string()
                .contains("secret fixture is not authorized: OutOfScope")
        );
    }

    #[test]
    fn secret_fixture_rejects_invalid_metadata_without_echoing_value() {
        let fixture_value = "fixture-only-token";
        let result = SecretResultFixture::Value {
            content_type: "text/plain\\unsafe".into(),
            body: ByteFixture::Text(fixture_value.into()),
        };
        let error = secret_result(&result).expect_err("invalid content type should fail");

        assert!(
            error
                .to_string()
                .contains("secret fixture value is invalid")
        );
        assert!(!error.to_string().contains(fixture_value));
    }

    #[test]
    fn secret_fixture_rejects_oversized_values() {
        let result = SecretResultFixture::Value {
            content_type: "application/octet-stream".into(),
            body: ByteFixture::Bytes(vec![7; touchbar_broker_schema::MAX_SECRET_BYTES + 1]),
        };
        let error = secret_result(&result).expect_err("oversized secret should fail");

        assert!(
            error
                .to_string()
                .contains("secret fixture value is invalid")
        );
    }

    #[test]
    fn http_fixture_is_limited_by_production_authorization() {
        let manifest = touchbar_package::PluginManifest::from_toml(include_str!(
            "../../../examples/http-component-plugin/touchbar-plugin.toml"
        ))
        .unwrap();
        let mut scenario: Scenario = serde_json::from_str(include_str!(
            "../../../examples/http-component-plugin/tests/replay.json"
        ))
        .unwrap();
        scenario.broker.http_requests[0].request.url =
            "https://metadata.google.internal/latest/".into();

        let Err(error) = scenario.replay_broker(&manifest) else {
            panic!("out-of-scope HTTP fixture was accepted");
        };

        assert!(error.to_string().contains("not authorized: OutOfScope"));
    }

    #[test]
    fn http_fixture_response_kind_must_match_the_operation() {
        let response = HttpResultFixture::Response {
            status: 200,
            final_url: None,
            content_type: None,
            etag: None,
            body: None,
        };
        assert!(
            http_result(
                &response,
                HttpOperationFixture::RequestStream,
                "https://example.test/",
                1024,
            )
            .is_err()
        );
    }

    #[test]
    fn http_fixture_cannot_claim_an_unmodeled_redirect() {
        let response = HttpResultFixture::Response {
            status: 200,
            final_url: Some("https://other.example/".into()),
            content_type: None,
            etag: None,
            body: None,
        };
        let error = http_result(
            &response,
            HttpOperationFixture::Request,
            "https://example.test/",
            1024,
        )
        .err()
        .expect("redirecting fixture should fail");
        assert!(error.to_string().contains("does not simulate redirects"));
    }

    #[test]
    fn http_inline_fixture_is_limited_by_manifest_response_bytes() {
        let response = HttpResultFixture::Response {
            status: 200,
            final_url: None,
            content_type: None,
            etag: None,
            body: Some(ByteFixture::Text("12345".into())),
        };
        let error = http_result(
            &response,
            HttpOperationFixture::Request,
            "https://example.test/",
            4,
        )
        .err()
        .expect("oversized inline response fixture should fail");

        assert!(error.to_string().contains("maximum_response_bytes"));
    }

    #[test]
    fn http_stream_fixture_aggregate_is_limited_by_manifest_response_bytes() {
        let response = HttpResultFixture::Stream {
            status: 200,
            final_url: None,
            content_type: None,
            etag: None,
            chunks: vec![
                ByteFixture::Text("123".into()),
                ByteFixture::Text("45".into()),
            ],
            terminal_error: None,
        };
        let error = http_result(
            &response,
            HttpOperationFixture::RequestStream,
            "https://example.test/",
            4,
        )
        .err()
        .expect("oversized streamed response fixture should fail");

        assert!(error.to_string().contains("maximum_response_bytes"));
    }
}
