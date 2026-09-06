use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};

use semver::Version;
use tempfile::tempdir;
use touchbar_package::{GithubSource, PluginManifest};
use touchbar_policy::{
    CapabilityId, CapabilityRegistry, CapabilityRequest, CapabilityScope, CapabilityStatus,
    CommandArgument, CommandRule, CommandRunScope, DbusArgumentConstraint, DbusBus, DbusCallRule,
    DbusCallScope, Decision, EffectivePolicy, FilesystemMountBinding, FilesystemMountRequest,
    FilesystemReadScope, GrantRecord, GrantStore, HttpMethod, HttpOriginRule, HttpRequestScope,
    PackageInstance, PermissionChangeKind, Provenance, ReusePolicy, RiskClass, RuntimeKind,
    SessionGrants, UriOpenScope, calculate_effective_policy, diff_permissions,
    normalize_manifest_permissions, summarize_trust, validate_capability_scope,
};

fn source() -> GithubSource {
    GithubSource::new("alice", "touchbar-media").unwrap()
}

fn digest(character: char) -> String {
    format!("sha256:{}", character.to_string().repeat(64))
}

#[test]
fn concurrent_consent_updates_do_not_lose_unrelated_records() {
    let temporary = tempdir().unwrap();
    let path = temporary.path().join("private/permissions.toml");
    let first = grant(
        CapabilityId::HttpRequestV1,
        https_scope("/first/", false, 4096),
        Decision::Allow,
        ReusePolicy::ExactDigest,
    );
    let mut second = first.clone();
    second.source = GithubSource::new("bob", "touchbar-weather").unwrap();
    second.approved_scope = https_scope("/second/", false, 4096);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    std::thread::scope(|scope| {
        for record in [first, second] {
            let barrier = barrier.clone();
            let path = path.clone();
            scope.spawn(move || {
                barrier.wait();
                GrantStore::update_record(path, record).unwrap();
            });
        }
        barrier.wait();
    });
    let stored = GrantStore::load(path).unwrap();
    assert_eq!(stored.records().count(), 2);
}

fn mount_binding(path: impl Into<PathBuf>) -> FilesystemMountBinding {
    FilesystemMountBinding {
        path: path.into(),
        device: 1,
        inode: 1,
    }
}

fn manifest_with_permissions(permissions: &str) -> PluginManifest {
    PluginManifest::from_toml(&format!(
        r#"
manifest_version = 1

[plugin]
name = "Policy Test"
version = "1.2.3"
description = "Policy test fixture"
license = "MIT"
source = "github:alice/touchbar-media"
api = "^1.0"

[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/plugin@1.0.0"

[[items]]
id = "media"
label = "Media"

{permissions}
"#
    ))
    .unwrap()
}

fn request(capability: CapabilityId, scope: CapabilityScope, required: bool) -> CapabilityRequest {
    CapabilityRequest {
        capability,
        required,
        reason: "Needed for the test".into(),
        scope,
    }
}

fn https_scope(path: &str, private_network: bool, maximum_response_bytes: u64) -> CapabilityScope {
    CapabilityScope::HttpRequest(HttpRequestScope {
        origins: BTreeSet::from([HttpOriginRule {
            scheme: "https".into(),
            host: "api.example.com".into(),
            port: 443,
            path_prefixes: BTreeSet::from([path.into()]),
        }]),
        methods: BTreeSet::from([HttpMethod::Get]),
        private_network,
        maximum_request_bytes: 1024,
        maximum_response_bytes,
        maximum_requests_per_minute: 10,
    })
}

fn package(provenance: Provenance, current_digest: String) -> PackageInstance {
    PackageInstance {
        source: source(),
        version: Version::new(1, 3, 0),
        digest: current_digest,
        provenance,
        runtime: RuntimeKind::Component,
    }
}

fn grant(
    capability: CapabilityId,
    scope: CapabilityScope,
    decision: Decision,
    reuse: ReusePolicy,
) -> GrantRecord {
    let filesystem_mounts = if decision == Decision::Allow {
        match &scope {
            CapabilityScope::FilesystemRead(scope) => scope
                .mounts
                .iter()
                .map(|mount| {
                    (
                        mount.label.clone(),
                        mount_binding(format!("/approved/{}", mount.label)),
                    )
                })
                .collect(),
            _ => BTreeMap::new(),
        }
    } else {
        BTreeMap::new()
    };
    GrantRecord {
        source: source(),
        capability,
        approved_scope: scope,
        bindings: touchbar_policy::GrantBindings {
            filesystem_mounts,
            ..Default::default()
        },
        decision,
        reuse,
        approved_version: Version::new(1, 2, 3),
        approved_digest: digest('a'),
    }
}

fn supported(capability: CapabilityId) -> CapabilityRegistry {
    CapabilityRegistry::from_supported([capability])
}

#[test]
fn manifest_permissions_become_typed_sorted_requests() {
    let manifest = manifest_with_permissions(
        r#"
[[permission]]
capability = "http.request.v1"
required = false
reason = "  Read public playback metadata  "
[permission.scope]
methods = ["GET"]
maximum_request_bytes = 1024
maximum_response_bytes = 4096
maximum_requests_per_minute = 5
[[permission.scope.origins]]
scheme = "https"
host = "api.example.com"
port = 443
path_prefixes = ["/v1/media/"]

[[permission]]
capability = "context.read.v1"
required = true
reason = "Adapt to the focused app"
[permission.scope]
facts = ["application.id", "workspace.id"]
maximum_updates_per_second = 4
"#,
    );

    let requests = normalize_manifest_permissions(&manifest).unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].capability, CapabilityId::ContextReadV1);
    assert_eq!(requests[1].capability, CapabilityId::HttpRequestV1);
    assert_eq!(requests[1].reason, "Read public playback metadata");
    assert!(matches!(requests[1].scope, CapabilityScope::HttpRequest(_)));
}

