# Capability, consent, and broker contract

**Status:** E3 design, broker implementation, and consent control surface
complete, 2026-09-05.

This document defines the security and user-experience contract for granting a
sandboxed TouchBar plugin access to resources outside its WebAssembly
Component. It is a design gate: broker implementation must conform to this
contract or explicitly revise it first.

The objective is not to enumerate every future integration. It is to make new
integrations fit one consistent authority model without turning
`touchbar-plugin-host` or `touchbar-sessiond` into ambiently privileged processes.

## Decisions

1. A package manifest requests authority; it never grants authority.
2. Grants are user-owned policy stored separately from installed packages.
3. `touchbar-sessiond` remains a compositor and is never a filesystem, network, D-Bus,
   command, clipboard, or secret broker.
4. The component host receives no ambient capability merely because a package
   requested it. It submits bounded operations over a private broker channel.
5. The trusted launcher associates that channel with package identity, artifact
   provenance, and effective grants. Plugin-controlled fields are never used to
   authenticate a request.
6. Broker calls are asynchronous. Rendering and hit testing never wait on I/O,
   subprocesses, D-Bus, DNS, or consent UI.
7. Capability scopes have versioned, capability-specific schemas and a defined
   subset relation. Unknown fields fail closed.
8. Permission expansion, capability-version changes, repository identity
   changes, and unverified development updates require new consent.
9. Optional denial is an ordinary runtime state. Required denial prevents the
   affected pack from activating and produces a host-owned explanation.
10. Broad command, filesystem, D-Bus, local IPC, secret, or input authority can
    make a component effectively trusted. The product must say so plainly and
    must not imply that “Wasm” alone makes such a grant safe.
11. Native plugins remain unrestricted user processes. Their manifest
    permissions are disclosure, not an enforceable sandbox boundary.
12. Broker request and response bodies, secrets, clipboard contents, file
    contents, and D-Bus payloads are never written to ordinary audit logs.

## Goals and non-goals

The first capability system must provide:

- understandable install, update, inspect, deny, and revoke behavior;
- exact authorization at every broker operation;
- required and optional permissions with deterministic degradation;
- narrowly scoped reads, subscriptions, and reversible actions;
- resource limits, cancellation, timeouts, backpressure, and stable errors;
- a language-neutral ABI with strongly typed SDK wrappers;
- machine-readable diagnostics suitable for agents and CI;
- enough audit metadata to answer what happened without recording user data;
- a path to filesystem, HTTP, D-Bus, commands, clipboard, secrets,
  notifications, context, and bounded local IPC.

It does not initially provide:

- a claim that an unrestricted native plugin is sandboxed;
- arbitrary POSIX sockets or general WASI preopens;
- runtime permission prompts initiated at arbitrary moments by a plugin;
- transparent inheritance of browser cookies, proxy variables, SSH agents,
  desktop keyrings, or the user's environment;
- kernel-level isolation as a substitute for operation-level authorization;
- a generic shell API;
- cross-user or system-wide plugin installation.

## Threat model

### Protected assets

- Files and metadata available to the logged-in user.
- Clipboard, secrets, notifications, application state, window titles, and
  other sensitive session data.
- Session and system services reachable through D-Bus or local sockets.
- Network identity, credentials, private network reachability, and bandwidth.
- User input, desktop control, running processes, and system configuration.
- Availability and responsiveness of `touchbar-sessiond`, the GPU path, and other
  plugins.
- The integrity of installed packages, grants, audit records, and updates.

### Adversaries and failures

The design assumes that a component, its repository, an update, returned
network data, or a service it talks to may be malicious. It also covers buggy
plugins, confused-deputy requests, forged package identifiers, path traversal,
symlink races, DNS rebinding, redirect escapes, D-Bus name-owner changes,
argument injection, prompt fatigue, stale grants, response floods, infinite
guest computation, and host-side allocation attacks.

The Wasmtime sandbox and broker implementation are security-critical trusted
code. Kernel, Wasmtime, UI-kit, and broker vulnerabilities remain possible, so
the design minimizes ambient authority and adds process-level defense in depth.
Root compromise, a malicious kernel, and unrestricted native plugins are
outside the component sandbox's protection.

## Authority separation

```text
package manifest ──requests──┐
                            ▼
user consent ─────────── grant store
                            │
installer lock/provenance ──┤
                            ▼
trusted plugin supervisor: normalized effective grants
             │              │
             │ private      └──────── broker backends ── OS/session/network
             │ channel
             ▼
component ─ WIT ─ host ─ private Wayland ─ touchbar-sessiond ─ ADP
```

The trusted plugin supervisor owns launch policy and the grant store. Its
executable name may be chosen during implementation, but it is a separate role
from `touchbar-sessiond`. A first implementation may put the supervisor and broker
router in one process; high-risk or complex backends may later be split into
additional processes without changing the component ABI.

For every component-host launch, the supervisor creates a connected,
close-on-exec `AF_UNIX` `SOCK_SEQPACKET` channel. The supervisor-side connection
record contains the canonical package source, installed artifact digest,
version, provenance state, and normalized effective grants. The host inherits
only its connected endpoint. There is no public broker socket that an arbitrary
same-user process can connect to and self-identify as a plugin.

Peer EOF, hangup, `ECONNRESET`, and `EPIPE` are one normal host-disconnect state;
the supervisor still waits for and returns the actual host exit status. Other
transport failures remain errors and terminate the supervised host.

Linux peer credentials are checked as defense in depth, but a UID is not a
plugin identity. Connection provenance and the supervisor's server-side record
are the authority. The guest never receives an authentication token or chooses
the identity attached to a request.

If a host is started manually, as in the current component demo, it has no
broker channel and observes every non-baseline capability as unavailable.

## Three layers of permission state

1. **Request:** package-authored capability, required flag, reason, and proposed
   scope from `touchbar-plugin.toml`.
2. **Decision:** user-owned allow or deny record, persistence, approved scope,
   and provenance conditions.
3. **Effective grant:** the intersection of the current request, user decision,
   host support, current policy, runtime availability, and provenance rules.

Only the effective grant authorizes an operation. The broker recalculates it
at launch and whenever grants, policy, installation state, or backend
availability changes. It checks the effective grant again on every operation;
the host cannot convert a prior successful check into permanent authority.

## Runtime-provided baseline

The following do not require a manifest permission:

- the selected item ID and compositor-assigned viewport;
- semantic theme tokens, color scheme, theme revision, and motion policy;
- normalized contacts delivered to the plugin's own visible surface;
- bounded monotonic time, timers, random, and closed standard streams;
- bounded plugin-local memory and state;
- structured logging after host redaction and rate limiting;
- capability availability and revocation events.

Wall-clock time may be baseline but must expose only the system clock, not
locale, timezone, environment, or calendar accounts. Focused application IDs,
window titles, project paths, clipboard state, media metadata, recording state,
and similar facts are not silently included in the baseline.

## Capability identifiers and scope schemas

Capability identifiers are lowercase dotted names ending in an interface
version, such as `http.request.v1`. A version change is a different permission,
not an in-place reinterpretation. This works with the existing strict manifest
shape and lets the core broker transport remain stable as new capabilities are
added.

Each capability version defines:

- allowed operations and payload schemas;
- scope fields, defaults, canonicalization, and maximum breadth;
- whether each operation reads sensitive data, writes externally, or controls
  the desktop;
