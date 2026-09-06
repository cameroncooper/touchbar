# Plugin ecosystem and sandbox plan

This document defines how TouchBar plugins are authored, executed, published,
discovered, installed, updated, and trusted. It extends the existing native
process architecture rather than replacing it.

## Decisions

1. `touchbar-sessiond` remains a compositor and policy engine. It never loads plugin
   code into its own address space.
2. Native plugins remain supported as a trusted escape hatch with direct
   Wayland/GLES and ordinary session access.
3. Sandboxed WebAssembly Components become the preferred format for shared
   community plugins.
4. Rust is the first-class SDK, but the ABI is a versioned, language-neutral
   WIT world rather than a Rust ABI.
5. A separate `touchbar-plugin-host` process runs one component pack, renders
   it through the native GPU UI kit, and connects to `touchbar-sessiond` as an ordinary
   private Wayland client.
6. Plugins request generic, scoped capabilities. Integration helpers such as
   MPRIS, PipeWire, and Hyprland adapters are SDK libraries built on those
   capabilities rather than privileged special cases in the compositor.
7. GitHub Releases store packages. A small pull-request-driven catalog provides
   discovery and aliases but is not a binary repository or a prerequisite for
   installation.

## Runtime boundary

```text
native executable ───────── private Wayland/GLES ────────┐
                                                         ▼
component.wasm ─ WIT ─ touchbar-plugin-host ─ Wayland ─ touchbar-sessiond ─ ADP
                       │
                       └─ explicitly granted capability brokers
```

The component does not receive the Touch Bar Wayland socket, the session bus,
the user's environment, or raw GPU driver access. The host exposes only the
imports granted by installation policy. Each component host receives CPU,
memory, table, response, asset, drawing, logging, and restart budgets. Process
isolation using namespaces, seccomp, and Landlock is defense in depth around
the Wasmtime capability boundary.

### Rendering

Sandboxed plugins send retained UI and drawing intent to their host. The host
owns text shaping, layout, hit testing, textures, animation scheduling, and GPU
submission. Theme values are semantic tokens supplied with the current host
snapshot, so text, lines, shapes, SVG masks, image tint, graphs, and animation
effects react to live theme changes without plugin restart.

The standard path includes UI-kit nodes, Canvas2D-style paths, and declarative
host-timed animations. A later advanced tier may accept validated WGSL effects
with bounded uniforms, textures, passes, and frame cost. Raw GLES remains a
native-plugin facility.

## Package contract

Every release contains a self-contained package:

```text
touchbar-plugin.toml
component/plugin.wasm              # component runtime
native/<target>/plugin             # native runtime
assets/
licenses/
README.md
screenshots/
```

`touchbar-plugin.toml` owns package-provided facts:

- canonical `github:owner/repository` source identity;
- version, description, license, authors, and host API requirement;
- component world and artifact, or target-specific native artifacts;
- multiple stable item contributions;
- bounded package-local presentation bars, with container sizing, ordered
  item/space elements, presentation sizing per item, an optional principal,
  compositor-managed selection dismissal, and per-item tap/hold references;
- required and optional capability requests with human-readable reasons and
  scopes.
- typed PNG and symbolic-SVG assets with stable logical IDs, exact decoded
  dimensions, and host-applied semantic tint.

The package cannot claim that it is signed, reviewed, safe, or installed from a
particular digest. The installer records those observations separately.

## Capabilities

Capabilities are general mechanisms with narrow scopes:

- filesystem read/write with explicit preopened paths;
- HTTP with host and port allowlists;
- D-Bus with bus, name, interface, and method constraints;
- explicit executable or command-template invocation;
- clipboard read and write as separate grants;
- named secret access without the ambient environment;
- notifications, time, timers, and bounded local IPC;
- compositor-owned system and focus context.

Generic input synthesis has no v1 schema. A future named-action design must be
implemented and reviewed as new controlling authority before it enters this
catalog.