#[test]
fn known_scopes_reject_unknown_fields_and_duplicates() {
    let malformed = manifest_with_permissions(
        r#"
[[permission]]
capability = "context.read.v1"
required = false
reason = "Context"
[permission.scope]
facts = ["application.id"]
surprise = true
"#,
    );
    let errors = normalize_manifest_permissions(&malformed).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("unknown field"))
    );

    let duplicate = manifest_with_permissions(
        r#"
[[permission]]
capability = "context.read.v1"
required = false
reason = "One"
[permission.scope]
facts = ["application.id"]

[[permission]]
capability = "context.read.v1"
required = true
reason = "Two"
[permission.scope]
facts = ["workspace.id"]
"#,
    );
    let errors = normalize_manifest_permissions(&duplicate).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.message == "duplicate capability request")
    );
}

#[test]
fn scope_budgets_are_rejected_above_host_maxima() {
    let manifest = manifest_with_permissions(
        r#"
[[permission]]
capability = "context.read.v1"
required = false
reason = "Context"
[permission.scope]
facts = ["application.id"]
maximum_updates_per_second = 61
"#,
    );
    let errors = normalize_manifest_permissions(&manifest).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.message.contains("host maximum 60"))
    );
}

#[test]
fn unknown_optional_capabilities_are_preserved_but_unsupported() {
    let manifest = manifest_with_permissions(
        r#"
[[permission]]
capability = "calendar.read.v9"
required = false
reason = "Future calendar integration"
[permission.scope]
calendars = ["work"]
"#,
    );
    let requests = normalize_manifest_permissions(&manifest).unwrap();
    assert_eq!(
        requests[0].capability,
        CapabilityId::Unknown("calendar.read.v9".into())
    );
    let policy = calculate_effective_policy(
        &package(Provenance::VerifiedRelease, digest('a')),
        &requests,
        &GrantStore::default(),
        &SessionGrants::default(),
        &CapabilityRegistry::default(),
    );
    assert_eq!(policy.grants[0].status, CapabilityStatus::Unsupported);
    assert!(!policy.blocked);
}

#[test]
fn http_subset_requires_narrower_paths_limits_and_network_access() {
    let approved = https_scope("/v1/", true, 4096);
    let narrower = https_scope("/v1/media/", false, 1024);
    assert!(narrower.is_subset_of(&approved));
    assert!(!approved.is_subset_of(&narrower));

    let outside = https_scope("/admin/", false, 1024);
    assert!(!outside.is_subset_of(&approved));
}

