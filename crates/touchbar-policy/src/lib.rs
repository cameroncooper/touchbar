//! Pure capability policy, grant, update-diff, and persistence primitives.
//!
//! This crate performs no broker I/O. Installers, the plugin supervisor, the
//! control CLI, and tests share it so authorization decisions cannot drift.

mod capability;
mod grant;
mod store;

pub use capability::{
    CapabilityId, CapabilityRegistry, CapabilityRequest, CapabilityScope, CapabilityStatus,
    ClipboardScope, CommandArgument, CommandRule, CommandRunScope, ContextReadScope,
    DbusArgumentConstraint, DbusBus, DbusCallRule, DbusCallScope, DbusSignalRule,
    DbusSubscribeScope, FileKind, FilesystemMountRequest, FilesystemReadScope,
    FilesystemWriteScope, HttpMethod, HttpOriginRule, HttpRequestScope, LocalConnectScope,
    LocalEndpointRequest, NormalizationError, NotificationScope, NotificationUrgency, RiskClass,
    RuntimeKind, SecretReadScope, TrustSummary, UriOpenScope, WriteOperation,
    normalize_manifest_permissions, summarize_trust, validate_capability_scope,
};
pub use grant::{
    ClipboardBinding, Decision, EffectiveGrant, EffectivePolicy, FilesystemMountBinding,
    GrantBindings, GrantRecord, LocalEndpointBinding, PackageInstance, PermissionChange,
    PermissionChangeKind, PersistentGrants, Provenance, ReusePolicy, SecretBinding, SessionGrants,
    calculate_effective_policy, diff_permissions,
};
pub use store::{GrantStore, StoreError};