Required capabilities prevent activation when denied. Optional capabilities
must allow a degraded experience. Arbitrary command execution, broad home
access, unrestricted D-Bus, or equivalent grants are displayed as effectively
trusted access even when the payload is WebAssembly.

## Developer and agent experience

The supported path is one discoverable CLI workflow:

```text
touchbarctl plugin new <name> --source github:owner/repository
touchbarctl plugin build
touchbarctl plugin check --format json
touchbarctl plugin test --format json
touchbarctl plugin dev
touchbarctl plugin pack
touchbarctl plugin release-check --tag vMAJOR.MINOR.PATCH --repository owner/repository
touchbarctl plugin submit --alias <name> --categories <category,...>
```

The SDK provides generated WIT bindings and a presentation-aware starter with
semantic theming, tap-to-open, hold-slide, and lifecycle handling. The CLI
supplies manifest validation, optional physical deployment, responsive compact
and presentation width matrices, and versioned machine-readable diagnostics.
Deterministic scenarios now drive production coordinate hit testing, capture,
sliders, stationary holds, presentation callbacks, and fake theme changes while
recording versioned semantic and primitive snapshots. Scoped fake
application/workspace context and exact D-Bus calls/subscriptions already
traverse the real asynchronous broker ABI; D-Bus fixtures are checked by the
production scope and physical-activation authorizer. Named checkpoints
optionally render create-only PNG goldens through a surfaceless production GLES
renderer. `touchbarctl plugin dev` launches the complete package in an isolated
onscreen production-compositor session with synthetic mouse/touch input. Exact
HTTP inline/stream fixtures cover network-backed plugin state without network
access. Exact command fixtures cover Command Deck and developer-tool actions,
including typed arguments, ordered stdout/stderr, exit state, and manifest
output limits, without launching a process. Exact filesystem-read fixtures
cover logical-mount listing and inline/streamed file state without selecting or
opening a host directory. Exact local-service fixtures cover framed
connect/send/receive/close flows without creating or contacting a Unix socket.
Exact notification and URI-open fixtures cover bounded desktop outcomes without
contacting the portal; URI replay retains the production physical-activation
gate. Exact clipboard fixtures cover separately granted reads and writes with
MIME, byte, binding, activation, and replay-clock rate checks without contacting
Wayland. Exact secret fixtures exercise logical-name, binding, activation,
content-type, and byte constraints without contacting Secret Service or
printing value material. Exact filesystem-mutation fixtures cover inline and
streamed writes, production scope/quota authorization, commit/abort lifecycle,
and receipt consistency without touching a host path.

`touchbarctl plugin context --format markdown|json` produces a self-contained
agent context bundle for the installed SDK version: exact APIs, capability
catalog, theme tokens, layout constraints, examples, and build/test/deploy
commands. Repository templates include `AGENTS.md`, CI, fixtures, and release
automation so an agent does not have to infer conventions from implementation
crates or stale web documentation.

## GitHub publication and installation

Any public GitHub repository can be installed:

```text
touchbarctl plugin add github:alice/touchbar-media
touchbarctl plugin add https://github.com/alice/touchbar-media
touchbarctl plugin add github:alice/touchbar-media --version 1.2.3
```

Stable installation downloads a standard package asset from a compatible
GitHub Release. It never builds or executes the default branch. Before running
anything, the installer verifies the release asset digest and available
attestation, validates the manifest, checks host compatibility, displays
permissions, and records the exact repository identity, version, tag, and
digest in a lock file. Immutable releases are recommended.

Local `--path` installation is visibly marked as an unverified development
source. Native development sources receive a stronger warning.

Updates remain on the same GitHub repository identity, show capability diffs,
and request consent only for new access. The previous content-addressed package
is retained for rollback. Repository transfers or identity changes require
confirmation.