#[test]
fn http_origins_reject_ambiguous_hosts_and_server_dependent_paths() {
    for (host, prefix) in [
        ("EXAMPLE.com", "/v1/"),
        ("example.com.", "/v1/"),
        ("127.1", "/v1/"),
        ("-api.example.com", "/v1/"),
        ("api..example.com", "/v1/"),
        ("api.example.com", "/v1/%2e%2e/"),
        ("api.example.com", "/v1//private/"),
    ] {
        let manifest = manifest_with_permissions(&format!(
            r#"
[[permission]]
capability = "http.request.v1"
required = false
reason = "HTTP validation"
[permission.scope]
methods = ["GET"]
maximum_request_bytes = 1024
maximum_response_bytes = 1024
maximum_requests_per_minute = 1
[[permission.scope.origins]]
scheme = "https"
host = "{host}"
port = 443
path_prefixes = ["{prefix}"]
"#
        ));
        assert!(
            normalize_manifest_permissions(&manifest).is_err(),
            "{host} {prefix}"
        );
    }
}

#[test]
fn filesystem_hints_do_not_change_authority_but_new_labels_do() {
    let scope = |label: &str, hint: &str| {
        CapabilityScope::FilesystemRead(FilesystemReadScope {
            mounts: BTreeSet::from([FilesystemMountRequest {
                label: label.into(),
                suggested_location: Some(hint.into()),
            }]),
            ..FilesystemReadScope::default()
        })
    };
    assert!(scope("library", "xdg:music").is_subset_of(&scope("library", "xdg:documents")));
    assert!(!scope("private", "xdg:music").is_subset_of(&scope("library", "xdg:music")));
}

#[test]
fn dbus_permission_is_a_complete_rule_and_activation_can_only_strengthen() {
    let rule = |requires_user_activation| DbusCallRule {
        bus: DbusBus::Session,
        destination: "org.mpris.MediaPlayer2.*".into(),
        path: "/org/mpris/MediaPlayer2".into(),
        interface: "org.mpris.MediaPlayer2.Player".into(),
        member: "PlayPause".into(),
        signature: String::new(),
        arguments: Vec::new(),
        allow_service_activation: false,
        requires_user_activation,
    };
    let approved = CapabilityScope::DbusCall(DbusCallScope {
        rules: BTreeSet::from([rule(false)]),
    });
    let safer = CapabilityScope::DbusCall(DbusCallScope {
        rules: BTreeSet::from([rule(true)]),
    });
    assert!(safer.is_subset_of(&approved));
    assert!(!approved.is_subset_of(&safer));
}

#[test]
fn dbus_wildcards_and_argument_sets_can_narrow_without_new_consent() {
    let rule = |destination: &str, arguments| DbusCallRule {
        bus: DbusBus::Session,
        destination: destination.into(),
        path: "/org/mpris/MediaPlayer2".into(),
        interface: "org.freedesktop.DBus.Properties".into(),
        member: "Get".into(),
        signature: "ss".into(),
        arguments,
        allow_service_activation: false,
        requires_user_activation: false,
    };
    let approved = CapabilityScope::DbusCall(DbusCallScope {
        rules: BTreeSet::from([rule(
            "org.mpris.MediaPlayer2.*",
            vec![DbusArgumentConstraint {
                index: 1,
                equals_string: None,
                one_of_strings: Some(BTreeSet::from(["Metadata".into(), "PlaybackStatus".into()])),
            }],
        )]),
    });
    let narrower = CapabilityScope::DbusCall(DbusCallScope {
        rules: BTreeSet::from([rule(
            "org.mpris.MediaPlayer2.spotify",
            vec![DbusArgumentConstraint {
                index: 1,
                equals_string: Some("PlaybackStatus".into()),
                one_of_strings: None,
            }],
        )]),
    });
    assert!(narrower.is_subset_of(&approved));
    assert!(!approved.is_subset_of(&narrower));
}

#[test]
fn command_parameters_and_uri_paths_use_semantic_subset_rules() {
    let command = |minimum, maximum| {
        CapabilityScope::CommandRun(CommandRunScope {
            commands: BTreeSet::from([CommandRule {
                id: "set-level".into(),
                executable: "/usr/bin/wpctl".into(),
                arguments: vec![CommandArgument::BoundedInteger {
                    name: "level".into(),
                    minimum,
                    maximum,
                }],
                environment: BTreeMap::new(),
                working_directory: None,
                maximum_output_bytes: 1024,
                timeout_milliseconds: 1000,
            }]),
            maximum_parallel_processes: 1,
        })
    };
    assert!(command(10, 90).is_subset_of(&command(0, 100)));
    assert!(!command(0, 100).is_subset_of(&command(10, 90)));

    let uri = |path| {
        CapabilityScope::UriOpen(UriOpenScope {
            schemes: BTreeSet::from(["https".into()]),
            origins: match https_scope(path, false, 1024) {
                CapabilityScope::HttpRequest(scope) => scope.origins,
                _ => unreachable!(),
            },
        })
    };
    assert!(uri("/docs/api/").is_subset_of(&uri("/docs/")));
    assert!(!uri("/admin/").is_subset_of(&uri("/docs/")));
}

