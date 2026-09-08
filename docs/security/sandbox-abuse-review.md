# Sandbox abuse review

This is the live security ledger for the v1 component sandbox. It treats the
component as malicious, package metadata as attacker-controlled, and all
broker payloads as hostile. A capability is not complete merely because its
happy path works: its row closes only after the production boundary and the
relevant adversarial tests exist.

## Cross-cutting invariants

- Package requests describe desired authority; only validated user/installer
  grants create authority. Host resource mappings never come from a package.
- The supervisor binds source, version, digest, runtime, instance, and policy.
  None can be asserted in guest messages.
- Every operation is decoded into one bounded canonical type, rechecked
  against its effective scope, and authorized before entering a worker queue.
- Pure render and item-discovery callbacks cannot invoke brokers. Physical
  activation tokens are host-created, short-lived, single-use, and scoped to
  one instance, surface, widget, and input sequence.
- Broker errors and audits contain identifiers and sizes, never payloads,
  paths, URLs, response bodies, clipboard content, or secrets.
- Component WASI has no inherited arguments, environment, stdio, preopens, or
  sockets. Supervised host launch clears ambient environment and descriptors,
  passes sealed artifacts in fixed slots, and requires `no_new_privs`.
  Before the first guest instruction, the host requires Landlock ABI 5 (ABI 9
  for live rendering), disables dumpability/core dumps, and installs seccomp.
- Optional permission denial keeps the component usable. Required denial
  prevents host execution. Revocation cancels operations and closes resources.

## Permission-confusion attacks

Every scope family must reject duplicate, unknown, noncanonical, and oversized
fields before consent. Particular traps to retain in tests:

- Filesystem: label/hint confusion; missing, extra, relative, or parent-bearing
  grant mappings; absolute paths; `.`/`..`; NULs; root and nested symlinks;
  magic links; mount crossings; FIFOs/devices/sockets; type swaps; oversized
  files; enumeration floods; rename races; hard-link and quota abuse.
- HTTP: userinfo credentials; Unicode/trailing-dot/numeric host aliases;
  encoded dot, slash, backslash, or NUL paths; method changes on redirects;
  cross-origin redirects; mixed public/private DNS answers; rebinding on every
  hop; IPv4-mapped, NAT64, 6to4, loopback, link-local, multicast, unspecified,
  and reserved targets; ambient proxy/cookie/netrc/client-certificate state;
  automatic referers, retries, decompression bombs, huge headers/bodies, slow
  responses, redirect loops, and header injection.
- D-Bus: wildcard/unique-name confusion, owner changes, argument/signature
  mismatch, service activation, monitor/eavesdrop APIs, nested containers, file
  descriptors, match floods, and stale subscriptions after revocation.
- Commands: executable symlink/replacement races, package-owned binaries,
  interpreter and shell escalation, argv/template count or type confusion,
  metacharacters becoming syntax, attacker-controlled environment/PATH,
  working-directory escapes, inherited descriptors, process trees surviving
  timeout, output floods, and approved-file path races.
- Desktop capabilities: MIME/category/action/logical-name confusion; forged or
  replayed activation; owner spoofing; sensitive payloads in errors; clipboard
  and secret enumeration; notification action injection; URI parser
  differentials; synthetic input targeting another app or plugin.
- Local IPC: endpoint label/path confusion, symlinks, abstract sockets,
  descriptor passing, peer credential changes, protocol smuggling, frame
  fragmentation, and bandwidth/backpressure abuse.

## Current production status