- whether recent trusted user activation is mandatory;
- per-call and aggregate budgets;
- the subset relation used for safe updates;
- audit redaction rules and effective-trust contribution.

The manifest's generic TOML scope is parsed immediately into a typed normalized
scope. Unknown capability IDs remain syntactically valid for forward
compatibility, but an unknown required capability makes the package
incompatible and an unknown optional capability is reported unavailable.
Unknown fields inside a known scope are errors.

Duplicate requests for the same capability version are rejected. This avoids
ambiguous required flags, reasons, or scope unions.

Example:

```toml
[[permission]]
capability = "dbus.call.v1"
required = false
reason = "Show playback state and control the selected media player"

[permission.scope]

[[permission.scope.rules]]
bus = "session"
destination = "org.mpris.MediaPlayer2.*"
path = "/org/mpris/MediaPlayer2"
interface = "org.freedesktop.DBus.Properties"
member = "Get"
signature = "ss"
allow_service_activation = false
requires_user_activation = false

[[permission.scope.rules.arguments]]
index = 0
equals_string = "org.mpris.MediaPlayer2.Player"

[[permission.scope.rules.arguments]]
index = 1
one_of_strings = ["PlaybackStatus", "Metadata", "Position"]

[[permission.scope.rules]]
bus = "session"
destination = "org.mpris.MediaPlayer2.*"
path = "/org/mpris/MediaPlayer2"
interface = "org.mpris.MediaPlayer2.Player"
member = "PlayPause"
signature = ""
allow_service_activation = false
requires_user_activation = true
```

The author-supplied reason is displayed as such. Host-generated text separately
describes the real normalized scope and risks.

## Initial capability catalog

| Capability | Principal scope | Important policy |
| --- | --- | --- |
| `context.read.v1` | Exact fact families or keys | Facts have public, activity, sensitive, or secret classification; values are coalesced and size bounded. |
| `filesystem.read.v1` | User-approved logical path or typed standard-directory roots and file-kind constraints | Relative paths only; no special files, devices, sockets, mount crossing, or symlink traversal by default. |
| `filesystem.write.v1` | User-approved logical path or typed standard-directory roots, operation set, and quota | Separate from read; atomic replace is distinct from create/append/delete; topology changes default off. |
| `http.request.v1` | Exact HTTPS origins, methods, optional path prefixes, and private-network flag | Reauthorize every redirect and resolved address; no ambient cookies, proxy, netrc, client certificate, or credentials. |
| `dbus.call.v1` | Bus, well-known destination, object path, interface, member, and signature | Session bus is not itself a security boundary; deny bus administration, monitoring, arbitrary destinations, and activation by default. |
| `dbus.subscribe.v1` | Exact senders/interfaces/members and bounded match rules | No eavesdropping; bounded queue with overflow markers and explicit unsubscribe. |
| `command.run.v1` | Host/user-approved executable and typed argument templates | Never invokes a shell or searches plugin-controlled `PATH`; empty environment by default; strict output, process, and time limits. |
| `clipboard.read.v1` | MIME types, maximum bytes, and operation rate | Requires a fresh physical touch activation; never audited by content or offered-format list. |
| `clipboard.write.v1` | MIME types, maximum bytes, and operation rate | Separate grant and a fresh physical touch activation; regular selection only in v1. |
| `secret.read.v1` | User-mapped logical secret names | No ambient keyring enumeration; values are never logged and normally require activation. |
| `notification.send.v1` | Categories, actions, urgency, and rate | Host labels the originating plugin; no spoofed system identity. |
| `uri.open.v1` | Schemes and optional origin set | Requires activation; forbids dangerous or unsupported schemes. |
| `local.connect.v1` | Exact user-approved Unix endpoints and protocol label | No arbitrary socket namespace or descriptor passing; classified as broad unless a protocol adapter narrows operations. |
| `appearance.provide.v1` | Exact package-local provider IDs, installer-bound roots, file size, and update rate | Available only to the separate provider worker; authorizes bounded reads and typed semantic publication, while the compositor owns selection, motion policy, derived roles, and atomic generations. |

Clipboard, secret, context, URI, notification, and input operations should use
desktop portals when a suitable portal actually supplies the needed authority
and UX. Portal availability does not remove the plugin-level permission or
audit requirement.

### Version 1 normalized scope grammar

The following conceptual types are normative for scope parsing. The manifest
uses equivalent TOML values; tooling generates those values so authors rarely
write nested rules by hand. Strings have capability-specific length limits and
all collections have host-defined maximum counts.

```text
ContextReadScope {
  facts: set<fact-key-or-family>,
  maximum_updates_per_second: u16,
}

FilesystemMountRequest {
  label: kebab-id,
  suggested_location: option<xdg-directory-or-display-hint>,
}

AppearanceProvideScope {
  providers: set<package-local-provider-id>,
  mounts: set<FilesystemMountRequest>,
  maximum_file_bytes: u64,
  maximum_updates_per_second: u16,
}

FilesystemReadScope {
  mounts: set<FilesystemMountRequest>,
  kinds: subset<regular-file, directory>,
  maximum_file_bytes: u64,
  enumerate: bool,
}

FilesystemWriteScope {
  mounts: set<FilesystemMountRequest>,
  operations: subset<create, replace, append, delete, rename, create-directory>,
  maximum_file_bytes: u64,
  maximum_total_bytes_per_hour: u64,
}

HttpOrigin {
  scheme: https-or-explicitly-approved-http,
  host: canonical-ascii-host,
  port: u16,
}

HttpRequestScope {
  origins: set<HttpOrigin>,
  methods: subset<GET, HEAD, POST, PUT, PATCH, DELETE>,
  path_prefixes_by_origin: map<HttpOrigin, set<absolute-path-prefix>>,
  private_network: bool,
  maximum_request_bytes: u64,
  maximum_response_bytes: u64,
  maximum_requests_per_minute: u16,
}

DbusArgumentConstraint {
  index: u16,
  equals_string: option<string>,
  one_of_strings: option<set<string>>,
}

DbusCallRule {
  bus: session-or-system,
  destination: exact-well-known-name-or-terminal-prefix,
  path: exact-object-path,
  interface: exact-interface,
  member: exact-member,
  signature: exact-signature,
  arguments: list<DbusArgumentConstraint>,
  allow_service_activation: bool,
  requires_user_activation: bool,
}

DbusCallScope { rules: set<DbusCallRule> }

DbusSignalRule {
  bus: session-or-system,
  sender: exact-well-known-name-or-terminal-prefix,
  path: exact-object-path-or-namespace,
  interface: exact-interface,
  member: exact-member,
  signature: exact-signature,
  argument_zero: option<exact-string>,
}

DbusSubscribeScope {
  rules: set<DbusSignalRule>,
  maximum_events_per_second: u16,
}

CommandParameter {
  name: kebab-id,
  kind: bounded-integer | fixed-enum | bounded-text | approved-file-handle | url,
  constraint: kind-specific-constraint,
}

CommandArgument = Literal(string) | Parameter(CommandParameter)

CommandRule {
  id: kebab-id,
  executable: host-path,
  arguments: list<CommandArgument>,
  environment: map<literal-name, literal-value>,
  working_directory: none | host-approved-absolute-path,
  maximum_output_bytes: u64,
  timeout_milliseconds: u32,
}

CommandRunScope {
  commands: set<CommandRule>,
  maximum_parallel_processes: u8,
}

ClipboardScope {
  mime_types: set<mime>,
  maximum_bytes: u64,
  maximum_operations_per_minute: u16,
}
SecretReadScope { logical_names: set<kebab-id>, requires_user_activation: true }
NotificationScope { categories: set<kebab-id>, urgency: set<low, normal, critical>, actions: bool, maximum_per_minute: u16 }
UriOpenScope { schemes: set<scheme>, origins: set<HttpOrigin> }
LocalEndpointRequest {
  label: kebab-id,
  protocol: registered-protocol-id,
  suggested_endpoint: option<display-hint>,
}

LocalConnectScope {
  endpoints: set<LocalEndpointRequest>,
  maximum_frame_bytes: u32,
  maximum_bytes_per_minute: u64,
}
```

