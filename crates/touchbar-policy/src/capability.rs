use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
    str::FromStr,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use touchbar_package::{PermissionRequest, PluginManifest};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityId {
    #[serde(rename = "context.read.v1")]
    ContextReadV1,
    #[serde(rename = "filesystem.read.v1")]
    FilesystemReadV1,
    #[serde(rename = "filesystem.write.v1")]
    FilesystemWriteV1,
    #[serde(rename = "http.request.v1")]
    HttpRequestV1,
    #[serde(rename = "dbus.call.v1")]
    DbusCallV1,
    #[serde(rename = "dbus.subscribe.v1")]
    DbusSubscribeV1,
    #[serde(rename = "command.run.v1")]
    CommandRunV1,
    #[serde(rename = "clipboard.read.v1")]
    ClipboardReadV1,
    #[serde(rename = "clipboard.write.v1")]
    ClipboardWriteV1,
    #[serde(rename = "secret.read.v1")]
    SecretReadV1,
    #[serde(rename = "notification.send.v1")]
    NotificationSendV1,
    #[serde(rename = "uri.open.v1")]
    UriOpenV1,
    #[serde(rename = "local.connect.v1")]
    LocalConnectV1,
    #[serde(rename = "appearance.provide.v1")]
    AppearanceProvideV1,
    #[serde(untagged)]
    Unknown(String),
}

impl CapabilityId {
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRegistry {
    supported: BTreeSet<CapabilityId>,
}

impl CapabilityRegistry {
    pub fn v1() -> Self {
        Self {
            supported: BTreeSet::from([
                CapabilityId::ContextReadV1,
                CapabilityId::FilesystemReadV1,
                CapabilityId::FilesystemWriteV1,
                CapabilityId::HttpRequestV1,
                CapabilityId::DbusCallV1,
                CapabilityId::DbusSubscribeV1,
                CapabilityId::CommandRunV1,
                CapabilityId::ClipboardReadV1,
                CapabilityId::ClipboardWriteV1,
                CapabilityId::SecretReadV1,
                CapabilityId::NotificationSendV1,
                CapabilityId::UriOpenV1,
                CapabilityId::LocalConnectV1,
                CapabilityId::AppearanceProvideV1,
            ]),
        }
    }

    pub fn from_supported(supported: impl IntoIterator<Item = CapabilityId>) -> Self {
        Self {
            supported: supported.into_iter().collect(),
        }
    }

    pub fn supports(&self, capability: &CapabilityId) -> bool {
        capability.is_known() && self.supported.contains(capability)
    }

    pub fn supported(&self) -> impl Iterator<Item = &CapabilityId> {
        self.supported.iter()
    }

    pub fn normalize(
        &self,
        manifest: &PluginManifest,
    ) -> Result<Vec<CapabilityRequest>, Vec<NormalizationError>> {
        normalize_manifest_permissions(manifest)
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::v1()
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ContextReadV1 => "context.read.v1",
            Self::FilesystemReadV1 => "filesystem.read.v1",
            Self::FilesystemWriteV1 => "filesystem.write.v1",
            Self::HttpRequestV1 => "http.request.v1",
            Self::DbusCallV1 => "dbus.call.v1",
            Self::DbusSubscribeV1 => "dbus.subscribe.v1",
            Self::CommandRunV1 => "command.run.v1",
            Self::ClipboardReadV1 => "clipboard.read.v1",
            Self::ClipboardWriteV1 => "clipboard.write.v1",
            Self::SecretReadV1 => "secret.read.v1",
            Self::NotificationSendV1 => "notification.send.v1",
            Self::UriOpenV1 => "uri.open.v1",
            Self::LocalConnectV1 => "local.connect.v1",
            Self::AppearanceProvideV1 => "appearance.provide.v1",
            Self::Unknown(value) => value,
        })
    }
}