#[test]
fn command_templates_reject_confusable_ids_slots_paths_and_loader_environment() {
    let rule = |executable: &str| CommandRule {
        id: "run".into(),
        executable: executable.into(),
        arguments: vec![
            CommandArgument::BoundedText {
                name: "value".into(),
                maximum_bytes: 16,
            },
            CommandArgument::FixedEnum {
                name: "value".into(),
                values: BTreeSet::from(["safe".into()]),
            },
        ],
        environment: BTreeMap::from([("LD_PRELOAD".into(), "/tmp/evil.so".into())]),
        working_directory: Some("/tmp/../tmp".into()),
        maximum_output_bytes: 1024,
        timeout_milliseconds: 1000,
    };
    let scope = CapabilityScope::CommandRun(CommandRunScope {
        commands: BTreeSet::from([rule("/usr/bin/one"), rule("/usr/bin/two")]),
        maximum_parallel_processes: 1,
    });
    let errors = validate_capability_scope(&CapabilityId::CommandRunV1, &scope)
        .unwrap_err()
        .into_iter()
        .map(|error| error.message)
        .collect::<Vec<_>>();
    assert!(errors.iter().any(|error| error.contains("command id")));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("duplicate command argument"))
    );
    assert!(errors.iter().any(|error| error.contains("environment")));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("working_directory"))
    );
}

#[test]
fn command_approved_files_require_exact_user_owned_mount_bindings() {
    let scope = CapabilityScope::CommandRun(CommandRunScope {
        commands: BTreeSet::from([CommandRule {
            id: "inspect".into(),
            executable: "/usr/bin/file".into(),
            arguments: vec![CommandArgument::ApprovedFile {
                name: "target".into(),
                mount: "gallery".into(),
            }],
            environment: BTreeMap::new(),
            working_directory: None,
            maximum_output_bytes: 1024,
            timeout_milliseconds: 1000,
        }]),
        maximum_parallel_processes: 1,
    });
    let record = |filesystem_mounts| GrantRecord {
        source: source(),
        capability: CapabilityId::CommandRunV1,
        approved_scope: scope.clone(),
        bindings: touchbar_policy::GrantBindings {
            filesystem_mounts,
            ..Default::default()
        },
        decision: Decision::Allow,
        reuse: ReusePolicy::ExactDigest,
        approved_version: Version::new(1, 0, 0),
        approved_digest: digest('a'),
    };
    assert!(record(BTreeMap::new()).validate().is_err());
    assert!(
        record(BTreeMap::from([(
            "gallery".into(),
            mount_binding("/srv/gallery")
        )]))
        .validate()
        .is_ok()
    );
}

#[test]
fn verified_updates_reuse_narrow_grants_but_unverified_updates_do_not() {
    let approved = https_scope("/v1/", false, 4096);
    let requested = request(
        CapabilityId::HttpRequestV1,
        https_scope("/v1/media/", false, 1024),
        true,
    );
    let mut store = GrantStore::default();
    store
        .insert(grant(
            CapabilityId::HttpRequestV1,
            approved,
            Decision::Allow,
            ReusePolicy::VerifiedSameSource,
        ))
        .unwrap();
    let verified = calculate_effective_policy(
        &package(Provenance::VerifiedRelease, digest('b')),
        std::slice::from_ref(&requested),
        &store,
        &SessionGrants::default(),
        &supported(CapabilityId::HttpRequestV1),
    );
    assert_eq!(verified.grants[0].status, CapabilityStatus::Granted);
    assert!(!verified.blocked);

    let unverified = calculate_effective_policy(
        &package(Provenance::UnverifiedRelease, digest('b')),
        &[requested],
        &store,
        &SessionGrants::default(),
        &supported(CapabilityId::HttpRequestV1),
    );
    assert_eq!(unverified.grants[0].status, CapabilityStatus::NeedsConsent);
    assert!(unverified.blocked);
}