Directory and local-endpoint hints never confer access by themselves. During
installation the host may resolve only its fixed location vocabulary
(`xdg-config:`, `xdg-data:`, `xdg-state:`, `xdg-cache:`, `xdg-runtime:`, and
`home:` plus the standard user-directory names) and must display the exact
result before consent. Arbitrary absolute paths and parent traversal remain
descriptive and cannot become defaults. Consent maps each logical label to a
host resource, and that mapping lives only in the grant store. A package cannot
name `/home/user` and cause it to become authoritative.

The v1 grant record has one typed `bindings` object. Filesystem roots, Secret
Service item object paths, pathname Unix-stream endpoints, and the exact
desktop Wayland clipboard socket occupy separate typed fields. An allowed grant
must contain exactly the authority required by its approved scope
and no bindings from another capability; a denied grant must contain none.
Effective policy copies this object as a unit, and any binding change revokes
live operations and resources. This is the only host-target authority. A
manifest suggestion becomes useful only when the installer resolves an
allowlisted symbolic location and the user confirms the displayed result; the
resulting grant binding, never the manifest string, is authoritative.

D-Bus call authority is a set of complete rules, not independent lists of
destinations, paths, interfaces, and members. Independent lists would create a
Cartesian product and accidentally authorize combinations the user never saw.
Terminal-prefix matching exists only for well-known namespaces that inherently
have dynamic instances, such as MPRIS names, and is displayed explicitly.
Argument constraints narrow generic methods such as
`org.freedesktop.DBus.Properties.Get`; unsupported value types or unconstrained
security-sensitive dispatcher methods are rejected.

Host policy may lower every numeric maximum. A package update that raises a
maximum beyond the approved value is a scope expansion. Boolean fields use the
safer value as the subset: `false` is narrower than `true` for authority such
as private-network access, enumeration, actions, or activation. A requirement
for trusted user activation may be strengthened without consent but never
weakened without consent.

## Scope normalization and update comparison

Raw manifest scopes are not compared as TOML. Each capability implementation
parses them into a canonical form:

- names, methods, schemes, and enum values use canonical case rules;
- DNS names use a canonical ASCII form and explicit ports;
- paths are associated with user-approved, identity-bound directory roots, not trusted
  because an author wrote `$HOME` or a string prefix;
- arrays are deduplicated and sorted where order has no semantic meaning;
- implicit defaults become explicit;
- wildcards are either unsupported or represented as an explicit broad-scope
  value with a higher risk class;
- numeric sizes, concurrency, and timeouts are clamped to host maxima.

Every capability supplies `is_subset(new, approved)`. An update can reuse a
grant only if its capability version is unchanged and its normalized requested
scope is a subset of the approved scope. Narrowing is automatic. Expansion is
inactive until approved. A required/optional change does not itself expand
authority, but changing optional to required can disable the plugin and is
shown in the update summary.

## Grant identity, provenance, and persistence

Persistent decisions are anchored to:

- canonical `github:owner/repository` identity;
- capability identifier and version;
- normalized approved scope;
- provenance policy under which reuse is allowed;
- the version and artifact digest at which consent was recorded.

The digest is evidence, not the long-term identity. A verified update from the
same repository may reuse a subset grant. A repository transfer, source change,
unverified artifact, mutable development ref, or failed provenance check cannot
silently inherit sensitive or controlling grants.

Local development grants are always bound to the exact content digest. The CLI
requires the user to choose `--session` or `--persistent`; it never turns an
omitted lifetime into durable authority. Persisting development authority emits
an explicit warning.

Persistent decisions live in
`$XDG_DATA_HOME/touchbar/permissions.toml`. Session decisions live in
`$XDG_RUNTIME_DIR/touchbar/session-permissions.toml`; the compositor
atomically replaces that file with an empty private store before starting any
plugin supervisor. Both stores are mode `0600`, reject symlinks, links, wrong
ownership, and broad parent permissions, and use a cross-process lock plus an
atomic write and directory synchronization. Secrets themselves never live in
either grant store.

Decisions support:

- `deny`;
- `allow-session`, expiring when the supervised plugin session ends;
- `allow-persistent`, subject to identity, provenance, and subset checks.

“Allow once” may later be added for one-shot operations. It is not represented
as an unbounded session grant.

## Consent and control UX

### Installation

Before executing package code, the high-level installer shows:

- source identity, version, and provenance signal;
- an explicit warning for an unrestricted native runtime;
- a host-written human label and stable identifier for each capability;
- the author reason and exact resolved host resources;
- optional permissions skipped because no safe default is available;
- unsupported requirements and the resulting activation state.

One confirmation accepts the complete displayed package plan and enables the
plugin; it is not a generic undisclosed “allow everything” shortcut. Advanced
binding flags override resolved defaults. `--yes` accepts that same printed
plan for automation; noninteractive installation without it fails before the
package is installed. The low-level add, permission, and enable commands remain
available when a controller needs separate machine-readable decisions.

### Updates

The update view distinguishes unchanged, narrowed, removed, expanded, and new
permissions. Removed grants become inactive. New or expanded authority is not
usable until approved; the previous content-addressed installation remains
available for rollback.

### Runtime

Version 1 has no arbitrary plugin-triggered permission prompt. Plugins receive
capability status and can render a disabled state or an action that opens the
host-owned settings view. This prevents prompt spam, misleading just-in-time
copy, and input capture ambiguity.

Revoking a capability cancels outstanding operations, closes its resources and
subscriptions, records a redacted event, and restarts the component host in the
first implementation. Restarting provides a simple guarantee that no stale
handles survive. If a revoked capability is required, the pack becomes blocked
with a host-owned placeholder. The daemon independently evaluates effective
policy for diagnostics, reports the process as `awaiting-consent`, keeps the
profile unready, and overlays a theme-aware noninteractive explanation on the
trusted fallback. If optional, the restarted guest receives an unavailable
status and must degrade.

Session decisions take precedence over persistent decisions. Supervisors watch
both containing directories, so an atomic update immediately recalculates
effective policy. A session denial can temporarily suppress a persistent allow;
resetting it falls back to the persistent decision. Restarting
`touchbar-sessiond` clears the entire session layer before plugin execution.
The CLI refuses to create session authority unless it can authenticate a live
same-user session daemon over its private control socket.

The implemented noninteractive control surface is:

```text
touchbarctl plugin permissions SOURCE [--format text|json]
touchbarctl plugin permission SOURCE CAPABILITY allow (--session|--persistent)
  [--reuse digest|source] [--bind LABEL=DIR] [--endpoint LABEL=SOCKET]
  [--secret NAME=OBJECT_PATH] [--clipboard-socket SOCKET]
  [--format text|json]
touchbarctl plugin permission SOURCE CAPABILITY deny (--session|--persistent)
  [--format text|json]
touchbarctl plugin permission SOURCE CAPABILITY reset (--session|--persistent)
  [--format text|json]
touchbarctl plugin inspect SOURCE --format text|json
```