| Boundary | Status | Evidence / remaining work |
| --- | --- | --- |
| Component runtime | V1 complete | Default-deny WASI, fuel/memory/object limits, strict WIT/UI validation, private broker transport, sealed verified artifact/manifest descriptors, cleared environment/extra FDs, `no_new_privs`, nondumpable memory, Landlock, seccomp, a private cgroup leaf, parent-death kill, task/address-space/descriptor limits, and recursive teardown are mandatory. A `SIGKILL` of the supervisor kills the host; the next launch removes only an empty same-user leaf with a dead encoded owner PID. Headless mode cannot create sockets or tasks. Live mode can create only Mesa-style threads and connect only to the exact Wayland socket. Procedural effects are capped straight-line WGSL bodies with a second Naga-IR allowlist: no guest functions, branches, loops, mutable locals, textures, storage, atomics, dynamic indexing, or raw driver source. Supervised headless and Apple M1 Wayland/EGL/DMA-BUF paths pass. |
| `context.read.v1` | Implemented public Hyprland slice | Exact requested keys are filtered against the grant. Snapshot and subscription schemas bound key count/value size/update rate; unchanged values are coalesced. Production supports only `application.id` and `workspace.id`, reads same-user symlink-free pinned Hyprland sockets, bounds command replies/event lines, lowercases application IDs, and never parses or forwards window titles. Other registered facts return unsupported until a dedicated provider exists. |
| `filesystem.read.v1` | Hardened inline + streaming slice | Every grant records the canonical selected root plus its device/inode identity. Every operation reopens that root with symlink-free `openat2`, verifies the identity, and resolves plugin-relative paths with `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS`, and `RESOLVE_NO_XDEV`. Absolute/parent paths, intermediate and final symlinks, root replacement, nested mounts, special files, multi-link files, oversized files, and enumeration floods fail closed. Inline and streaming reads use pinned descriptors with bounded backpressure and cancellation/revocation. The release gate repeatedly replaces final entries, intermediate parents, and grant-root paths after open, then saturates all resource/buffer slots and verifies overflow, revocation, repeated close, producer-thread exit, and runtime reuse. |
| `http.request.v1` | Implemented inline + streaming slice | Exact origin/method/path checks, per-hop pinned DNS, address classification, manual bounded redirects, cumulative upload/response limits, credential-free incremental reqwest transport, bounded backpressure, explicit stream termination, and close/revocation exist. DNS uses a fixed two-worker/eight-job pool and a two-second result deadline, so libc resolver hangs cannot create unbounded threads or occupy broker workers indefinitely. |
| `dbus.call.v1` | Hardened constrained slice | Every typed field is jointly matched; malformed names/paths/interfaces/members, bus confusion, unsupported argument shapes, and administration/monitoring destinations fail before transport. Non-activating calls resolve the approved well-known name to a unique owner and call that owner, preventing autostart and name-handoff redirection. Connections have a two-second method timeout and one-message receive queue. Raw replies reject bodies over 64 KiB, unexpected signatures, and Unix FDs before application decoding. Private-bus tests cover live calls, absent owners, hangs, scope mutations, and envelope abuse. |
| `dbus.subscribe.v1` | Hardened constrained slice | Exact typed subscriptions resolve and pin one current unique owner, install one-message signal and owner-change queues, recheck current ownership and sender before every event, and terminate on churn. Headers, signatures, argument zero, 64 KiB bodies, and absence of Unix FDs are verified before typed scalar decoding. The common resource layer bounds rate, buffered bytes, overflow, close, and revocation. The release gate repeats deterministic one-message overflow and well-known-name loss for 32 real private-bus subscriptions each, then saturates all 16 lifecycle slots and verifies atomic overflow, revocation, 64 close/reopen cycles, monotonic IDs, and runtime-drop cleanup. |
| `filesystem.write.v1` | Hardened inline + streaming slice | Exact create/replace/append/delete/rename/create-directory bits operate beneath device/inode-bound grant roots reopened with symlink-free `openat2`; root replacement and nested mount crossings fail. Parent directories and existing files are descriptor-pinned; create/replace/append stage and atomically commit complete content, rename never overwrites, delete quarantines and inode-checks before unlink, regular files must have one link, plugin mutations are serialized, and broker-private random names cannot be requested. Large writes use exact-offset broker resources with complete-size commit and abort/revocation cleanup. Payload/file caps and a durable source-bound rolling-hour ledger are enforced before I/O; quota corruption and restart attempts fail closed. Data and parent directories are synced. The release gate repeatedly replaces opened parent paths, grant-root paths, target inodes, and append snapshots, then saturates all resource slots and verifies overflow, cancellation, revocation, reuse, and teardown without escape or staging-file leakage. |
| `command.run.v1` | Hardened constrained slice | Exact typed templates become literal `argv`; executable and approved-file descriptors are pinned; environment/stdin are empty by default; output/events, address space, descriptors, CPU time, tasks, concurrency, and deadlines are bounded. Every launch requires a private cgroup-v2 leaf and pidfd before the resource opens. Landlock prevents the child from reopening the delegated cgroup hierarchy, seccomp blocks namespace/mount/handle/tracing escape syscalls, and `cgroup.kill` catches descendants after `setsid`. Command Deck provides a harmless `/usr/bin/printf` reference action with visible pending/running/success/failure state, exact headless replay, and opt-in physical-runner consent. The release gate repeats 12 touched 32 MiB allocations plus oversized address-space reservations and six 128-fork attempts; it proves the 33-task ceiling, memory/swap settings, cancellation, descendant death, worker reclamation, and cgroup removal after every cycle. |
| `clipboard.read.v1` / `clipboard.write.v1` | Implemented constrained Wayland slice | Each grant binds the exact desktop compositor socket; ambient `WAYLAND_DISPLAY` and manifest paths are ignored. Both operations consume a fresh physical touch activation, select one exact approved MIME type, cap inline content at 48 KiB, share a durable source-bound rolling-minute rate limit, and never enumerate formats to the guest. The broker uses `ext-data-control-v1`, regular selection only, a pinned symlink-free same-user socket, bounded polling/transfers, zeroing transport copies, and bounded long-lived write ownership. A private mock compositor proves real read/write and FD transfer behavior; cancellation also interrupts owner discovery and join promptly. |
| `secret.read.v1` | Implemented constrained and isolated Secret Service slice | A fresh physical activation can read one exact logical name mapped by private grant to one Secret Service item object path. The broker never searches or enumerates, never unlocks or prompts, uses a short-lived plain session, verifies the returned session/parameters, caps inline values at 48 KiB, zeroes transport copies, and closes the session. Untrusted D-Bus parsing occurs only in a dedicated 128 MiB helper with method/queue/body limits, unique-owner pinning, a bounded private pipe, exact-socket Landlock, and seccomp; cancellation kills and reaps it. Secret values never enter errors or audit records, and both processes are nondumpable with core files disabled. Private hostile-service tests cover oversized and malformed replies, kernel-enforced confinement, cancellation, and repeated reclamation. |
| `notification.send.v1` | Implemented constrained portal slice | Plain bounded text only; exact category/urgency; immutable source title; source-namespaced IDs; lock-screen content hiding; own-ID removal; and durable rolling restart-resistant rate limiting. Markup, icons, sounds, persistence hints, and actions are not representable yet. |
| `uri.open.v1` | Implemented constrained portal slice | Every request consumes fresh physical activation; exact scheme and optional origin/path matching precede the portal call. Only HTTP(S) and `mailto` are supported; credentials, encoded traversal, `file:`, `data:`, `javascript:`, and custom handlers fail closed. A dedicated connection pins the portal's unique bus owner; an unpredictable token, pre-call signal subscription, exact handle/header/signature checks, and bounded descriptor-free response decoding prevent request races and spoofing. Portal cancellation is reported, while local cancellation/timeouts issue `Request.Close`, close the connection, and join the worker. Private-portal tests cover fast and malformed responses, mismatched handles, all status codes, cancellation, timeout, and repeated teardown. |
| `local.connect.v1` | Implemented constrained Unix-stream slice | Manifests supply only a logical label/protocol. A private grant binds one normalized pathname. The broker pins a symlink-free socket inode, rejects non-sockets/hardlinks/wrong ownership, verifies same-user `SO_PEERCRED`, rechecks complete authority on every send, and exposes only nonempty length-prefixed frames. No abstract sockets, raw descriptors, descriptor passing, unframed bytes, or peer-selected targets. Frames, events, resources, and aggregate bidirectional rolling-minute bytes are bounded; the byte ledger survives restart. |
| `appearance.provide.v1` | Implemented declarative provider slice | A grant names exact package-local provider IDs, installer-bound source roots, and file-size/update-rate ceilings. Its bindings are purpose-specific and never enter the component's generic filesystem broker. The session daemon reopens and inode-checks the mount, resolves one normalized relative palette path with `openat2` beneath/no-symlink/no-mount-crossing constraints, accepts only a small complete semantic palette, and owns provider arbitration, derived roles, accessibility policy, and atomic generations. Invalid or incomplete replacements never displace the last valid snapshot. |