#[test]
fn exact_digest_session_decision_overrides_persistent_denial() {
    let scope = https_scope("/v1/", false, 4096);
    let request = request(CapabilityId::HttpRequestV1, scope.clone(), true);
    let mut persistent = GrantStore::default();
    persistent
        .insert(grant(
            CapabilityId::HttpRequestV1,
            scope.clone(),
            Decision::Deny,
            ReusePolicy::ExactDigest,
        ))
        .unwrap();
    let mut session = SessionGrants::default();
    session
        .insert(grant(
            CapabilityId::HttpRequestV1,
            scope,
            Decision::Allow,
            ReusePolicy::ExactDigest,
        ))
        .unwrap();
    let policy = calculate_effective_policy(
        &package(Provenance::LocalDevelopment, digest('a')),
        &[request],
        &persistent,
        &session,
        &supported(CapabilityId::HttpRequestV1),
    );
    assert_eq!(policy.grants[0].status, CapabilityStatus::Granted);
    assert!(policy.grants[0].from_session);
}

#[test]
fn permission_diff_distinguishes_narrowing_expansion_and_requirement_changes() {
    let base = request(
        CapabilityId::HttpRequestV1,
        https_scope("/v1/", false, 4096),
        false,
    );
    let narrower = request(
        CapabilityId::HttpRequestV1,
        https_scope("/v1/media/", false, 1024),
        false,
    );
    assert_eq!(
        diff_permissions(std::slice::from_ref(&base), std::slice::from_ref(&narrower))[0].kind,
        PermissionChangeKind::Narrowed
    );
    assert_eq!(
        diff_permissions(std::slice::from_ref(&narrower), std::slice::from_ref(&base))[0].kind,
        PermissionChangeKind::Expanded
    );
    let required = CapabilityRequest {
        required: true,
        ..base.clone()
    };
    assert_eq!(
        diff_permissions(&[base], &[required])[0].kind,
        PermissionChangeKind::RequirementChanged
    );
}

#[test]
fn risk_is_host_derived_and_highlights_exfiltration_and_interpreters() {
    let network = request(
        CapabilityId::HttpRequestV1,
        https_scope("/", false, 4096),
        false,
    );
    let files = request(
        CapabilityId::FilesystemReadV1,
        CapabilityScope::FilesystemRead(FilesystemReadScope {
            mounts: BTreeSet::from([FilesystemMountRequest {
                label: "library".into(),
                suggested_location: None,
            }]),
            ..FilesystemReadScope::default()
        }),
        false,
    );
    let summary = summarize_trust(RuntimeKind::Component, [&network, &files]);
    assert_eq!(summary.class, RiskClass::Sensitive);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning.contains("egress"))
    );

    let shell = request(
        CapabilityId::CommandRunV1,
        CapabilityScope::CommandRun(CommandRunScope {
            commands: BTreeSet::from([CommandRule {
                id: "script".into(),
                executable: "/usr/bin/bash".into(),
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                working_directory: None,
                maximum_output_bytes: 1024,
                timeout_milliseconds: 1000,
            }]),
            maximum_parallel_processes: 1,
        }),
        true,
    );
    assert_eq!(
        summarize_trust(RuntimeKind::Component, [&shell]).class,
        RiskClass::EffectivelyTrusted
    );
    assert_eq!(
        summarize_trust(RuntimeKind::Native, [&network]).class,
        RiskClass::UnrestrictedNative
    );
}

#[test]
fn native_permissions_are_disclosure_only_and_do_not_block_launch() {
    let request = request(
        CapabilityId::HttpRequestV1,
        https_scope("/", false, 1024),
        true,
    );
    let mut native = package(Provenance::VerifiedRelease, digest('a'));
    native.runtime = RuntimeKind::Native;
    let policy = calculate_effective_policy(
        &native,
        &[request],
        &GrantStore::default(),
        &SessionGrants::default(),
        &supported(CapabilityId::HttpRequestV1),
    );
    assert_eq!(policy.grants[0].status, CapabilityStatus::DisclosureOnly);
    assert!(!policy.blocked);
    assert_eq!(policy.trust.class, RiskClass::UnrestrictedNative);
}

#[test]
fn grant_store_round_trips_atomically_with_private_permissions() {
    let directory = tempdir().unwrap();
    let policy_directory = directory.path().join("touchbar");
    let path = policy_directory.join("permissions.toml");
    let mut store = GrantStore::default();
    store
        .insert(grant(
            CapabilityId::HttpRequestV1,
            https_scope("/v1/", false, 4096),
            Decision::Allow,
            ReusePolicy::VerifiedSameSource,
        ))
        .unwrap();
    store.save(&path).unwrap();

    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(GrantStore::load(&path).unwrap(), store);
    assert_eq!(
        GrantStore::from_toml(&store.to_toml().unwrap()).unwrap(),
        store
    );
}