`permissions` reports the manifest request, author reason, normalized scope,
host-derived risk, current status, and whether the winning decision came from
the session layer. `allow` always copies the exact installed normalized request;
there is no user-supplied scope document that can be confused with the package
or vice versa. Resource bindings are resolved from explicit canonical
user-owned filesystem objects. Manifest location hints never become authority.
Source-wide grant reuse is accepted only for immutable verified releases;
session and local-development grants remain exact-digest decisions.

## Trusted activation

Some authority should be usable only as the direct consequence of a user
gesture. The component cannot prove this by setting a boolean.

The input protocol must distinguish physical, trusted-control, and synthetic
origins and carry a compositor-generated sequence. When
`touchbar-plugin-host` is executing an activation generated from a real captured
touch, it holds a short-lived, single-use activation context containing the
surface instance, item, widget, input sequence, and monotonic deadline. If the
guest requests an activation-gated operation during that callback, trusted host
code attaches that context over the supervisor-associated broker channel. The
broker validates and consumes it. Activation context is not exposed to guest
memory and cannot authorize a different plugin or operation after expiry.

The native host is part of the component sandbox's trusted computing base. A
Wasmtime escape that compromises it may forge activation, but still cannot
exceed the connection's effective capability scope; keeping the broker outside
the host preserves that second boundary.

Synthetic test and onscreen-preview input is marked synthetic by the v1 surface
protocol and cannot authorize sensitive operations unless the deterministic
test supervisor explicitly enables a fake policy.
Background ticks, renders, theme changes, network responses, and subscription
events do not carry activation.

## Component broker ABI

The broker interface is a small transport, not a collection of untyped ambient
OS APIs. Conceptually it provides:

```text
capabilities() -> list<capability-status>
request(capability, operation, canonical-payload) -> result<request-id, error>
cancel(request-id) -> result<(), error>
close(resource-id) -> result<(), error>
```

Requests return promptly after local validation and bounded queue admission.
Completion is delivered later through a host-event export containing request
ID, operation result metadata, and either a bounded inline payload or a
broker-owned resource ID. Large bodies are read in bounded chunks. Resources
are connection-scoped, quota-counted, revocable, and closed on host exit.

The transport payload uses one documented canonical binary encoding with a
strict schema per capability operation. SDKs expose typed Rust and later C,
Go, or other language bindings; ordinary plugin authors do not manually encode
payloads. The generic envelope means adding `calendar.read.v1` or another
adapter does not require changing the core plugin world, while its operation
schema remains typed and testable.

Capability imports are phase restricted. `items()` and `render()` are pure:
broker requests made during them fail with `invalid-phase`. Requests are
allowed from input and host-event callbacks and, when introduced, explicit
lifecycle/timer callbacks. This prevents repeated layout or theme renders from
duplicating side effects.

The host-event queue is bounded. Replaceable state events coalesce to the
newest revision. Edge events preserve order until the limit, then emit an
overflow marker rather than silently pretending continuity. A guest that does
not drain events is restarted or disabled according to health policy.

The unpublished project has one concrete `touchbar:plugin/plugin@1.0.0` world.
Incompatible prototype changes update that world directly; there are no world
adapters or fallback imports. Every component exports `handle-host-event`. The native
host waits on the broker socket beside Wayland and delivers bounded callback
batches on the component's single UI thread. Idle components use no timer
polling.

## Common error model

Every broker uses stable, non-sensitive error categories:

- `unavailable`: backend or optional capability is absent;
- `denied`: no effective grant;
- `out-of-scope`: the operation exceeds the normalized grant;
- `invalid-request`: schema, encoding, or operation is invalid;
- `invalid-phase`: the current guest callback cannot initiate the operation;
- `activation-required`: no valid trusted activation is attached;
- `quota-exceeded`: bytes, resources, concurrency, or rate budget is exhausted;
- `rate-limited`;
- `timeout`;
- `cancelled`;
- `unsupported`;
- `backend-failed`;
- `internal`.

Errors do not reveal whether an out-of-scope file, secret, D-Bus owner, socket,
or private address exists. Human diagnostics live in host-owned logs; guest
messages remain bounded and sanitized.

## Broker-specific enforcement

### Filesystem

Version 1 uses broker-owned directory descriptors rooted in installer-owned
bindings rather than raw host path preopens. A binding is either a normalized
logical path selected by the user or a typed standard directory (`home`, XDG
config/data/state/cache/runtime) plus a relative suffix. Standard directories
are resolved from trusted host session state, never guest environment. The
guest sees only an opaque label and relative paths. Legitimate replacement of
a directory at the approved logical location preserves the grant; replacing it
with a symlink does not.

Linux resolution uses a directory file descriptor and `openat2` containment,
including `RESOLVE_BENEATH`, `RESOLVE_NO_MAGICLINKS`, and by default
`RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_XDEV`. The broker rejects absolute paths,
NULs, `..` escapes, device nodes, sockets, procfs-like magic links, unexpected
file types, oversized files, and excessive directory enumeration. Create,
replace, append, delete, rename, and create-directory are separate scope bits;
v1 has no generic truncate or metadata mutation. Creates are staged and
committed with no-overwrite rename, renames never overwrite, deletes move a
pinned single-link regular file to an unpredictable broker-private quarantine
name before checking and unlinking it, and replace plus append exchange a
synced broker-created temporary file with an existing pinned regular file in
the same directory. Mutations from one plugin are serialized, broker temporary
names occupy a namespace that requests cannot address, and every topology
change is followed by a directory sync. Inline writes are bounded by both the
operation envelope and per-file/per-hour grants. The hourly allowance is a
source-bound, cross-process-locked rolling 60-minute ledger in the supervisor's
required private state directory; restarts do not replenish it and malformed
state fails closed. Large create, replace, and append operations use
broker-owned staging resources: exact-offset chunks are acknowledged one at a
time, close/revocation aborts and removes the stage, and only an explicit
complete-size commit can change the destination.

If a future mode supplies WASI directory preopens directly, revocation requires
host restart, its reduced per-operation auditability is disclosed, and it must
not become the default merely for convenience.

### HTTP

The initial broker accepts HTTPS only unless the user explicitly approves
another scheme. Scope is based on normalized origin: scheme, canonical host,
and explicit port. Optional path prefixes and method sets further narrow it.

The broker resolves and validates every connection destination, including each
redirect. Loopback, link-local, multicast, unspecified, metadata-service, and
private addresses are denied unless a distinct private-network scope permits
them. Redirects cannot escape approved origins. Authorization, cookies, and
other credential-bearing headers are stripped when policy requires and are
never synthesized from ambient user state. DNS, connect, TLS, header, body,
response, decompression, redirect, concurrency, bandwidth, and total-duration
limits are enforced host-side.

### D-Bus

The component never receives the session or system bus socket. The broker owns
the connection and constructs messages after validating the bus, well-known
destination, object path, interface, member, signature, argument sizes, and
operation direction. Raw serialized messages are not accepted.