## V1 acceptance conclusion and residual risk

The component sandbox acceptance gate is complete on the Apple M1 target as of
2026-09-06. The ordinary workspace suite has 525 passing tests; the gate also
runs nine release-only filesystem, D-Bus, command, desktop-portal, and Secret
Service campaigns. The supervisor contributes 137 ordinary library tests,
eight library campaigns, 11 launch tests, and three ordinary plus one
release-only Secret Service process-integration tests covering
process pressure, backend panic, host crash, grant corruption/restart,
cross-plugin identity isolation, resource saturation/reuse, cgroup cleanup,
and denial before side effects. Supervised headless and real Apple-M1
GPU/DMA-BUF integrations both pass.

Three sanitizer-backed libFuzzer targets cover every typed broker decoder, both
wire directions, manifests, grant stores, and effective policy. The latest
bounded campaigns executed 1,399,314 schema inputs, 530,130 wire inputs, and
318,248 policy inputs with no crash, timeout, or artifact. Stable deterministic
mutation corpora remain in the ordinary test suite. Generated fuzz corpora are
ignored; any future finding must be reduced into a permanent regression test.

The remaining risks are explicit rather than unfinished authority paths:

1. GPU device IOCTLs, fixed system graphics/font data, and the exact live
   compositor socket remain deliberately reachable by the renderer host.
   Separating renderer and Wasmtime into two processes could reduce the impact
   of a renderer-host memory-safety bug in a later architecture.