#[test]
fn filesystem_grants_require_exact_host_owned_normalized_mount_bindings() {
    let scope = CapabilityScope::FilesystemRead(FilesystemReadScope {
        mounts: BTreeSet::from([FilesystemMountRequest {
            label: "gallery".into(),
            suggested_location: Some("Pictures".into()),
        }]),
        kinds: BTreeSet::from([touchbar_policy::FileKind::RegularFile]),
        maximum_file_bytes: 4096,
        enumerate: false,
    });
    let record = grant(
        CapabilityId::FilesystemReadV1,
        scope,
        Decision::Allow,
        ReusePolicy::ExactDigest,
    );
    assert_eq!(
        record.bindings.filesystem_mounts["gallery"].path,
        PathBuf::from("/approved/gallery")
    );
    let mut store = GrantStore::default();
    store.insert(record.clone()).unwrap();
    assert_eq!(
        GrantStore::from_toml(&store.to_toml().unwrap()).unwrap(),
        store
    );

    let mut missing = record.clone();
    missing.bindings.filesystem_mounts.clear();
    assert!(GrantStore::default().insert(missing).is_err());

    let mut relative = record.clone();
    relative
        .bindings
        .filesystem_mounts
        .insert("gallery".into(), mount_binding("relative/gallery"));
    assert!(GrantStore::default().insert(relative).is_err());

    let mut parent = record.clone();
    parent
        .bindings
        .filesystem_mounts
        .insert("gallery".into(), mount_binding("/approved/../escape"));
    assert!(GrantStore::default().insert(parent).is_err());

    let mut denied = record;
    denied.decision = Decision::Deny;
    assert!(GrantStore::default().insert(denied).is_err());
}

#[test]
fn secret_grants_require_exact_host_owned_item_bindings() {
    use touchbar_policy::{GrantBindings, SecretBinding, SecretReadScope};

    let scope = CapabilityScope::SecretRead(SecretReadScope {
        logical_names: BTreeSet::from(["github-token".into(), "weather-key".into()]),
    });
    let make = |secrets| GrantRecord {
        source: source(),
        capability: CapabilityId::SecretReadV1,
        approved_scope: scope.clone(),
        bindings: GrantBindings {
            secrets,
            ..Default::default()
        },
        decision: Decision::Allow,
        reuse: ReusePolicy::ExactDigest,
        approved_version: Version::new(1, 0, 0),
        approved_digest: digest('a'),
    };
    let valid = BTreeMap::from([
        (
            "github-token".into(),
            SecretBinding::SecretServiceItem {
                object_path: "/org/freedesktop/secrets/collection/login/1".into(),
            },
        ),
        (
            "weather-key".into(),
            SecretBinding::SecretServiceItem {
                object_path: "/org/freedesktop/secrets/collection/login/2".into(),
            },
        ),
    ]);
    assert!(make(valid.clone()).validate().is_ok());

    let mut missing = valid.clone();
    missing.remove("weather-key");
    assert!(make(missing).validate().is_err());
    let mut extra = valid.clone();
    extra.insert(
        "hidden".into(),
        SecretBinding::SecretServiceItem {
            object_path: "/org/freedesktop/secrets/collection/login/3".into(),
        },
    );
    assert!(make(extra).validate().is_err());
    let mut invalid = valid;
    invalid.insert(
        "github-token".into(),
        SecretBinding::SecretServiceItem {
            object_path: "/org/freedesktop/secrets/../escape".into(),
        },
    );
    assert!(make(invalid).validate().is_err());
}