impl FromStr for CapabilityId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let known = match value {
            "context.read.v1" => Self::ContextReadV1,
            "filesystem.read.v1" => Self::FilesystemReadV1,
            "filesystem.write.v1" => Self::FilesystemWriteV1,
            "http.request.v1" => Self::HttpRequestV1,
            "dbus.call.v1" => Self::DbusCallV1,
            "dbus.subscribe.v1" => Self::DbusSubscribeV1,
            "command.run.v1" => Self::CommandRunV1,
            "clipboard.read.v1" => Self::ClipboardReadV1,
            "clipboard.write.v1" => Self::ClipboardWriteV1,
            "secret.read.v1" => Self::SecretReadV1,
            "notification.send.v1" => Self::NotificationSendV1,
            "uri.open.v1" => Self::UriOpenV1,
            "local.connect.v1" => Self::LocalConnectV1,
            "appearance.provide.v1" => Self::AppearanceProvideV1,
            _ if valid_dotted_id(value) => Self::Unknown(value.into()),
            _ => return Err("capability must be a lowercase dotted identifier".into()),
        };
        Ok(known)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskClass {
    Isolated,
    Limited,
    Connected,
    Sensitive,
    Controlling,
    EffectivelyTrusted,
    UnrestrictedNative,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    Component,
    Native,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustSummary {
    pub class: RiskClass,
    pub warnings: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityStatus {
    Granted,
    Denied,
    NeedsConsent,
    Unsupported,
    DisclosureOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequest {
    pub capability: CapabilityId,
    pub required: bool,
    pub reason: String,
    pub scope: CapabilityScope,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum CapabilityScope {
    ContextRead(ContextReadScope),
    FilesystemRead(FilesystemReadScope),
    FilesystemWrite(FilesystemWriteScope),
    HttpRequest(HttpRequestScope),
    DbusCall(DbusCallScope),
    DbusSubscribe(DbusSubscribeScope),
    CommandRun(CommandRunScope),
    Clipboard(ClipboardScope),
    SecretRead(SecretReadScope),
    NotificationSend(NotificationScope),
    UriOpen(UriOpenScope),
    LocalConnect(LocalConnectScope),
    AppearanceProvide(AppearanceProvideScope),
    Unknown(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizationError {
    pub field: String,
    pub message: String,
}

impl fmt::Display for NormalizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemMountRequest {
    pub label: String,
    #[serde(default)]
    pub suggested_location: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextReadScope {
    pub facts: BTreeSet<String>,
    pub maximum_updates_per_second: u16,
}

impl Default for ContextReadScope {
    fn default() -> Self {
        Self {
            facts: BTreeSet::new(),
            maximum_updates_per_second: 10,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileKind {
    RegularFile,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesystemReadScope {
    pub mounts: BTreeSet<FilesystemMountRequest>,
    pub kinds: BTreeSet<FileKind>,
    pub maximum_file_bytes: u64,
    pub enumerate: bool,
}

impl Default for FilesystemReadScope {
    fn default() -> Self {
        Self {
            mounts: BTreeSet::new(),
            kinds: BTreeSet::from([FileKind::RegularFile]),
            maximum_file_bytes: 4 * 1024 * 1024,
            enumerate: false,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteOperation {
    Create,
    Replace,
    Append,
    Delete,
    Rename,
    CreateDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesystemWriteScope {
    pub mounts: BTreeSet<FilesystemMountRequest>,
    pub operations: BTreeSet<WriteOperation>,
    pub maximum_file_bytes: u64,
    pub maximum_total_bytes_per_hour: u64,
}

impl Default for FilesystemWriteScope {
    fn default() -> Self {
        Self {
            mounts: BTreeSet::new(),
            operations: BTreeSet::new(),
            maximum_file_bytes: 4 * 1024 * 1024,
            maximum_total_bytes_per_hour: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOriginRule {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub path_prefixes: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpRequestScope {
    pub origins: BTreeSet<HttpOriginRule>,
    pub methods: BTreeSet<HttpMethod>,
    pub private_network: bool,
    pub maximum_request_bytes: u64,
    pub maximum_response_bytes: u64,
    pub maximum_requests_per_minute: u16,
}

impl Default for HttpRequestScope {
    fn default() -> Self {
        Self {
            origins: BTreeSet::new(),
            methods: BTreeSet::from([HttpMethod::Get]),
            private_network: false,
            maximum_request_bytes: 64 * 1024,
            maximum_response_bytes: 4 * 1024 * 1024,
            maximum_requests_per_minute: 30,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DbusBus {
    Session,
    System,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbusArgumentConstraint {
    pub index: u16,
    #[serde(default)]
    pub equals_string: Option<String>,
    #[serde(default)]
    pub one_of_strings: Option<BTreeSet<String>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbusCallRule {
    pub bus: DbusBus,
    pub destination: String,
    pub path: String,
    pub interface: String,
    pub member: String,
    pub signature: String,
    #[serde(default)]
    pub arguments: Vec<DbusArgumentConstraint>,
    #[serde(default)]
    pub allow_service_activation: bool,
    #[serde(default)]
    pub requires_user_activation: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbusCallScope {
    pub rules: BTreeSet<DbusCallRule>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbusSignalRule {
    pub bus: DbusBus,
    pub sender: String,
    pub path: String,
    pub interface: String,
    pub member: String,
    pub signature: String,
    #[serde(default)]
    pub argument_zero: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbusSubscribeScope {
    pub rules: BTreeSet<DbusSignalRule>,
    pub maximum_events_per_second: u16,
}

impl Default for DbusSubscribeScope {
    fn default() -> Self {
        Self {
            rules: BTreeSet::new(),
            maximum_events_per_second: 30,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CommandArgument {
    Literal {
        value: String,
    },
    BoundedInteger {
        name: String,
        minimum: i64,
        maximum: i64,
    },
    FixedEnum {
        name: String,
        values: BTreeSet<String>,
    },
    BoundedText {
        name: String,
        maximum_bytes: u32,
    },
    ApprovedFile {
        name: String,
        mount: String,
    },
    Url {
        name: String,
        schemes: BTreeSet<String>,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRule {
    pub id: String,
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<CommandArgument>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default = "default_command_output")]
    pub maximum_output_bytes: u64,
    #[serde(default = "default_command_timeout")]
    pub timeout_milliseconds: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandRunScope {
    pub commands: BTreeSet<CommandRule>,
    pub maximum_parallel_processes: u8,
}

impl Default for CommandRunScope {
    fn default() -> Self {
        Self {
            commands: BTreeSet::new(),
            maximum_parallel_processes: 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClipboardScope {
    pub mime_types: BTreeSet<String>,
    pub maximum_bytes: u64,
    pub maximum_operations_per_minute: u16,
}

impl Default for ClipboardScope {
    fn default() -> Self {
        Self {
            mime_types: BTreeSet::new(),
            maximum_bytes: 64 * 1024,
            maximum_operations_per_minute: 12,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretReadScope {
    pub logical_names: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationUrgency {
    Low,
    Normal,
    Critical,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationScope {
    pub categories: BTreeSet<String>,
    pub urgency: BTreeSet<NotificationUrgency>,
    pub actions: bool,
    pub maximum_per_minute: u16,
}

impl Default for NotificationScope {
    fn default() -> Self {
        Self {
            categories: BTreeSet::new(),
            urgency: BTreeSet::from([NotificationUrgency::Normal]),
            actions: false,
            maximum_per_minute: 6,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UriOpenScope {
    pub schemes: BTreeSet<String>,
    pub origins: BTreeSet<HttpOriginRule>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalEndpointRequest {
    pub label: String,
    pub protocol: String,
    #[serde(default)]
    pub suggested_endpoint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConnectScope {
    pub endpoints: BTreeSet<LocalEndpointRequest>,
    pub maximum_frame_bytes: u32,
    pub maximum_bytes_per_minute: u64,
}

/// Authority to offer named package appearance providers to the compositor.
/// The compositor remains responsible for source selection, validation, and
/// assigning atomic appearance generations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppearanceProvideScope {
    pub providers: BTreeSet<String>,
    pub mounts: BTreeSet<FilesystemMountRequest>,
    pub maximum_file_bytes: u64,
    pub maximum_updates_per_second: u16,
}

impl Default for AppearanceProvideScope {
    fn default() -> Self {
        Self {
            providers: BTreeSet::new(),
            mounts: BTreeSet::new(),
            maximum_file_bytes: 64 * 1024,
            maximum_updates_per_second: 4,
        }
    }
}

impl Default for LocalConnectScope {
    fn default() -> Self {
        Self {
            endpoints: BTreeSet::new(),
            maximum_frame_bytes: 64 * 1024,
            maximum_bytes_per_minute: 4 * 1024 * 1024,
        }
    }
}

pub fn normalize_manifest_permissions(
    manifest: &PluginManifest,
) -> Result<Vec<CapabilityRequest>, Vec<NormalizationError>> {
    if manifest.permissions.len() > 64 {
        return Err(vec![NormalizationError {
            field: "permission".into(),
            message: "a package may request at most 64 capabilities".into(),
        }]);
    }
    let mut requests = Vec::with_capacity(manifest.permissions.len());
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, permission) in manifest.permissions.iter().enumerate() {
        match normalize_permission(permission, index) {
            Ok(request) => {
                if !seen.insert(request.capability.clone()) {
                    errors.push(NormalizationError {
                        field: format!("permission[{index}].capability"),
                        message: "duplicate capability request".into(),
                    });
                } else {
                    requests.push(request);
                }
            }
            Err(mut permission_errors) => errors.append(&mut permission_errors),
        }
    }
    if errors.is_empty() {
        for (index, provider) in manifest.appearance_providers.iter().enumerate() {
            let prefix = format!("appearance-provider[{index}]");
            let appearance_authorized = requests.iter().any(|request| {
                matches!(
                    &request.scope,
                    CapabilityScope::AppearanceProvide(scope)
                        if scope.providers.contains(&provider.id)
                            && scope.mounts.iter().any(|mount| mount.label == provider.mount)
                )
            });
            if !appearance_authorized {
                errors.push(NormalizationError {
                    field: format!("{prefix}.id"),
                    message: format!(
                        "provider `{}` and mount `{}` must be listed by appearance.provide.v1",
                        provider.id, provider.mount
                    ),
                });
            }
        }
    }
    if errors.is_empty() {
        requests.sort_by(|left, right| left.capability.cmp(&right.capability));
        Ok(requests)
    } else {
        Err(errors)
    }
}

fn normalize_permission(
    permission: &PermissionRequest,
    index: usize,
) -> Result<CapabilityRequest, Vec<NormalizationError>> {
    let encoded_scope = toml::to_string(&permission.scope).map_err(|error| {
        vec![NormalizationError {
            field: format!("permission[{index}].scope"),
            message: error.to_string(),
        }]
    })?;
    if encoded_scope.len() > 64 * 1024 {
        return Err(vec![NormalizationError {
            field: format!("permission[{index}].scope"),
            message: "encoded scope exceeds 64 KiB".into(),
        }]);
    }
    let capability = permission
        .capability
        .parse::<CapabilityId>()
        .map_err(|message| {
            vec![NormalizationError {
                field: format!("permission[{index}].capability"),
                message,
            }]
        })?;
    let field = format!("permission[{index}].scope");
    let mut errors = Vec::new();
    let scope = match &capability {
        CapabilityId::ContextReadV1 => {
            decode_scope::<ContextReadScope>(permission, &field).map(CapabilityScope::ContextRead)
        }
        CapabilityId::FilesystemReadV1 => decode_scope::<FilesystemReadScope>(permission, &field)
            .map(CapabilityScope::FilesystemRead),
        CapabilityId::FilesystemWriteV1 => decode_scope::<FilesystemWriteScope>(permission, &field)
            .map(CapabilityScope::FilesystemWrite),
        CapabilityId::HttpRequestV1 => {
            decode_scope::<HttpRequestScope>(permission, &field).map(CapabilityScope::HttpRequest)
        }
        CapabilityId::DbusCallV1 => {
            decode_scope::<DbusCallScope>(permission, &field).map(CapabilityScope::DbusCall)
        }
        CapabilityId::DbusSubscribeV1 => decode_scope::<DbusSubscribeScope>(permission, &field)
            .map(CapabilityScope::DbusSubscribe),
        CapabilityId::CommandRunV1 => {
            decode_scope::<CommandRunScope>(permission, &field).map(CapabilityScope::CommandRun)
        }
        CapabilityId::ClipboardReadV1 | CapabilityId::ClipboardWriteV1 => {
            decode_scope::<ClipboardScope>(permission, &field).map(CapabilityScope::Clipboard)
        }
        CapabilityId::SecretReadV1 => {
            decode_scope::<SecretReadScope>(permission, &field).map(CapabilityScope::SecretRead)
        }
        CapabilityId::NotificationSendV1 => decode_scope::<NotificationScope>(permission, &field)
            .map(CapabilityScope::NotificationSend),
        CapabilityId::UriOpenV1 => {
            decode_scope::<UriOpenScope>(permission, &field).map(CapabilityScope::UriOpen)
        }
        CapabilityId::LocalConnectV1 => {
            decode_scope::<LocalConnectScope>(permission, &field).map(CapabilityScope::LocalConnect)
        }
        CapabilityId::AppearanceProvideV1 => {
            decode_scope::<AppearanceProvideScope>(permission, &field)
                .map(CapabilityScope::AppearanceProvide)
        }
        CapabilityId::Unknown(_) => Ok(CapabilityScope::Unknown(encoded_scope)),
    }
    .map_err(|message| {
        vec![NormalizationError {
            field: field.clone(),
            message,
        }]
    })?;
    validate_scope(&capability, &scope, &field, &mut errors);
    if permission.reason.len() > 1024 {
        errors.push(NormalizationError {
            field: format!("permission[{index}].reason"),
            message: "reason exceeds 1024 bytes".into(),
        });
    }
    if errors.is_empty() {
        Ok(CapabilityRequest {
            capability,
            required: permission.required,
            reason: permission.reason.trim().into(),
            scope,
        })
    } else {
        Err(errors)
    }
}

fn decode_scope<T: DeserializeOwned>(
    permission: &PermissionRequest,
    field: &str,
) -> Result<T, String> {
    toml::Value::Table(
        permission
            .scope
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
    .try_into()
    .map_err(|error| format!("invalid {field}: {error}"))
}

fn validate_scope(
    capability: &CapabilityId,
    scope: &CapabilityScope,
    field: &str,
    errors: &mut Vec<NormalizationError>,
) {
    let mut issue = |message: String| {
        errors.push(NormalizationError {
            field: field.into(),
            message,
        });
    };
    match scope {
        CapabilityScope::ContextRead(value) => {
            require_nonempty(&value.facts, "facts", &mut issue);
            require_collection_limit(value.facts.len(), 32, "facts", &mut issue);
            validate_rate(value.maximum_updates_per_second, &mut issue);
            require_at_most(
                value.maximum_updates_per_second,
                60,
                "maximum_updates_per_second",
                &mut issue,
            );
            for fact in &value.facts {
                if !valid_dotted_id(fact) {
                    issue(format!("invalid context fact {fact}"));
                }
            }
        }
        CapabilityScope::FilesystemRead(value) => {
            validate_mounts(&value.mounts, &mut issue);
            require_nonzero(value.maximum_file_bytes, "maximum_file_bytes", &mut issue);
            require_at_most(
                value.maximum_file_bytes,
                64 * 1024 * 1024,
                "maximum_file_bytes",
                &mut issue,
            );
        }
        CapabilityScope::FilesystemWrite(value) => {
            validate_mounts(&value.mounts, &mut issue);
            require_nonempty(&value.operations, "operations", &mut issue);
            require_nonzero(value.maximum_file_bytes, "maximum_file_bytes", &mut issue);
            require_nonzero(
                value.maximum_total_bytes_per_hour,
                "maximum_total_bytes_per_hour",
                &mut issue,
            );
            require_at_most(
                value.maximum_file_bytes,
                64 * 1024 * 1024,
                "maximum_file_bytes",
                &mut issue,
            );
            require_at_most(
                value.maximum_total_bytes_per_hour,
                1024 * 1024 * 1024,
                "maximum_total_bytes_per_hour",
                &mut issue,
            );
        }
        CapabilityScope::HttpRequest(value) => {
            require_nonempty(&value.origins, "origins", &mut issue);
            require_nonempty(&value.methods, "methods", &mut issue);
            require_nonzero(
                value.maximum_request_bytes,
                "maximum_request_bytes",
                &mut issue,
            );
            require_nonzero(
                value.maximum_response_bytes,
                "maximum_response_bytes",
                &mut issue,
            );
            validate_rate(value.maximum_requests_per_minute, &mut issue);
            require_at_most(
                value.maximum_request_bytes,
                4 * 1024 * 1024,
                "maximum_request_bytes",
                &mut issue,
            );
            require_at_most(
                value.maximum_response_bytes,
                32 * 1024 * 1024,
                "maximum_response_bytes",
                &mut issue,
            );
            require_at_most(
                value.maximum_requests_per_minute,
                600,
                "maximum_requests_per_minute",
                &mut issue,
            );
            for origin in &value.origins {
                validate_http_origin(origin, &mut issue);
            }
        }
        CapabilityScope::DbusCall(value) => {
            require_nonempty(&value.rules, "rules", &mut issue);
            require_collection_limit(value.rules.len(), 64, "rules", &mut issue);
            for rule in &value.rules {
                validate_dbus_rule(
                    &rule.destination,
                    &rule.path,
                    &rule.interface,
                    &rule.member,
                    &mut issue,
                );
                let mut indices = BTreeSet::new();
                for argument in &rule.arguments {
                    if !indices.insert(argument.index) {
                        issue(format!(
                            "duplicate D-Bus argument constraint {}",
                            argument.index
                        ));
                    }
                    if argument.equals_string.is_some() == argument.one_of_strings.is_some() {
                        issue("D-Bus argument constraint requires exactly one matcher".into());
                    }
                    if argument
                        .one_of_strings
                        .as_ref()
                        .is_some_and(BTreeSet::is_empty)
                    {
                        issue("D-Bus argument one_of_strings must not be empty".into());
                    }
                }
            }
        }
        CapabilityScope::DbusSubscribe(value) => {
            require_nonempty(&value.rules, "rules", &mut issue);
            validate_rate(value.maximum_events_per_second, &mut issue);
            require_at_most(
                value.maximum_events_per_second,
                120,
                "maximum_events_per_second",
                &mut issue,
            );
            require_collection_limit(value.rules.len(), 64, "rules", &mut issue);
            for rule in &value.rules {
                validate_dbus_rule(
                    &rule.sender,
                    &rule.path,
                    &rule.interface,
                    &rule.member,
                    &mut issue,
                );
            }
        }
        CapabilityScope::CommandRun(value) => {
            require_nonempty(&value.commands, "commands", &mut issue);
            if value.maximum_parallel_processes == 0 {
                issue("maximum_parallel_processes must be nonzero".into());
            }
            require_at_most(
                value.maximum_parallel_processes,
                4,
                "maximum_parallel_processes",
                &mut issue,
            );
            require_collection_limit(value.commands.len(), 32, "commands", &mut issue);
            let mut command_ids = BTreeSet::new();
            for command in &value.commands {
                if !valid_kebab_id(&command.id) || !command_ids.insert(command.id.as_str()) {
                    issue(format!("invalid command id {}", command.id));
                }
                let executable = Path::new(&command.executable);
                if !executable.is_absolute()
                    || executable.components().any(|part| {
                        !matches!(
                            part,
                            std::path::Component::RootDir | std::path::Component::Normal(_)
                        )
                    })
                {
                    issue(format!(
                        "command executable {} must be an absolute normalized path",
                        command.executable
                    ));
                }
                require_nonzero(
                    command.maximum_output_bytes,
                    "maximum_output_bytes",
                    &mut issue,
                );
                require_nonzero(
                    command.timeout_milliseconds,
                    "timeout_milliseconds",
                    &mut issue,
                );
                require_at_most(
                    command.maximum_output_bytes,
                    1024 * 1024,
                    "maximum_output_bytes",
                    &mut issue,
                );
                require_at_most(
                    command.timeout_milliseconds,
                    60_000,
                    "timeout_milliseconds",
                    &mut issue,
                );
                require_collection_limit(
                    command.arguments.len(),
                    32,
                    "command arguments",
                    &mut issue,
                );
                let mut argument_names = BTreeSet::new();
                for argument in &command.arguments {
                    validate_command_argument(argument, &mut issue);
                    if let Some(name) = command_argument_name(argument)
                        && !argument_names.insert(name)
                    {
                        issue(format!("duplicate command argument name {name}"));
                    }
                }
                require_collection_limit(
                    command.environment.len(),
                    16,
                    "command environment",
                    &mut issue,
                );
                for (name, value) in &command.environment {
                    if !valid_environment_name(name) || dangerous_environment_name(name) {
                        issue(format!("invalid environment name {name}"));
                    }
                    if value.len() > 4096 || value.contains('\0') {
                        issue(format!("invalid environment value for {name}"));
                    }
                }
                if let Some(directory) = &command.working_directory {
                    let directory = Path::new(directory);
                    if !directory.is_absolute()
                        || directory.components().any(|part| {
                            !matches!(
                                part,
                                std::path::Component::RootDir | std::path::Component::Normal(_)
                            )
                        })
                    {
                        issue(
                            "command working_directory must be an absolute normalized path".into(),
                        );
                    }
                }
            }
        }
        CapabilityScope::Clipboard(value) => {
            require_nonempty(&value.mime_types, "mime_types", &mut issue);
            require_collection_limit(value.mime_types.len(), 16, "mime_types", &mut issue);
            require_nonzero(value.maximum_bytes, "maximum_bytes", &mut issue);
            require_at_most(
                value.maximum_bytes,
                4 * 1024 * 1024,
                "maximum_bytes",
                &mut issue,
            );
            validate_rate(value.maximum_operations_per_minute, &mut issue);
            require_at_most(
                value.maximum_operations_per_minute,
                60,
                "maximum_operations_per_minute",
                &mut issue,
            );
            for mime in &value.mime_types {
                if mime.is_empty()
                    || mime.len() > 128
                    || !mime.is_ascii()
                    || !mime.contains('/')
                    || mime
                        .bytes()
                        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
                {
                    issue(format!("invalid MIME type {mime}"));
                }
            }
        }
        CapabilityScope::SecretRead(value) => {
            require_nonempty(&value.logical_names, "logical_names", &mut issue);
            validate_kebab_set(&value.logical_names, "secret", &mut issue);
        }
        CapabilityScope::NotificationSend(value) => {
            require_nonempty(&value.categories, "categories", &mut issue);
            require_nonempty(&value.urgency, "urgency", &mut issue);
            validate_rate(value.maximum_per_minute, &mut issue);
            require_at_most(
                value.maximum_per_minute,
                60,
                "maximum_per_minute",
                &mut issue,
            );
            validate_kebab_set(&value.categories, "notification category", &mut issue);
        }
        CapabilityScope::UriOpen(value) => {
            require_nonempty(&value.schemes, "schemes", &mut issue);
            for scheme in &value.schemes {
                if !valid_scheme(scheme) {
                    issue(format!("invalid URI scheme {scheme}"));
                }
            }
            for origin in &value.origins {
                validate_http_origin(origin, &mut issue);
            }
        }
        CapabilityScope::LocalConnect(value) => {
            require_nonempty(&value.endpoints, "endpoints", &mut issue);
            require_nonzero(value.maximum_frame_bytes, "maximum_frame_bytes", &mut issue);
            require_nonzero(
                value.maximum_bytes_per_minute,
                "maximum_bytes_per_minute",
                &mut issue,
            );
            require_at_most(
                value.maximum_frame_bytes,
                1024 * 1024,
                "maximum_frame_bytes",
                &mut issue,
            );
            require_at_most(
                value.maximum_bytes_per_minute,
                64 * 1024 * 1024,
                "maximum_bytes_per_minute",
                &mut issue,
            );
            for endpoint in &value.endpoints {
                if !valid_kebab_id(&endpoint.label) || !valid_dotted_id(&endpoint.protocol) {
                    issue(format!("invalid local endpoint {}", endpoint.label));
                }
            }
        }
        CapabilityScope::AppearanceProvide(value) => {
            require_nonempty(&value.providers, "providers", &mut issue);
            require_collection_limit(value.providers.len(), 8, "providers", &mut issue);
            validate_mounts(&value.mounts, &mut issue);
            require_nonzero(value.maximum_file_bytes, "maximum_file_bytes", &mut issue);
            require_at_most(
                value.maximum_file_bytes,
                1024 * 1024,
                "maximum_file_bytes",
                &mut issue,
            );
            validate_rate(value.maximum_updates_per_second, &mut issue);
            require_at_most(
                value.maximum_updates_per_second,
                30,
                "maximum_updates_per_second",
                &mut issue,
            );
            validate_kebab_set(&value.providers, "appearance provider", &mut issue);
        }
        CapabilityScope::Unknown(_) => {}
    }
    if !scope.matches_capability(capability) {
        issue("scope type does not match capability".into());
    }
}

/// Validates an already-typed scope using the same limits as manifest normalization.
pub fn validate_capability_scope(
    capability: &CapabilityId,
    scope: &CapabilityScope,
) -> Result<(), Vec<NormalizationError>> {
    let mut errors = Vec::new();
    validate_scope(capability, scope, "scope", &mut errors);
    if let CapabilityScope::Unknown(encoded) = scope
        && encoded.len() > 64 * 1024
    {
        errors.push(NormalizationError {
            field: "scope".into(),
            message: "encoded scope exceeds 64 KiB".into(),
        });
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

impl CapabilityScope {
    pub fn matches_capability(&self, capability: &CapabilityId) -> bool {
        matches!(
            (capability, self),
            (CapabilityId::ContextReadV1, Self::ContextRead(_))
                | (CapabilityId::FilesystemReadV1, Self::FilesystemRead(_))
                | (CapabilityId::FilesystemWriteV1, Self::FilesystemWrite(_))
                | (CapabilityId::HttpRequestV1, Self::HttpRequest(_))
                | (CapabilityId::DbusCallV1, Self::DbusCall(_))
                | (CapabilityId::DbusSubscribeV1, Self::DbusSubscribe(_))
                | (CapabilityId::CommandRunV1, Self::CommandRun(_))
                | (CapabilityId::ClipboardReadV1, Self::Clipboard(_))
                | (CapabilityId::ClipboardWriteV1, Self::Clipboard(_))
                | (CapabilityId::SecretReadV1, Self::SecretRead(_))
                | (CapabilityId::NotificationSendV1, Self::NotificationSend(_))
                | (CapabilityId::UriOpenV1, Self::UriOpen(_))
                | (CapabilityId::LocalConnectV1, Self::LocalConnect(_))
                | (
                    CapabilityId::AppearanceProvideV1,
                    Self::AppearanceProvide(_)
                )
                | (CapabilityId::Unknown(_), Self::Unknown(_))
        )
    }

    pub fn is_subset_of(&self, approved: &Self) -> bool {
        match (self, approved) {
            (Self::ContextRead(new), Self::ContextRead(old)) => {
                new.facts.is_subset(&old.facts)
                    && new.maximum_updates_per_second <= old.maximum_updates_per_second
            }
            (Self::FilesystemRead(new), Self::FilesystemRead(old)) => {
                mount_labels_subset(&new.mounts, &old.mounts)
                    && new.kinds.is_subset(&old.kinds)
                    && new.maximum_file_bytes <= old.maximum_file_bytes
                    && bool_authority_subset(new.enumerate, old.enumerate)
            }
            (Self::FilesystemWrite(new), Self::FilesystemWrite(old)) => {
                mount_labels_subset(&new.mounts, &old.mounts)
                    && new.operations.is_subset(&old.operations)
                    && new.maximum_file_bytes <= old.maximum_file_bytes
                    && new.maximum_total_bytes_per_hour <= old.maximum_total_bytes_per_hour
            }
            (Self::HttpRequest(new), Self::HttpRequest(old)) => {
                new.methods.is_subset(&old.methods)
                    && bool_authority_subset(new.private_network, old.private_network)
                    && new.maximum_request_bytes <= old.maximum_request_bytes
                    && new.maximum_response_bytes <= old.maximum_response_bytes
                    && new.maximum_requests_per_minute <= old.maximum_requests_per_minute
                    && origins_subset(&new.origins, &old.origins)
            }
            (Self::DbusCall(new), Self::DbusCall(old)) => new.rules.iter().all(|rule| {
                old.rules
                    .iter()
                    .any(|approved| dbus_call_rule_subset(rule, approved))
            }),
            (Self::DbusSubscribe(new), Self::DbusSubscribe(old)) => {
                new.rules.is_subset(&old.rules)
                    && new.maximum_events_per_second <= old.maximum_events_per_second
            }
            (Self::CommandRun(new), Self::CommandRun(old)) => {
                new.maximum_parallel_processes <= old.maximum_parallel_processes
                    && new.commands.iter().all(|command| {
                        old.commands
                            .iter()
                            .any(|approved| command_rule_subset(command, approved))
                    })
            }
            (Self::Clipboard(new), Self::Clipboard(old)) => {
                new.mime_types.is_subset(&old.mime_types)
                    && new.maximum_bytes <= old.maximum_bytes
                    && new.maximum_operations_per_minute <= old.maximum_operations_per_minute
            }
            (Self::SecretRead(new), Self::SecretRead(old)) => {
                new.logical_names.is_subset(&old.logical_names)
            }
            (Self::NotificationSend(new), Self::NotificationSend(old)) => {
                new.categories.is_subset(&old.categories)
                    && new.urgency.is_subset(&old.urgency)
                    && bool_authority_subset(new.actions, old.actions)
                    && new.maximum_per_minute <= old.maximum_per_minute
            }
            (Self::UriOpen(new), Self::UriOpen(old)) => {
                new.schemes.is_subset(&old.schemes) && origins_subset(&new.origins, &old.origins)
            }
            (Self::LocalConnect(new), Self::LocalConnect(old)) => {
                endpoint_labels_subset(&new.endpoints, &old.endpoints)
                    && new.maximum_frame_bytes <= old.maximum_frame_bytes
                    && new.maximum_bytes_per_minute <= old.maximum_bytes_per_minute
            }
            (Self::AppearanceProvide(new), Self::AppearanceProvide(old)) => {
                new.providers.is_subset(&old.providers)
                    && mount_labels_subset(&new.mounts, &old.mounts)
                    && new.maximum_file_bytes <= old.maximum_file_bytes
                    && new.maximum_updates_per_second <= old.maximum_updates_per_second
            }
            (Self::Unknown(new), Self::Unknown(old)) => new == old,
            _ => false,
        }
    }

    pub fn risk(&self, capability: &CapabilityId) -> RiskClass {
        match capability {
            CapabilityId::ContextReadV1 => match self {
                Self::ContextRead(scope)
                    if scope.facts.iter().all(|fact| context_fact_is_public(fact)) =>
                {
                    RiskClass::Limited
                }
                _ => RiskClass::Sensitive,
            },
            CapabilityId::DbusSubscribeV1 => RiskClass::Sensitive,
            CapabilityId::HttpRequestV1 => RiskClass::Connected,
            CapabilityId::FilesystemReadV1
            | CapabilityId::ClipboardReadV1
            | CapabilityId::SecretReadV1 => RiskClass::Sensitive,
            CapabilityId::FilesystemWriteV1
            | CapabilityId::DbusCallV1
            | CapabilityId::ClipboardWriteV1
            | CapabilityId::NotificationSendV1
            | CapabilityId::UriOpenV1 => RiskClass::Controlling,
            CapabilityId::AppearanceProvideV1 => RiskClass::Controlling,
            CapabilityId::CommandRunV1 => match self {
                Self::CommandRun(scope) if scope.commands.iter().any(command_is_interpreter) => {
                    RiskClass::EffectivelyTrusted
                }
                _ => RiskClass::Controlling,
            },
            CapabilityId::LocalConnectV1 => RiskClass::EffectivelyTrusted,
            CapabilityId::Unknown(_) => RiskClass::EffectivelyTrusted,
        }
    }
}

pub fn summarize_trust<'a>(
    runtime: RuntimeKind,
    grants: impl IntoIterator<Item = &'a CapabilityRequest>,
) -> TrustSummary {
    if runtime == RuntimeKind::Native {
        return TrustSummary {
            class: RiskClass::UnrestrictedNative,
            warnings: BTreeSet::from(["native permission declarations are disclosure only".into()]),
        };
    }
    let grants = grants.into_iter().collect::<Vec<_>>();
    let mut class = RiskClass::Isolated;
    let mut warnings = BTreeSet::new();
    let mut network = false;
    let mut sensitive = false;
    for grant in &grants {
        let risk = grant.scope.risk(&grant.capability);
        class = class.max(risk);
        network |= grant.capability == CapabilityId::HttpRequestV1;
        sensitive |= matches!(
            grant.capability,
            CapabilityId::FilesystemReadV1
                | CapabilityId::ClipboardReadV1
                | CapabilityId::SecretReadV1
        );
    }
    if network && sensitive {
        warnings.insert("network egress can expose data from sensitive read capabilities".into());
        class = class.max(RiskClass::Sensitive);
    }
    TrustSummary { class, warnings }
}

fn dbus_call_rule_subset(new: &DbusCallRule, old: &DbusCallRule) -> bool {
    new.bus == old.bus
        && dotted_name_subset(&new.destination, &old.destination)
        && new.path == old.path
        && new.interface == old.interface
        && new.member == old.member
        && new.signature == old.signature
        && dbus_arguments_subset(&new.arguments, &old.arguments)
        && bool_authority_subset(new.allow_service_activation, old.allow_service_activation)
        && activation_requirement_subset(new.requires_user_activation, old.requires_user_activation)
}

fn command_rule_subset(new: &CommandRule, old: &CommandRule) -> bool {
    new.id == old.id
        && new.executable == old.executable
        && new.arguments.len() == old.arguments.len()
        && new
            .arguments
            .iter()
            .zip(&old.arguments)
            .all(|(new, old)| command_argument_subset(new, old))
        && new.environment == old.environment
        && new.working_directory == old.working_directory
        && new.maximum_output_bytes <= old.maximum_output_bytes
        && new.timeout_milliseconds <= old.timeout_milliseconds
}

fn command_argument_subset(new: &CommandArgument, old: &CommandArgument) -> bool {
    match (new, old) {
        (CommandArgument::Literal { value: new }, CommandArgument::Literal { value: old }) => {
            new == old
        }
        (
            CommandArgument::BoundedInteger {
                name: new_name,
                minimum: new_minimum,
                maximum: new_maximum,
            },
            CommandArgument::BoundedInteger {
                name: old_name,
                minimum: old_minimum,
                maximum: old_maximum,
            },
        ) => new_name == old_name && new_minimum >= old_minimum && new_maximum <= old_maximum,
        (
            CommandArgument::FixedEnum {
                name: new_name,
                values: new_values,
            },
            CommandArgument::FixedEnum {
                name: old_name,
                values: old_values,
            },
        ) => new_name == old_name && new_values.is_subset(old_values),
        (
            CommandArgument::BoundedText {
                name: new_name,
                maximum_bytes: new_maximum,
            },
            CommandArgument::BoundedText {
                name: old_name,
                maximum_bytes: old_maximum,
            },
        ) => new_name == old_name && new_maximum <= old_maximum,
        (
            CommandArgument::ApprovedFile {
                name: new_name,
                mount: new_mount,
            },
            CommandArgument::ApprovedFile {
                name: old_name,
                mount: old_mount,
            },
        ) => new_name == old_name && new_mount == old_mount,
        (
            CommandArgument::Url {
                name: new_name,
                schemes: new_schemes,
            },
            CommandArgument::Url {
                name: old_name,
                schemes: old_schemes,
            },
        ) => new_name == old_name && new_schemes.is_subset(old_schemes),
        _ => false,
    }
}

fn dbus_arguments_subset(new: &[DbusArgumentConstraint], old: &[DbusArgumentConstraint]) -> bool {
    old.iter().all(|approved| {
        new.iter().any(|requested| {
            requested.index == approved.index
                && match (
                    &requested.equals_string,
                    &requested.one_of_strings,
                    &approved.equals_string,
                    &approved.one_of_strings,
                ) {
                    (Some(new), None, Some(old), None) => new == old,
                    (Some(new), None, None, Some(old)) => old.contains(new),
                    (None, Some(new), None, Some(old)) => new.is_subset(old),
                    _ => false,
                }
        })
    })
}

fn dotted_name_subset(new: &str, old: &str) -> bool {
    match old.strip_suffix(".*") {
        Some(prefix) => {
            new == old
                || new
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        }
        None => new == old,
    }
}

fn origins_subset(new: &BTreeSet<HttpOriginRule>, old: &BTreeSet<HttpOriginRule>) -> bool {
    new.iter().all(|origin| {
        old.iter().any(|approved_origin| {
            origin.scheme == approved_origin.scheme
                && origin.host == approved_origin.host
                && origin.port == approved_origin.port
                && path_prefixes_subset(&origin.path_prefixes, &approved_origin.path_prefixes)
        })
    })
}

fn bool_authority_subset(new: bool, old: bool) -> bool {
    !new || old
}

fn activation_requirement_subset(new: bool, old: bool) -> bool {
    !old || new
}

fn path_prefixes_subset(new: &BTreeSet<String>, old: &BTreeSet<String>) -> bool {
    if old.is_empty() {
        return true;
    }
    !new.is_empty()
        && new
            .iter()
            .all(|path| old.iter().any(|prefix| path.starts_with(prefix)))
}

fn mount_labels_subset(
    new: &BTreeSet<FilesystemMountRequest>,
    old: &BTreeSet<FilesystemMountRequest>,
) -> bool {
    new.iter()
        .all(|mount| old.iter().any(|approved| approved.label == mount.label))
}

fn endpoint_labels_subset(
    new: &BTreeSet<LocalEndpointRequest>,
    old: &BTreeSet<LocalEndpointRequest>,
) -> bool {
    new.iter().all(|endpoint| {
        old.iter().any(|approved| {
            approved.label == endpoint.label && approved.protocol == endpoint.protocol
        })
    })
}

fn validate_mounts(mounts: &BTreeSet<FilesystemMountRequest>, issue: &mut impl FnMut(String)) {
    require_nonempty(mounts, "mounts", issue);
    require_collection_limit(mounts.len(), 32, "mounts", issue);
    let mut labels = BTreeSet::new();
    for mount in mounts {
        if !valid_kebab_id(&mount.label) {
            issue(format!("invalid mount label {}", mount.label));
        }
        if !labels.insert(&mount.label) {
            issue(format!("duplicate mount label {}", mount.label));
        }
    }
}

fn validate_http_origin(origin: &HttpOriginRule, issue: &mut impl FnMut(String)) {
    if origin.scheme != "https" && origin.scheme != "http" {
        issue(format!("unsupported HTTP scheme {}", origin.scheme));
    }
    if origin.host.is_empty()
        || !origin.host.is_ascii()
        || origin.host != origin.host.to_ascii_lowercase()
        || !valid_canonical_http_host(&origin.host)
    {
        issue(format!("HTTP host {} is not canonical", origin.host));
    }
    if origin.port == 0 {
        issue("HTTP origin port must be nonzero".into());
    }
    for prefix in &origin.path_prefixes {
        if !prefix.starts_with('/')
            || prefix.contains(['?', '#'])
            || prefix.contains(['%', '\\'])
            || prefix.contains("//")
            || prefix
                .split('/')
                .any(|component| matches!(component, "." | ".."))
            || (prefix != "/" && !prefix.ends_with('/'))
        {
            issue(format!(
                "HTTP path prefix {prefix} must be an absolute directory prefix ending in /"
            ));
        }
    }
}

fn valid_canonical_http_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if host.len() > 253
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains("..")
        || host
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
    {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn validate_dbus_rule(
    destination: &str,
    path: &str,
    interface: &str,
    member: &str,
    issue: &mut impl FnMut(String),
) {
    if destination.starts_with(':')
        || !(valid_dotted_name(destination)
            || destination
                .strip_suffix(".*")
                .is_some_and(valid_dotted_name))
    {
        issue(format!("invalid D-Bus well-known name {destination}"));
    }
    if !path.starts_with('/') || path.contains("//") || path.contains('*') {
        issue(format!("invalid D-Bus object path {path}"));
    }
    if !valid_dotted_name(interface) || !valid_member(member) {
        issue(format!(
            "invalid D-Bus interface or member {interface}.{member}"
        ));
    }
}

fn validate_command_argument(argument: &CommandArgument, issue: &mut impl FnMut(String)) {
    match argument {
        CommandArgument::Literal { value } => {
            if value.len() > 4096 || value.contains('\0') {
                issue("invalid literal command argument".into());
            }
        }
        CommandArgument::BoundedInteger {
            name,
            minimum,
            maximum,
        } => {
            if !valid_kebab_id(name) || minimum > maximum {
                issue(format!("invalid bounded integer argument {name}"));
            }
        }
        CommandArgument::FixedEnum { name, values } => {
            if !valid_kebab_id(name)
                || values.is_empty()
                || values.len() > 64
                || values
                    .iter()
                    .any(|value| value.len() > 4096 || value.contains('\0'))
            {
                issue(format!("invalid fixed enum argument {name}"));
            }
        }
        CommandArgument::BoundedText {
            name,
            maximum_bytes,
        } => {
            if !valid_kebab_id(name) || *maximum_bytes == 0 || *maximum_bytes > 4096 {
                issue(format!("invalid bounded text argument {name}"));
            }
        }
        CommandArgument::ApprovedFile { name, mount } => {
            if !valid_kebab_id(name) || !valid_kebab_id(mount) {
                issue(format!("invalid approved file argument {name}"));
            }
        }
        CommandArgument::Url { name, schemes } => {
            if !valid_kebab_id(name)
                || schemes.is_empty()
                || !schemes.iter().all(|v| valid_scheme(v))
            {
                issue(format!("invalid URL argument {name}"));
            }
        }
    }
}

fn command_argument_name(argument: &CommandArgument) -> Option<&str> {
    match argument {
        CommandArgument::Literal { .. } => None,
        CommandArgument::BoundedInteger { name, .. }
        | CommandArgument::FixedEnum { name, .. }
        | CommandArgument::BoundedText { name, .. }
        | CommandArgument::ApprovedFile { name, .. }
        | CommandArgument::Url { name, .. } => Some(name),
    }
}

fn dangerous_environment_name(name: &str) -> bool {
    name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || matches!(
            name,
            "GCONV_PATH" | "GETCONF_DIR" | "GLIBC_TUNABLES" | "HOSTALIASES" | "LOCPATH"
        )
}

fn command_is_interpreter(command: &CommandRule) -> bool {
    let Some(name) = Path::new(&command.executable)
        .file_name()
        .and_then(|value| value.to_str())
    else {
        return true;
    };
    matches!(
        name,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "env"
            | "python"
            | "python3"
            | "node"
            | "ruby"
            | "perl"
    )
}

fn context_fact_is_public(fact: &str) -> bool {
    matches!(
        fact,
        "application.id"
            | "workspace.id"
            | "monitor.id"
            | "recording.active"
            | "media.playback-status"
            | "power.profile"
            | "power.on-battery"
    )
}

fn require_nonempty<T>(values: &BTreeSet<T>, name: &str, issue: &mut impl FnMut(String)) {
    if values.is_empty() {
        issue(format!("{name} must not be empty"));
    }
}

fn require_nonzero<T>(value: T, name: &str, issue: &mut impl FnMut(String))
where
    T: Default + PartialEq,
{
    if value == T::default() {
        issue(format!("{name} must be nonzero"));
    }
}

fn require_at_most<T>(value: T, maximum: T, name: &str, issue: &mut impl FnMut(String))
where
    T: fmt::Display + PartialOrd,
{
    if value > maximum {
        issue(format!("{name} exceeds host maximum {maximum}"));
    }
}

fn require_collection_limit(
    length: usize,
    maximum: usize,
    name: &str,
    issue: &mut impl FnMut(String),
) {
    if length > maximum {
        issue(format!(
            "{name} contains {length} entries; maximum is {maximum}"
        ));
    }
}

fn validate_rate(value: u16, issue: &mut impl FnMut(String)) {
    if value == 0 {
        issue("rate must be nonzero".into());
    }
}

fn validate_kebab_set(values: &BTreeSet<String>, label: &str, issue: &mut impl FnMut(String)) {
    for value in values {
        if !valid_kebab_id(value) {
            issue(format!("invalid {label} {value}"));
        }
    }
}

fn valid_kebab_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_dotted_id(value: &str) -> bool {
    value.len() <= 128 && value.split('.').all(valid_kebab_id) && value.contains('.')
}

fn valid_dotted_name(value: &str) -> bool {
    value.contains('.')
        && value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
}

fn valid_member(value: &str) -> bool {
    !value.is_empty()
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && (value.as_bytes()[0].is_ascii_alphabetic() || value.as_bytes()[0] == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_scheme(value: &str) -> bool {
    !value.is_empty()
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+-.".contains(&byte)
        })
}

const fn default_command_output() -> u64 {
    64 * 1024
}

const fn default_command_timeout() -> u32 {
    5_000
}