The fresh v1 standard asset name is
`touchbar-plugin.touchbar`, attached to a canonical
`vMAJOR.MINOR.PATCH` tag. The installer uses GitHub's
[versioned Releases REST API](https://docs.github.com/en/rest/releases/releases)
and
requires the API-supplied `sha256:` asset digest; it checks both declared size
and streamed bytes before the archive parser sees the file. Repository
redirects are rejected as identity changes, while asset redirects are bounded
to GitHub-owned HTTPS hosts. Drafts, prereleases, ambiguous assets, missing
digests, noncanonical tags, manifest/source mismatches, unsupported host APIs,
and malformed packages fail without replacing the current lock.

Public installation works anonymously and has no `gh`, `git`, or `curl`
dependency. `GITHUB_TOKEN` is optional for installation and may be set to raise
GitHub's API allowance; an empty token is rejected rather than sending an
ambiguous credential. Rate-limit responses report GitHub's bounded message,
request ID, and advertised retry/reset value, then fail immediately instead of
sleeping inside an interactive or agent-driven install.

The generated publisher uses `touchbarctl plugin publish`, not an external
GitHub CLI. It verifies the exact remote tag before creating a draft release,
stages the package into a private bounded file, accepts only GitHub's exact
repository/release upload endpoint, verifies returned asset size and SHA-256,
and publishes the draft only after the upload succeeds. Upload or finalization
failure triggers deletion of the incomplete draft. Authentication comes only
from the workflow's `GITHUB_TOKEN`; redirects and environment proxies are
disabled for authenticated publishing requests.

After the byte digest is verified, the installer asks GitHub's artifact
attestation API whether provenance exists for that digest. Absence remains a
visible `attested=false` state because third-party publishers are not required
to adopt attestations. If GitHub advertises an attestation, however, installation
fails unless the in-process Rust verifier validates the DSSE signature, Fulcio
chain, embedded SCT, Rekor SET, Merkle inclusion proof and signed checkpoint,
artifact digest, numeric repository identity, exact release tag, and canonical
`.github/workflows/release.yml` signer. The installer fetches the bounded
raw-Snappy bundle from GitHub's exact attestation storage host and never shells
out to `gh`; `octocrab` is unnecessary because the existing bounded `reqwest`
client already owns the small REST surface.
This prevents a malformed or unverifiable advertised attestation from being
silently downgraded. Attestation state is recorded independently from release
immutability: an immutable release controls reusable publisher provenance,
while the attestation records cryptographically checked build provenance.

Installer-owned origin is passed through the session launcher to the
supervisor. An immutable GitHub release is `verified-release`; a mutable one is
`unverified-release`; `--path` is `local-development`. This distinction is not
package-controlled and directly governs whether verified-source grants may be
reused. Release updates retain prior content-addressed snapshots for
`touchbarctl plugin rollback`. An enabled component is disabled if an update
expands its requested authority, and enabled native code is disabled whenever
its package changes; the user must inspect and explicitly re-enable it.

## Catalog

The core framework lives at `github.com/cameroncooper/touchbar`. Discovery
metadata lives in a separate `github.com/cameroncooper/touchbar-plugins`
repository so catalog review, releases, and ownership do not churn the hardware
and compositor code. That repository contains only records, schemas, review
automation, and permanent alias tombstones; it never mirrors plugin source or
release artifacts.

Each independently versioned plugin or pack has its own GitHub repository and
publishes the standard release asset. The installer accepts any public
`github:owner/repository` whether or not it is listed. A catalog PR adds
discoverability, not execution authority or ownership transfer. Built-in
fallback controls remain core code because they are part of hardware recovery;
first-party feature packs follow exactly the same external-repository contract
as community packs.

The catalog stores small reviewed records mapping unique aliases to GitHub
sources. It does not mirror artifacts:

```toml
alias = "media-controls"
source = "github:alice/touchbar-media"
name = "Media Controls"
description = "MPRIS media controls and timeline"
categories = ["media"]
tier = "listed"
state = "active"
```

Catalog changes arrive through pull requests and are compiled into
`touchbarctl`; this prevents a mutable discovery document from silently
retargeting an alias between core releases. CI resolves the exact GitHub
identity without repository redirects, downloads the latest stable fixed-name
asset into an isolated temporary store, and verifies release availability,
streamed digest, advertised attestation, manifest source/version, host API,
archive/artifact integrity, metadata, and permission syntax. Search is local
and deterministic. A catalog alias is resolved only for a fresh add; updates
and rollback always use the canonical installed `github:owner/repository`
identity, so later alias changes cannot redirect an existing installation.
Accepted aliases are permanent: removal is represented by a one-way
`state = "retired"` tombstone, never deletion or reuse, and pull-request CI
compares the proposed catalog with its base to enforce that transition.
Catalog signals remain distinct:

- **Listed:** packaging and metadata passed automated checks.
- **Verified publisher:** release provenance matches the expected source.
- **Curated:** maintainers reviewed and recommend the experience.
- **First-party:** maintained with the core project.

A plugin does not need a catalog entry to be installed. Its canonical identity
is always `github:owner/repository`; display names need not be globally unique.
`touchbarctl plugin submit` emits a reviewer-ready entry derived from the
validated manifest but can only request `listed`; maintainer review owns the
stronger tiers. See [`catalog/README.md`](../../catalog/README.md).

## Engineering sequence

### E0 — Local lifecycle

**Status: complete (2026-09-04).** Author scaffolding, local validation and test matrices, the fresh
v1 package archive, content-addressed installation, persistent enable/item/width state, a same-user
daemon control protocol, supervised installed-component launch, reconciliation, status, and bounded
crash restart are implemented. See [`local-plugin-lifecycle.md`](local-plugin-lifecycle.md).

### E1 — Package contract

- Shared strict manifest parser and structured validator.
- Common native/component runtime descriptions.
- Stable multi-item contributions and permission declarations.
- Valid and invalid fixtures with aggregated diagnostics.

### E2 — Component vertical slice

**Status: complete (2026-09-04).**

- Versioned `touchbar:plugin/plugin@1.0.0` WIT world.
- Wasmtime-based one-pack-per-process host with memory, table, instance, fuel,
  node, depth, expanded-tree, and text limits.
- Lifecycle, complete theme snapshot, viewport, retained text/button/layout UI,
  activation input, and a stateful two-item Rust component.
- Flat guest node arenas validated for indices, cycles, finite dimensions, and
  flex invariants before conversion to the native GPU UI kit.
- A live host mode registers one selected package item with `touchbar-sessiond`, rebuilds
  it at compositor-assigned dimensions, propagates appearance snapshots, draws
  with the native GLES renderer, and routes captured press/activate/release/
  cancel events back through WIT.
- Headless semantic/scene resolver and deterministic integration runner. A
  default-deny WASI context supplies clocks, random, polling, and closed stdio
  so ordinary Rust `std` works; it supplies no arguments, environment,
  preopened filesystem paths, allowed socket addresses, or session services.

Build and exercise the demo with:

```text
./scripts/run-component-host.sh hello 160 1
./scripts/run-component-host.sh theme 100
./scripts/test-component-host.sh
./scripts/run-component-ui.sh 160
./scripts/run-component-ui-physical.sh 15 160 hello
./scripts/run-component-ui-physical.sh 15 160 theme
```

The Rust `wasm32-wasip2` standard library is needed when rebuilding the guest.
Once built, the cached package can be exercised without that target.

### E3 — Capability and consent foundation

**E3.1 policy core complete (2026-09-04).** The normative threat model,
authority separation, initial capability catalog, consent/revocation behavior,
broker ABI, risk labels, and adversarial acceptance matrix are in
[`capability-consent.md`](capability-consent.md).

The new `touchbar-policy` crate turns manifest permissions into typed scopes,
computes update-safe subsets and effective grants, derives risk instead of
trusting package labels, and persists user decisions with locked atomic writes.
The first E3.2 slice now connects it to a supervisor-owned inherited
`SOCK_SEQPACKET` transport. The supervisor locks source/version/digest before
launch, the wire format cannot assert identity or grants, and the host reports
optional capability status while preserving manual-launch default denial.
Lifecycle completion precedes the first real broker backend.

The follow-up E3.2 lifecycle slice now supplies capability-owned pending work
and resources, selective revocation, single-use trusted activation, coalesced
state events, ordered edge events with overflow markers, payload-free audit
records, and deterministic restart backoff. These stay backend-neutral so MPRIS,
filesystem, HTTP, and later integrations share identical cancellation and
health behavior.

A bounded backend-neutral worker runtime now composes those primitives. It
contains backend panics, applies deadlines and cancellation after execution,
and delivers completions through the same bounded event path. The executable
supervisor now waits on the broker socket and worker `eventfd` together, routes
only granted requests, binds normalized scope to each backend job, and handles
cancellation and revocation end to end. No OS backend is registered by default;
the next integration layer is permission lifecycle and the versioned WIT
surface.

Grant watching is now connected as well. The supervisor watches the containing
directory so atomic permission-file replacement is visible, recalculates policy
from a freshly validated store, updates optional capabilities in place, and
stops a host immediately when required authority disappears. It waits without
executing the component while blocked and creates a fresh instance after the
grant is restored. Invalid replacement files fail closed. Durable audit output
and the WIT SDK bridge are the final E3.2 integration layers.

Durable audit output is now available through the supervisor's explicit
`--audit FILE` option. The JSON-lines sink is private, synced, size/age rotated,
cross-process locked, structurally unable to contain broker payloads, and
monotonically sequenced across rotation and host restart. The touchbar-sessiond launcher
will eventually choose its standard XDG state path; keeping the low-level
supervisor option explicit prevents tests and manual component runs from
silently mutating user state.

The supervisor also requires an explicit private `--state DIRECTORY`. It must
already exist, be owned by the invoking user, and grant no group or other
permissions. Security counters that must survive a component or supervisor
restart live there under source-hashed names. The filesystem-write broker uses
an atomically replaced, cross-process-locked rolling 60-minute ledger, so
restarting a misbehaving plugin cannot replenish its byte allowance. The
launcher, rather than a package, chooses this directory.

The WIT SDK bridge is now implemented. The single
`touchbar:plugin/plugin@1.0.0` world provides typed, nonblocking broker imports
and requires a host-event callback. The host delivers asynchronous
completion and capability events through a guest callback while its native
event loop waits on Wayland and broker readiness together. The
`touchbar-component-sdk` crate re-exports the event-aware generated bindings
and thin request-ID/resource-ID wrappers. The compile-checked
`examples/broker-component-plugin` package is the reference starting point for
agents and humans.

E3.2 is complete with v1 activation sequencing. `touchbar-sessiond` assigns a
64-bit sequence to each captured physical gesture, the component host attaches
short-lived context only during its activation callback, and the supervisor
validates it before backend dispatch. E3.3 now provides the first actual service
integration: exact-scope MPRIS property reads and activation-gated Play/Pause
through `dbus.call.v1`, plus ordered MPRIS `PropertiesChanged` events through a
broker-owned `dbus.subscribe.v1` resource. The resource is bounded, explicitly
closed by the SDK, and forcibly closed whenever its granted authority changes.

- Permission store separate from package metadata.
- Filesystem, HTTP, D-Bus, and constrained command brokers.
- Required/optional degradation and permission-diff UX.
- Effective-trust classification and structured audit events.

The user consent control surface is now connected end to end. `touchbarctl`
reviews normalized effective policy and atomically records explicit
session-only or persistent allow/deny/reset decisions. The compositor clears
session policy at startup, supervisors watch both stores and apply session
precedence live, and resource-bearing grants accept only explicit canonical
host-owned bindings. Concurrent CLI writers cannot lose unrelated decisions.
Native-process declarations remain clearly disclosure-only.

### E4 — Tooling and GitHub delivery

**Status: complete (2026-09-05).** Local tooling was completed in E0. GitHub
delivery supports canonical source IDs and exact HTTPS
GitHub URLs, latest or explicit semantic versions, fixed release assets,
bounded download and digest verification, strict identity/API/package
validation, immutable-versus-mutable provenance, permission diffs, safe
disable-on-expanded-authority, retained release history, and offline rollback.
The standalone author scaffold now also generates theme-aware multi-item starter
code, a package-local tap/hold presentation, agent guidance, CI, semantic release
validation, the fixed release asset, a native transactional publisher, and a
GitHub provenance attestation. Its
test command exercises the standard 80/160/320/1004/2008 matrix plus every
declared presentation width and can return the exact matrix as versioned JSON.
The generated project is itself covered by create/build/check/test/pack
acceptance. Installs verify any advertised attestation and record its result.
The strict bundled catalog adds deterministic local search, fresh-install
aliases, permanent identity/tombstone transitions, PR validation against the
base catalog, isolated online release/conformance validation, and
manifest-derived submission output. Direct GitHub installation remains
independent of catalog review.

- Scaffold, dev, test, check, pack, and context commands.
- Content-addressed installation, lock file, update, and rollback.
- Native GitHub Release publication, digest/attestation verification, and
  immutable-release guidance.
- Catalog schema, validation CI, search, aliases, and submission command.

### E5 — Advanced visual content

- ~~Canvas2D command nodes and quotas.~~ The first slice provides responsive
  view-box geometry, semantic or literal alpha paint, rectangles, circles,
  arbitrary lines/polylines, text, two-stop semantic linear gradients, and
  stroked and simple filled line/quadratic/cubic path grammar. Host validation
  applies independent
  command, point, coordinate, color, and shared text budgets before native
  retained layout or GLES. The first-party Media component is verified through
  Wasm, the native Apple M1 renderer, and DMA-BUF output. See
  [`canvas2d.md`](canvas2d.md).
- ~~Host-timed animation descriptions.~~ Stable component-owned IDs preserve
  host-owned phase across guest rerenders; one-shot, loop, and alternate
  transforms are validated, sampled in native GLES, paced by compositor frame
  callbacks, suspended while hidden, and settled under reduced-motion policy.
  The Apple-GPU acceptance proves 60 native frames from one guest render. See
  [`host-timed-animation.md`](host-timed-animation.md).
- ~~Sealed package images and symbolic SVG.~~ Typed manifest entries are locked
  as installer artifacts, transferred in one verified sealed bundle, decoded
  under confinement, and referenced from Wasm only by logical ID. PNG and
  self-contained symbolic SVG have strict byte/pixel limits; semantic
  multiply/mask tint stays live without texture re-upload. The official
  TouchBar wordmark proves installed and direct Apple-GPU paths. See
  [`package-assets.md`](package-assets.md).
- ~~Validated, bounded WGSL effect nodes.~~ A dynamically sized retained leaf
  accepts only a 4 KiB straight-line body with fixed geometry, time, parameter,
  and live semantic-theme inputs. Naga validation plus a second IR allowlist
  excludes guest functions, branches, loops, mutation, textures, storage,
  atomics, and dynamic indexing before canonical GLES 3.0 translation. Program,
  node, expression, statement, parameter, and animation limits are independent;
  reduced motion settles after one frame. Media proves 60 Apple-M1 DMA-BUF
  frames from one guest render. See
  [`validated-effects.md`](validated-effects.md).
- Performance and power acceptance. The compositor now has event/deadline-
  driven idle behavior: one static Apple-GPU frame settled to eight voluntary
  event-loop switches and zero CPU ticks over two seconds, while animated and
  reduced-motion paths retain their 60-frame and one-frame contracts.
  Malicious-input release gates already cover component, protocol, manifest,
  policy, asset, Canvas, and effect boundaries. Installed suspend/resume and a
  controlled battery-energy sample remain explicit physical gates. See
  [`performance-and-power.md`](performance-and-power.md).