#[test]
fn local_grants_require_exact_host_owned_unix_socket_bindings() {
    use touchbar_policy::{
        GrantBindings, LocalConnectScope, LocalEndpointBinding, LocalEndpointRequest,
    };

    let scope = CapabilityScope::LocalConnect(LocalConnectScope {
        endpoints: BTreeSet::from([LocalEndpointRequest {
            label: "music-player".into(),
            protocol: "mpris.bridge.v1".into(),
            suggested_endpoint: Some("/run/user/1000/player.sock".into()),
        }]),
        maximum_frame_bytes: 4096,
        maximum_bytes_per_minute: 65_536,
    });
    let make = |local_endpoints| GrantRecord {
        source: source(),
        capability: CapabilityId::LocalConnectV1,
        approved_scope: scope.clone(),
        bindings: GrantBindings {
            local_endpoints,
            ..Default::default()
        },
        decision: Decision::Allow,
        reuse: ReusePolicy::ExactDigest,
        approved_version: Version::new(1, 0, 0),
        approved_digest: digest('a'),
    };
    assert!(
        make(BTreeMap::from([(
            "music-player".into(),
            LocalEndpointBinding::UnixStream {
                path: "/run/user/1000/player.sock".into(),
            },
        )]))
        .validate()
        .is_ok()
    );
    assert!(make(BTreeMap::new()).validate().is_err());
    assert!(
        make(BTreeMap::from([(
            "music-player".into(),
            LocalEndpointBinding::UnixStream {
                path: "/run/user/1000/../escape.sock".into(),
            },
        )]))
        .validate()
        .is_err()
    );
}

#[test]
fn clipboard_grants_require_one_exact_host_owned_compositor_socket() {
    use touchbar_policy::{ClipboardBinding, ClipboardScope, GrantBindings};

    let scope = CapabilityScope::Clipboard(ClipboardScope {
        mime_types: BTreeSet::from(["text/plain;charset=utf-8".into()]),
        maximum_bytes: 4096,
        maximum_operations_per_minute: 6,
    });
    let make = |capability, clipboard| GrantRecord {
        source: source(),
        capability,
        approved_scope: scope.clone(),
        bindings: GrantBindings {
            clipboard,
            ..Default::default()
        },
        decision: Decision::Allow,
        reuse: ReusePolicy::ExactDigest,
        approved_version: Version::new(1, 0, 0),
        approved_digest: digest('a'),
    };
    let valid = Some(ClipboardBinding::WaylandDataControl {
        socket: "/run/user/1000/wayland-1".into(),
    });
    assert!(
        make(CapabilityId::ClipboardReadV1, valid.clone())
            .validate()
            .is_ok()
    );
    assert!(
        make(CapabilityId::ClipboardWriteV1, valid)
            .validate()
            .is_ok()
    );
    assert!(
        make(CapabilityId::ClipboardReadV1, None)
            .validate()
            .is_err()
    );
    assert!(
        make(
            CapabilityId::ClipboardReadV1,
            Some(ClipboardBinding::WaylandDataControl {
                socket: "/run/user/1000/../attacker".into(),
            }),
        )
        .validate()
        .is_err()
    );
    assert!(
        grant(
            CapabilityId::HttpRequestV1,
            https_scope("/", false, 1024),
            Decision::Allow,
            ReusePolicy::ExactDigest,
        )
        .validate()
        .is_ok()
    );
    let mut smuggled = grant(
        CapabilityId::HttpRequestV1,
        https_scope("/", false, 1024),
        Decision::Allow,
        ReusePolicy::ExactDigest,
    );
    smuggled.bindings.clipboard = Some(ClipboardBinding::WaylandDataControl {
        socket: "/run/user/1000/wayland-1".into(),
    });
    assert!(smuggled.validate().is_err());
}

#[test]
fn grants_reject_cross_capability_authority_and_denied_authority() {
    use touchbar_policy::{GrantBindings, SecretBinding};

    let mut record = grant(
        CapabilityId::HttpRequestV1,
        https_scope("/v1/", false, 4096),
        Decision::Allow,
        ReusePolicy::ExactDigest,
    );
    record.bindings = GrantBindings {
        secrets: BTreeMap::from([(
            "token".into(),
            SecretBinding::SecretServiceItem {
                object_path: "/org/freedesktop/secrets/collection/login/1".into(),
            },
        )]),
        ..Default::default()
    };
    assert!(record.validate().is_err());
    record.decision = Decision::Deny;
    assert!(record.validate().is_err());
}

#[test]
fn grant_store_round_trips_forward_compatible_denials() {
    let capability = CapabilityId::Unknown("calendar.read.v9".into());
    let mut store = GrantStore::default();
    store
        .insert(grant(
            capability,
            CapabilityScope::Unknown("calendars = [\"work\"]\n".into()),
            Decision::Deny,
            ReusePolicy::ExactDigest,
        ))
        .unwrap();
    let encoded = store.to_toml().unwrap();
    assert_eq!(GrantStore::from_toml(&encoded).unwrap(), store);
}