2. `command.run.v1` starts a specifically consented executable as the user.
   Cgroup, pidfd, Landlock, seccomp, rlimits, literal argv, and pinned files
   contain lifecycle and common escape paths, but cannot make a dangerous
   executable semantically safe. Consent must continue to present it as a
   controlling/high-risk grant.
3. Generic input synthesis intentionally has no production v1 schema or
   registry entry. There is no half-trusted backend or compatibility
   placeholder.
4. RustSec reports no known vulnerability. One visible unmaintained transitive
   font-parser warning and its narrow rationale are tracked in
   `dependency-policy.md`; it is not suppressed.
5. zbus applies its own 128 MiB transport-frame ceiling before application code
   can inspect a message. Secret Service parsing now occurs in a disposable
   helper whose complete address space is capped at that amount, so such a frame
   cannot pressure the long-lived supervisor. Other desktop adapters retain
   one-message queues and reject bodies above 64 KiB before typed decoding; a
   generalized helper boundary remains an option if those already-authorized
   services are later treated as actively malicious high-volume peers.
6. Native plugins are intentionally unsandboxed, but their Wayland rendering
   traffic is still resource-bounded. A per-client 120 Hz token bucket with an
   eight-commit burst drops work before buffer readback/import; 256 rejections
   within one second disconnect the client. Frame callback queues are bounded
   and replaced pending buffers are released. The wire-level
   `test-native-commit-budget.sh` acceptance attempts up to 640 rapid commits
   in transport-safe batches and proves the compositor disconnects the
   offender while remaining alive.

Unsupported Landlock, cgroup-v2, pidfd, or seccomp requirements fail launch.
There are no backward-compatibility schemas, shims, or migrations in this v1
codebase.