The `org.freedesktop.DBus` administrative and monitoring APIs, unique-name
wildcards, eavesdropping, arbitrary match rules, file-descriptor passing, and
service activation are denied unless a future capability explicitly models
them. A non-activating method call resolves the approved well-known name and
targets that unique owner, so an absent service is not started and a handoff
cannot redirect the call. Calls have a two-second transport deadline and raw
replies are size/signature/descriptor checked before typed decoding.
Subscriptions are exact, resolve one unique owner, recheck ownership and sender
on every event, and terminate on churn or revocation. Transport and resource
queues, body bytes, and event rate are bounded.

### Commands

There is no `shell`, `eval`, or arbitrary executable operation. A grant names
an executable approved by the host/user plus argument templates whose slots
have types such as bounded integer, fixed enum, opaque file handle, URL, or
length-limited text. Arguments are passed directly as `argv`; no shell quoting
is involved. The broker does not search a guest-controlled `PATH`, load a
guest-controlled working directory, inherit stdin, or inherit environment
variables unless each value is explicitly part of the approved template.

Executable provenance matters. Package-supplied executables turn the package
into native code and are treated as the native/effectively-trusted runtime, not
as a clever component capability. The production broker rejects executables
resolved inside the package, pins the opened executable descriptor before
launch, and requires a regular executable owned by root or the current user
which is not group/world writable and has one link. Ordinary symlinks are
resolved once and pinned; kernel magic links are rejected. User scripts and
commands with broad interpreters receive a strong effective-trust warning.
Child count, address space, open descriptors, CPU/wall time, output bytes, and
termination are bounded. Every command joins a new mandatory cgroup-v2 leaf in
the pre-exec phase, receives a pidfd, and cannot reopen the delegated cgroup
hierarchy after a command-specific Landlock layer is installed. Cancellation
first signals the pidfd/process group and then uses recursive `cgroup.kill`, so
descendants remain owned even after `setsid`. A narrow seccomp layer blocks
mount/namespace, handle-based filesystem, tracing, cross-process memory,
kernel-keyring, BPF/perf, module, and host-administration escape syscalls while
still permitting the exact executable to spawn ordinary helpers. These are
lifecycle and defense-in-depth constraints, not a claim that an approved
interpreter or dangerous literal template is safe.

### Clipboard, secrets, notifications, URI opening, and input

These brokers accept structured values, not arbitrary desktop-protocol
messages. Clipboard and secret reads default to trusted activation. Secret
grants map plugin-local logical names to user-selected entries and never permit
enumeration. Notifications always display a host-controlled plugin identity.
URI opening validates schemes and uses the desktop handler without returning
handler secrets. Synthesized input is a high-risk, activation-gated set of
named actions rather than arbitrary device access.

The implemented notification adapter accepts bounded plain text only. It
places the immutable GitHub source in the portal title, moves package-authored
title/body text into the body, rejects control and bidirectional-override
abuse, matches category and urgency exactly, namespaces IDs by source, hides
content on the lock screen, and keeps a durable rolling rate ledger. Markup,
arbitrary icons/sounds, persistent hints, and actions are not yet representable.
The implemented URI adapter consumes one fresh physical Touch Bar activation
and supports only exact-granted HTTP(S) origins/paths or explicit `mailto`; it
rejects credentials, encoded traversal, local files, executable/custom
handlers, and dangerous schemes before calling the XDG desktop portal. It
activates and pins the portal's unique D-Bus owner on a dedicated connection,
uses an unpredictable request token, subscribes before `OpenURI`, and accepts
only the exact expected handle and a bounded, descriptor-free
`Request.Response`. Portal status 0 succeeds, status 1 reports user
cancellation, and every other status fails closed. Local cancellation and
deadlines send `Request.Close`, close the dedicated connection, and join the
bounded worker.

The implemented local-connect adapter speaks one deliberately small broker
protocol over an installer-bound pathname Unix stream: a four-byte
network-order length followed by a nonempty payload. It pins the socket inode
with `openat2` before connecting through that descriptor, rejects symlink
traversal, hardlinks, non-sockets, and sockets not owned by the supervisor user,
then requires the connected peer's `SO_PEERCRED` UID to match. It never uses
`sendmsg` or `recvmsg`, so file-descriptor passing cannot cross the sandbox. The
wire frame cap is 12 KiB even when a broader manifest maximum was approved; the
opened-resource reply reports the effective cap. Every send rechecks the
instance, endpoint, protocol, grant binding, frame cap, and byte rate. Inbound
and outbound bytes share a source-bound, restart-resistant rolling 60-second
ledger. Protocol-specific adapters should still be preferred when a service
exposes dangerous generic operations.

The freedesktop Clipboard portal is not a drop-in implementation for
`clipboard.read.v1` or `clipboard.write.v1`: it only extends an already active
Remote Desktop or Input Capture session. Creating such a session solely for
clipboard access would grant the wrong authority. Likewise, the freedesktop
Secret portal retrieves one per-application master secret for encrypted local
storage, rather than user-selected named entries.

The clipboard adapter therefore uses `ext-data-control-v1` directly against one
installer/user-bound desktop compositor socket. It connects through a pinned,
symlink-free, same-user Unix-socket descriptor and never consults ambient
`WAYLAND_DISPLAY`. A guest can request one exact MIME type already present in
its grant; it cannot enumerate offered types, access primary selection, or pass
a socket name. Reads and writes both consume fresh physical Touch Bar
activation and share a source-bound durable rolling-minute operation limit.
Inline values are capped at 48 KiB even if the declared scope is broader.
Wayland discovery, selection setup, pipe reads/writes, and shutdown are bounded;
write content is retained only by the broker-owned selection source and its
transport copies are zeroed on drop. Payload content never enters errors or
audit records.

The implemented secret adapter instead uses the freedesktop Secret Service
API. Every read consumes a fresh physical Touch Bar activation and maps one
validated logical name through the private grant to one exact item object path.
It never calls SearchItems, lists collections, unlocks an item, or invokes a
prompt. An already unlocked item is read through a short-lived `plain` session;
the returned session path must match and its algorithm parameters must be
empty. Values are inline-only and capped at 48 KiB, the transport-owned copy is
zeroed on drop, and the session is closed on success or failure. The supervisor
disables dumpability and core files before it handles any broker payload.
Untrusted Secret Service frames are isolated in a sibling
`touchbar-secret-helper`: the supervisor exchanges only one bounded private
binary request/response, while the helper has a 128 MiB address-space ceiling,
two-second method limits, one-message queues, and raw reply validation before
typed decoding. It activates and pins the service's unique bus owner. Landlock
allows only the exact same-user session-bus socket and no ambient filesystem or
network access; seccomp allows AF_UNIX plus runtime threads while denying
process creation and kernel-administration surfaces. Cancellation kills and
reaps the helper, and disconnecting it releases the short-lived session.

The implemented context adapter currently exposes only public Hyprland facts:
`application.id` and `workspace.id`. A plugin requests an exact subset of its
grant and can take a bounded snapshot or open a bounded subscription. The
initial snapshot and subscription registration are one atomic transaction, so
an update cannot disappear between them. Values are size/control-character
validated, unchanged values do not advance the generation, and event rate is
the lower manifest/grant limit. Production connects only to a same-user,
symlink-free pinned Hyprland command/event socket, limits JSON replies to 64
KiB and event lines to 4 KiB, and extracts the application class or workspace
identifier. Window titles are deliberately ignored. Other requested fact keys
return `unsupported` as a unit until a dedicated trusted provider is wired.

## Resource and availability policy