#[test]
fn grant_store_rejects_invalid_typed_authority() {
    let invalid = grant(
        CapabilityId::HttpRequestV1,
        CapabilityScope::HttpRequest(HttpRequestScope::default()),
        Decision::Allow,
        ReusePolicy::ExactDigest,
    );
    assert!(GrantStore::default().insert(invalid).is_err());
}

#[test]
fn registry_and_effective_policy_are_machine_readable() {
    let registry = CapabilityRegistry::default();
    assert_eq!(registry.supported().count(), 13);
    assert!(registry.supports(&CapabilityId::HttpRequestV1));
    let omitted = "input.synthesize.v1".parse::<CapabilityId>().unwrap();
    assert!(!omitted.is_known());
    assert!(!registry.supports(&omitted));
    assert!(!registry.supports(&CapabilityId::Unknown("future.api.v2".into())));

    let policy = calculate_effective_policy(
        &package(Provenance::VerifiedRelease, digest('a')),
        &[request(
            CapabilityId::HttpRequestV1,
            https_scope("/", false, 1024),
            false,
        )],
        &GrantStore::default(),
        &SessionGrants::default(),
        &registry,
    );
    let encoded = toml::to_string(&policy).unwrap();
    assert!(encoded.contains("needs-consent"));
    assert_eq!(toml::from_str::<EffectivePolicy>(&encoded).unwrap(), policy);
}

#[test]
fn grant_store_rejects_symlinks_and_broad_file_permissions() {
    let directory = tempdir().unwrap();
    let target = directory.path().join("target.toml");
    fs::write(&target, "schema_version = 1\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let link = directory.path().join("permissions.toml");
    symlink(&target, &link).unwrap();
    assert!(
        GrantStore::load(&link)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );

    fs::remove_file(&link).unwrap();
    fs::rename(&target, &link).unwrap();
    fs::set_permissions(&link, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        GrantStore::load(&link)
            .unwrap_err()
            .to_string()
            .contains("too broad")
    );
}

#[test]
fn effective_policy_exposes_host_owned_blocking_state() {
    let required = request(
        CapabilityId::HttpRequestV1,
        https_scope("/", false, 1024),
        true,
    );
    let optional = CapabilityRequest {
        required: false,
        ..required.clone()
    };
    let calculate = |request: CapabilityRequest| -> EffectivePolicy {
        calculate_effective_policy(
            &package(Provenance::VerifiedRelease, digest('a')),
            &[request],
            &GrantStore::default(),
            &SessionGrants::default(),
            &supported(CapabilityId::HttpRequestV1),
        )
    };
    assert!(calculate(required).blocked);
    assert!(!calculate(optional).blocked);
}

#[test]
fn deterministic_adversarial_manifests_and_grants_never_panic() {
    let valid = r#"
manifest_version = 1
[plugin]
name = "Fuzz Seed"
version = "1.0.0"
description = "seed"
license = "MIT"
source = "github:alice/fuzz-seed"
api = "^1.0"
[runtime]
kind = "component"
entrypoint = "component/plugin.wasm"
world = "touchbar:plugin/plugin@1.0.0"
[[items]]
id = "seed"
label = "Seed"
[[permission]]
capability = "clipboard.read.v1"
required = false
reason = "seed"
[permission.scope]
mime_types = ["text/plain;charset=utf-8"]
maximum_bytes = 1024
maximum_operations_per_minute = 4
"#;
    let mut random = 0xa076_1d64_78bd_642f_u64;
    for index in 0..4096 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let candidate = match index % 4 {
            0 => valid[..index % (valid.len() + 1)].to_owned(),
            1 => {
                let mut bytes = valid.as_bytes().to_vec();
                let offset = (random as usize) % bytes.len();
                bytes[offset] = 0x20 + ((random >> 32) % 95) as u8;
                String::from_utf8(bytes).unwrap()
            }
            _ => {
                let length = (random as usize) % 2048;
                let mut value = String::with_capacity(length);
                for _ in 0..length {
                    random ^= random << 13;
                    random ^= random >> 7;
                    random ^= random << 17;
                    value.push(char::from(0x20 + (random % 95) as u8));
                }
                value
            }
        };
        if let Ok(manifest) = PluginManifest::from_toml(&candidate) {
            let _ = CapabilityRegistry::default().normalize(&manifest);
        }
        let _ = GrantStore::from_toml(&candidate);
        let _ = toml::from_str::<EffectivePolicy>(&candidate);
    }
}