Fuel bounds guest instructions, but it does not stop a host call blocked in
native code. Every backend operation therefore has its own deadline and runs
off the GLES/Wayland callback thread. Epoch interruption complements fuel for
wall-clock guest limits, while store memory, tables, instances, guest-to-host
copy sizes, host resource handles, random bytes, event queues, broker requests,
and response bodies all have explicit maxima.

Default starting budgets per host connection are deliberately modest and
centrally configurable:

- 32 pending broker operations;
- 16 live broker resources;
- 64 KiB request envelope and inline response;
- 4 MiB total buffered response data;
- 8 concurrent HTTP connections, subject to lower per-origin limits;
- 1 command process unless a grant explicitly permits more;
- 256 queued edge events, with state-event coalescing;
- capability-specific byte, rate, and duration limits.

Exhaustion returns an error to that plugin. It must not allocate until the
whole host aborts. Repeated traps, timeouts, protocol violations, or quota
events feed a circuit breaker with bounded restart backoff and a visible health
state.

## Audit contract

The structured audit record contains:

- monotonic sequence and wall-clock timestamp;
- canonical source, installed version, artifact digest, and host instance ID;
- item and widget when an operation originated from input;
- capability, operation, normalized scope identifier, and activation class;
- decision path: allowed, denied, out of scope, unavailable, or policy blocked;
- result category, duration, and byte/resource counts;
- backend category and redacted diagnostic code.

It never contains secret values, clipboard contents, paths beyond the
user-facing approved-root label plus a redacted relative-path hash, HTTP query
or body data, authorization headers, file contents, command output, D-Bus
payloads, or window titles. Logs are user-owned, mode `0600`, rotated by size
and age, and removable through the control interface. Live diagnostic output
uses the same redaction function as persistent audit storage.

## Effective-access labels

The installer and inspector compute, rather than accept from the package, one
of these summaries:

- **Isolated:** baseline UI/runtime only.
- **Limited:** narrowly scoped, non-sensitive local reads.
- **Connected:** network egress or narrowly scoped session integration.
- **Sensitive:** clipboard, secrets, private content, or equivalent reads.
- **Controlling:** external writes, process invocation, or consequential
  desktop/system actions.
- **Effectively trusted:** broad home/session/local-IPC authority, interpreters,
  arbitrary command shapes, package native code, or equivalent combinations.
- **Unrestricted native process:** no component sandbox enforcement.

The summary is the maximum individual risk plus combination rules. In
particular, egress combined with sensitive reads highlights exfiltration risk,
and user-controlled scripts can raise a narrow-looking command grant to
effectively trusted. Catalog status and publisher verification are displayed
separately; neither lowers effective access.

## Defense in depth

The component model is the primary guest-code boundary, and operation-level
broker authorization is the primary external-authority boundary. Around them:

- keep one component pack per host process;
- keep Wasmtime on supported patched releases and treat runtime advisories as
  security updates;
- set memory/object/copy limits, fuel, and epoch deadlines;
- close inherited descriptors except the private Wayland, GPU/runtime, and
  supervisor-created broker endpoints;
- clear arguments, environment, cwd, standard streams, network, filesystem,
  and session handles unless deliberately supplied;
- set `no_new_privs` and apply a tested syscall allowlist after initialization;
- apply Landlock to restrict ambient filesystem, network, signals, and Unix
  sockets where the running kernel supports the needed ABI;
- use namespaces only where their operational complexity produces a measured
  benefit; they are not the authority model;
- report which defense-in-depth layers are active instead of silently claiming
  unavailable kernel features.

The host needs already-open GPU and private Wayland descriptors to render.
Landlock and seccomp policy must preserve those known operations without
allowing new ambient connections. Broker backends remain outside the renderer
host so a renderer-host compromise does not automatically acquire their full
OS connections.

## Adversarial acceptance matrix

E3 is not complete until automated tests cover at least:

| Area | Required test |
| --- | --- |
| Identity | Guest-supplied source/digest cannot change the connection's supervisor-bound identity. |
| Default deny | A manually launched or grantless host cannot access any non-baseline broker. |
| Manifest | Unknown keys, duplicate capabilities, malformed versions, invalid scopes, and oversized reasons fail before execution. |
| Scope | Canonical equivalence and subset/expansion decisions are deterministic for every capability. |
| Update | Same-source narrowed verified update reuses a grant; expansion, source transfer, and unverified dev update do not. |
| Revocation | In-flight work is cancelled, resources close, host restarts, and no stale response crosses into the new instance. |
| Optional | Denial produces an availability event and the demo remains functional. |
| Required | Denial prevents activation and renders a host-owned explanation without executing the component. |
| Callback phase | Broker requests from `items` or `render` fail and never reach a backend. |
| Activation | Forged, expired, reused, synthetic, cross-widget, and cross-plugin activations fail. |
| Filesystem | Absolute paths, `..`, symlink swaps, magic links, mount escapes, device nodes, sockets, rename escapes, and quota abuse fail. |
| HTTP | Disallowed methods/origins, redirects, DNS rebinding, IPv4/IPv6 private targets, oversized/decompression responses, and credential forwarding fail. |
| D-Bus | Destination/path/interface/member/signature escapes, owner changes, autostart, monitoring, eavesdropping, and FD passing fail. |
| Commands | Shell metacharacters remain literal argv, template escapes fail, environment is empty, output is capped, timeout kills the process group, and package binaries are rejected. |
| Sensitive data | Clipboard, secret, file, HTTP, command, D-Bus, and title payloads never appear in logs or errors. |
| Resources | Guest loops, host-call floods, huge WIT strings/lists, event backpressure, abandoned resources, and backend hangs remain bounded. |
| Isolation | One plugin cannot observe, cancel, consume, or receive another plugin's requests, events, grants, or resources. |
| Recovery | Broker crash, host crash, corrupted grant store, unsupported kernel defenses, and unavailable desktop services fail visibly and safely. |
| Native | UI and machine-readable inspection state clearly report that native permission declarations are not enforced. |

Tests use fake filesystem trees, DNS, HTTP, D-Bus, command, clipboard, secret,
and clock backends. No conformance test depends on the user's live files,
credentials, network, or session services. A smaller physical test verifies
trusted-activation provenance without exposing sensitive content.

## E3 implementation slices

### E3.1 — Policy core

**Complete (2026-09-04).** `crates/touchbar-policy` now provides the shared,
broker-independent policy implementation used by future installer, supervisor,
control, and inspection surfaces.

- Typed capability registry, versioned scope parsers, normalization, subset
  checks, risk classification, and aggregated diagnostics.
- User-owned grant store and pure effective-grant calculation.
- Install/update permission diff and machine-readable inspection models.
- Policy adversarial tests cover malformed and duplicate scopes, host maxima,
  semantic narrowing, verified and development update reuse, required/optional
  launch behavior, native disclosure, invalid grants, store ownership/modes,
  atomic persistence, and symlink rejection. Backend identity and redaction
  tests enter with the private transport in E3.2.

### E3.2 — Private transport and lifecycle

**Complete (2026-09-04).** The first executable slice provides a versioned,
strictly decoded 64 KiB binary protocol over inherited `AF_UNIX`
`SOCK_SEQPACKET`, close-on-exec descriptor handling, same-user peer checks,
server-owned connection identity, monotonic request IDs, capability snapshots,
pure-callback rejection, default-deny routing, and required-revocation shutdown
semantics. The supervisor verifies installer-supplied source, version, and
artifact digest before `exec`; the protocol contains no field through which a
host can claim those values or alter its grants.

An unregistered capability remains unavailable even if a malformed or stale
grant mentions it; production authority exists only where the supervisor has
installed the corresponding typed backend.

The second lifecycle slice adds the shared machinery needed before a backend
can be registered: 32-operation and 16-resource accounting, total buffered-byte
limits, deterministic deadlines, capability-owned cleanup, bounded/coalescing
host events with explicit overflow, single-use activation validation, bounded
structured audit records whose types cannot contain payloads, and deterministic
circuit-breaker backoff. These models are exercised independently and policy
replacement now selectively revokes affected resources before emitting status.

The third slice adds a backend-neutral asynchronous runtime. A bounded worker
pool receives jobs carrying the supervisor-owned identity, never a guest claim;
queue and total-active admission fail before allocation grows without bound.
Cancellation, capability revocation, and monotonic deadlines override late
backend success, backend panics become `backend-failed`, and completions release
lifecycle state before entering the bounded host-event queue. Hostile fake
backends cover identity, pressure, cancellation, timeout, revocation, and panic.

The fourth slice connects that runtime to the executable socket loop. Linux
`eventfd` completion notification and the private sequenced-packet descriptor
share one blocking `poll` loop, so idle plugins consume no timer wakeups and a
host waiting for a response cannot deadlock behind another inbound request.
Only policy-granted requests enter the bounded executor; each job carries the
immutable connection identity and normalized authorized scope. Cancellation
commands, optional revocation, completion delivery, and structurally redacted
audit outcomes run end to end. The connection rejects configurations whose
edge-event queue could not hold every possible in-flight completion.

The fifth slice adds live permission lifecycle. An inotify watch is attached to
the grant store's parent directory so atomic replacement, deletion, creation,
and mode changes are observed without periodic polling. Unrelated directory
events are ignored; queue overflow forces a full reload, and an invalid,
over-broad, missing, or symlinked replacement is interpreted as an empty store
rather than preserving stale authority. Optional changes update the running
connection and emit capability generations. A required revocation sends
shutdown, cancels its host, and leaves the supervisor dormant; restoring a
valid grant launches a fresh channel and incremented instance. Without an
explicit watched grant path, the one-shot supervisor retains its original
fail-fast behavior.

The sixth slice adds durable audit storage behind an explicit supervisor
`--audit FILE` sink. Records use one JSON object per line and retain the same
payload-free type as the bounded in-memory log. A separate stable lock
serializes append and rotation across supervisor processes; the active file is
reopened under that lock so no process keeps writing to an archived inode.
Files and locks are user-owned mode `0600` beneath a user-owned private
directory. Symlinks, multiple hard links, broad modes, and wrong ownership are
rejected. Size and age rotation retain at most five archives by default, and
every record is synced before its completion is exposed to the host. Durable
sequence assignment is monotonic across host restart, rotation, and concurrent
writers. The explicit path avoids development and test launches silently
writing user state; the eventual touchbar-sessiond launcher owns the standard XDG state
location.

The seventh slice completes the WIT/SDK bridge. The trusted host owns the
broker endpoint inside its Wasmtime store, exposes typed capability snapshots,
asynchronous request IDs, cancellation, resource close, stable errors, and
ordered host events, and tags requests with the callback phase actually in
progress. `items` and `render` are rejected locally before a broker packet can
be sent. `touchbar-component-sdk` wraps the generated Rust bindings without
inventing capability-specific convenience APIs, and the broker component
example is a compile-checked minimal package.

The eighth slice completes trusted activation attachment. The v1 protocol assigns
one nonzero 64-bit sequence in `touchbar-sessiond` for every captured physical gesture
and carries it through every touch event. The component
host attaches the sequence, its private surface instance, item, widget, origin,
and a two-second absolute monotonic deadline only while running the resulting
`Activated` callback. Synthetic headless input, render, initialization, and
host-event callbacks cannot inherit it. The supervisor independently validates
phase, structure, origin, expiry, and maximum lifetime before forwarding the
metadata to a backend, and audit records retain only the origin category.
Guest-provided fields are never accepted. Operation-specific matching and
single-use consumption occur in the typed backend adapter after it decodes the
requested operation; the first such adapter is E3.3 MPRIS.

- Supervisor-launched host with inherited `SOCK_SEQPACKET` broker channel.
- Server-side immutable connection identity and bounded request/event/resource
  protocol.
- Capability-status snapshot, optional degradation, revocation, cancellation,
  restart, audit records, and circuit-breaker health.
- Phase restriction and trusted-activation plumbing.

### E3.3 — Useful vertical proof

- Exact-scope `dbus.call.v1` and `dbus.subscribe.v1` backends.
- A sandboxed MPRIS demo that reads properties and performs a Play/Pause action
  only after Touch Bar activation.
- Consent, denial, revocation, theme, fake service, headless, GPU, and physical
  acceptance paths.

This proves one ongoing read/subscription and one consequential action without
starting with a generic command escape hatch.

The call slice is implemented. `touchbar-broker-schema` defines the single
bounded binary call/reply representation shared by the component SDK and
supervisor. `DbusCallBackend` rejects malformed shapes, matches the normalized
bus, destination, path, interface, member, derived signature, and argument
constraints, then consumes a fresh physical activation for a rule that requires
one. Only after that preflight can `ZbusTransport` construct a message. Service
activation is disabled unless the matched rule opts in. The production
transport enforces that by resolving and calling the current unique owner, then
rejects oversized, descriptor-bearing, or wrongly typed raw replies before
decoding. A fixed method timeout and one-message receive queue bound a stalled
or flooding peer. Authorization is part of
the executor's queue-submission operation, so there is no unguarded enqueue API
for a caller to misuse. The reference component
contains both a `PlaybackStatus` read and `PlayPause`; a fake transport proves
denied operations perform no I/O.

The subscription slice is also implemented. Each `dbus.subscribe.v1` request
names one exact signal and receives an opaque broker-owned resource ID. The
production adapter resolves and pins the current unique service owner, then
installs a one-message zbus match only after normalized scope matching. Its
first typed body is `PropertiesChanged` (`sa{sv}as`): scalar property values are
canonicalized into the shared schema and unsupported container values are
omitted rather than exposing arbitrary bus payloads. Each incoming message is
checked for current owner, sender, headers, signature, size, and Unix FDs before
decoding. Events are sequenced per
resource and delivered through the existing callback. A 16 KiB event bound,
per-grant rate limit, bounded ingress queue, explicit overflow, and lifecycle
reservation prevent an emitter from creating unbounded work. Close, terminal
transport failure, permission revocation, and scope narrowing all tear down the
actual match. The production transport is now covered against a real private `dbus-daemon`
and fake MPRIS object, including property read, signal delivery, bounded queue
overflow, 32 repeated owner-loss cycles, and close. A separate lifecycle
campaign fills all 16 resources and proves overflow, revocation, monotonic reuse,
and supervisor-drop cleanup. The
isolated physical runner has also passed on Apple M1 hardware with real touch
activation, changing broker-driven scenes, zero invalid frames, and automatic
restoration of `tiny-dfr`. E3.3 is complete.

### E3.4 — Filesystem and HTTP

- Broker-owned filesystem handles with kernel-enforced resolution tests.
- Origin-scoped HTTP with redirect, address, credentials, decompression, and
  quota enforcement.
- User-selected roots, private-network consent, streaming resources, and
  network-plus-sensitive-read combination warnings.

The first filesystem-read slice is implemented. Logical manifest mount labels
are bound to canonical host directories only in validated grant records; the
binding is carried as supervisor-owned authority and changing it revokes
matching in-flight work. `list-directory` and `read-file` use one bounded typed
schema shared by the Rust SDK and production backend. Inline reads are chunked
to 48 KiB while the scope limits the complete file size. Large reads use a
broker-owned resource with metadata, ordered 12 KiB chunks, byte totals,
explicit termination, and bounded lossless backpressure. Directory results are
bounded and expose only UTF-8 regular files and directories.

On Linux, the backend resolves the approved logical or standard-directory root
without symlinks on every operation, then resolves each relative target using
`openat2` with
`RESOLVE_BENEATH`, `RESOLVE_NO_MAGICLINKS`, `RESOLVE_NO_SYMLINKS`, and
`RESOLVE_NO_XDEV`. Absolute paths, `.`/`..`, symlink escapes, mount crossings,
special files, oversized files, multi-link inodes, and ungranted labels fail
before content is returned. A stream opens and validates one descriptor before
its worker starts, so later path replacement cannot redirect that in-flight
operation. The reference filesystem component and physical runner exercise a real
list followed by a bounded stream. Filesystem write is not part of this slice.

The inline HTTP slice is implemented. Its typed request exposes only method,
URL, optional `Accept` and `Content-Type`, and a bounded body, so arbitrary or
credential-bearing headers are unrepresentable. The production transport uses
Rustls, disables system proxies, redirects, retries, referers, cookies,
automatic decompression, and connection reuse, and sends `Accept-Encoding:
identity`. The backend manually resolves and validates all addresses, pins the
validated set into a fresh client, and repeats origin, method, path, DNS, and
address authorization at every bounded redirect hop. Mixed public/private
answers fail as a unit. IPv4, IPv6, mapped, NAT64, and 6to4 private-address
forms are covered by adversarial tests, alongside userinfo, parser-differential
paths, header injection, response limits, compression, rate, and redirect
abuse. Large responses now open a broker-owned resource with typed metadata,
bounded 12 KiB chunks, explicit success/error termination, bounded lossless
backpressure, cumulative response accounting, and immediate close/revocation.
Redirects cannot multiply an approved request-body budget. The production
transport reads directly into chunks without response buffering or automatic
decompression. DNS uses a fixed two-worker/eight-job pool with a two-second
result deadline. With filesystem streaming implemented, E3.4's data plane is
complete.

### E3.5 — High-risk and defense-in-depth completion

- Constrained command templates, clipboard, secrets, notifications, URI open,
  and bounded local IPC; generic input synthesis is intentionally omitted.
- `no_new_privs`, seccomp, Landlock capability detection and policy.
- Full adversarial matrix, fuzzing boundaries, dependency advisory policy, and
  effective-access UX review.

**Complete (2026-09-04).** The defense-in-depth boundary is implemented for
supervised components.
Launch clears the inherited environment except fixed broker/artifact values,
the exact Wayland location, and a disabled Mesa shader cache; `close_range`
removes every descriptor above the three fixed slots. Before Wasmtime parses or
instantiates guest code, the host disables dumps and core files, enters a
Landlock domain, and loads an architecture-checked seccomp filter. Landlock
permits read-only runtime libraries/fonts/system GPU discovery, the GPU device
rights required by the renderer, and (for live mode) only the exact compositor
socket. It denies ambient writes and TCP. Seccomp additionally denies
non-Unix sockets, execution, tracing/cross-process memory, namespaces/mounts,
keyrings, modules, BPF/perf/userfaultfd, handle-based opens, and other
host-administration calls. ABI 5 is mandatory for headless mode and ABI 9 for
live exact-socket mediation; launch fails rather than silently degrading.
Headless mode cannot create sockets or tasks. Live mode allows only
thread-style clone flags needed by Mesa and only AF_UNIX sockets, with Landlock
confining connect to the one compositor socket. Each host is moved before exec
to a private cgroup leaf, receives parent-death kill, and has hard task,
address-space, descriptor, and available cgroup-controller limits. A normal
supervisor teardown recursively kills and reaps the leaf. If the supervisor
itself receives `SIGKILL`, parent-death handling kills the host and the next
launch removes only an empty, same-user leaf whose encoded owner PID is dead.

E3 is complete. It includes immutable-identity transport, supervised launch,
bounded lifecycle/security models, asynchronous execution, live socket-loop
orchestration, policy-driven restart, durable audit storage, the versioned
WIT/Rust SDK bridge, and v1 trusted physical activation attachment.
Production registers narrow D-Bus, filesystem, HTTP, constrained-command,
notification, URI-opening, secret-read, public-context, clipboard, and framed
local-IPC backends. Calls consume activation only after matching decoded
operations to their grants; live resources remain broker-owned until explicit
close, completion, or authority loss. Generic input synthesis has no v1 schema
or registry entry.

The acceptance suite covers deterministic permission-confusion mutations,
filesystem read/write root/parent/inode replacement, producer/thread cleanup,
write staging cleanup under full resource pressure, D-Bus signal overflow and
owner churn, backend panic and timeout, host crash, grant-store corruption and
restart, cross-plugin identity separation, and supervised headless plus real
Apple-M1 GPU/DMA-BUF paths. Three sanitizer-backed
libFuzzer targets exercise all typed broker schemas, both IPC directions, and
manifest/grant/effective-policy inputs. The latest bounded campaigns processed
1,399,314, 530,130, and 318,248 inputs with no crash, hang, or artifact.
`scripts/test-sandbox-security.sh` is the repeatable release gate. Dependency
advisory rules and the current clean-vulnerability scan are recorded in
`docs/security/dependency-policy.md`.

## Deferred decisions

- Runtime one-shot prompts and temporary user-selected resources.
- Whether stable WASI 0.3 async interfaces should replace part of the queued
  event adapter before the first release.
- Raw local-protocol adapters that require descriptor passing, shared memory,
  or high-rate streams, such as some PipeWire use cases.
- Sandboxing policies for native plugins beyond clear disclosure.
- System-wide administration and multi-user installations.

## Primary references

- [Wasmtime security model](https://docs.wasmtime.dev/security.html)
- [Wasmtime configuration and interruption limits](https://docs.wasmtime.dev/api/wasmtime/struct.Config.html)
- [Wasmtime WASI context defaults and preopens](https://docs.wasmtime.dev/api/wasmtime_wasi/struct.WasiCtxBuilder.html)
- [Wasmtime security advisories](https://github.com/bytecodealliance/wasmtime/security/advisories)
- [WebAssembly Component Model WIT reference](https://component-model.bytecodealliance.org/design/wit.html)
- [Linux `openat2(2)` resolution controls](https://man7.org/linux/man-pages/man2/openat2.2.html)
- [Linux Landlock userspace API](https://www.kernel.org/doc/html/latest/userspace-api/landlock.html)
- [Linux seccomp filter documentation](https://kernel.org/doc/html/latest/userspace-api/seccomp_filter.html)
- [Linux Unix-domain socket credentials and message semantics](https://man7.org/linux/man-pages/man7/unix.7.html)
- [D-Bus specification](https://dbus.freedesktop.org/doc/dbus-specification.html)
- [D-Bus API security design guidance](https://dbus.freedesktop.org/doc/dbus-api-design.html)
