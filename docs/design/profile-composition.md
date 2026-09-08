# Profile and slot composition contract

Application focus selects a profile transactionally. A user document can
compose arbitrary cross-package layouts; a package can additionally provide a
bounded, package-local automatic profile that works immediately after the
plugin is enabled.

## Definitions

An **item** is a stable, package-qualified plugin surface definition. Its v1
identity is the pair `{ plugin, item }`: `plugin` is the package/runtime
identity and `item` is the package-local manifest item ID. Keeping the pair
structured allows unrelated packages to use familiar local names such as
`volume` or `status` without collisions. A
**contribution** is an ordered fragment containing items plus a
typed context predicate, scope, and selection priority. Contributions are
reusable and independent of profiles.

A **profile** is a named ordered template containing spacing and arbitrary
user-named slots, plus recursive composition groups. Slot names have no
built-in spatial meaning. A profile could have one slot, traditional
leading/content/status slots, or a specialized media arrangement.

A **group** is a stable composition node whose direct children are slots or
nested groups. `natural` groups preserve each child's intrinsic sizing;
`equal-width` groups assign their active direct children equal space. Groups
own spacing and compression priority and are hidden atomically according to
their visibility priority. Empty slot containers collapse completely, including
adjacent group spacing, then rejoin the group when context or presentation
content populates them. Nesting is bounded to eight resolver levels and the
final output remains a flat set of leaf surface rectangles.

A **region** is an optional named, exact global rectangle used only by the
presentation system. Regions are declared once at the top level rather than as
profile elements. They let a plugin request a known overlay area while the
compositor retains placement authority; they do not reserve space in the
compact bar.

Each slot has one policy:

- `fixed` includes all bindings regardless of their context predicates. This
  represents items the user wants in that profile at all times.
- `collect` includes every active contribution in user-defined binding order.
- `select` includes the highest-priority active contribution. Scope breaks
  priority ties, followed by stable binding order.

For example, this conceptual profile keeps volume and status visible while
changing only application content:

```text
profile default
  slot persistent fixed: volume
  flexible-space
  slot application select: terminal-fallback, browser, editor
  flexible-space
  slot status collect: recording, network, battery
```

Profile definitions bind reusable contributions rather than copying item
definitions. A `media` profile can bind the same volume contribution into a
different slot while retaining the exact item and plugin surface identity.

## Selection precedence

The state controller chooses the effective profile in this order:

```text
explicit user selection
highest-priority temporary mode lease (newest wins equal priority)
highest-priority automatic context rule (declaration order breaks ties)
fallback profile
```

Temporary modes are keyed by a control-owner identity and mode name. Releasing
an owner removes all of its leases, which will allow a future control socket to
clean up application state automatically on disconnect.

The controller accepts context updates, explicit profile selection and release,
mode activation and release, and owner release. This in-process command API is
the shared boundary intended for the Hyprland provider, `touchbarctl`, and
application clients; none should mutate compositor layout directly.

## Transactions and reconciliation

Each successful command produces an immutable, generation-numbered composition
snapshot containing the selected profile, active contributions, final ordered
bar, and stable item reconciliation sets:

- retained items preserve their existing plugin surface and local state;
- entering items become visible and receive compositor-selected geometry;
- leaving items remain retained but become invisible.

An invalid composition rolls back the entire command. If any touch contact is
captured, the newest valid snapshot replaces any older pending snapshot but is
not committed until every captured contact ends. Reconciliation is calculated
from the displayed snapshot to the final pending snapshot, preventing an
intermediate focus or profile state from leaking into the live scene.

The pure `touchbar-model` module deliberately does not define configuration
serialization, plugin package metadata, a desktop context source, or a control
transport. Those remain adapters around its semantics.

## Package-provided automatic profiles

A manifest `[[profile]]` names only items from the same immutable package and
matches exact normalized application classes and/or Hyprland layer namespaces.
When the plugin and stored profile choice are enabled, `touchbar-sessiond`
generates collision-free internal profile/contribution IDs and merges them with
the user document in memory. Package rules are assigned below the lowest user
priority, so a user rule wins even when it uses a negative priority. An active
foreground-layer activity is more specific than an application match.

Items may set `show_in_default_profile = false` when they make sense only in a
contextual profile. If no user document exists, enabled items that remain
default-visible form the fallback; contextual profiles are added around that
fallback. Disabling a referenced item also removes its package profile from the
effective catalog. `touchbarctl plugin profile SOURCE PROFILE enable|disable`
persists the profile choice across updates. Application and activity selection
uses ordinary context rules, not session leases or another daemon.

## Fresh-v1 user configuration

`touchbar-profile-config` is the strict serialization adapter. The default path
is `$XDG_CONFIG_HOME/touchbar/profiles.toml`; `TOUCHBAR_HOME`
places it at `$TOUCHBAR_HOME/profiles.toml`, and `touchbar-sessiond
--profiles FILE` selects an explicit file. If no file exists, the compositor
retains the simple all-connected-items layout until an enabled package provides
an automatic profile. It then synthesizes the equivalent fallback from
default-visible installed items and adds the contextual package profiles.

The format has only version 1. There are no aliases, migration readers, legacy
item forms, or compatibility shims. Unknown fields, unsupported versions,
duplicate identities, invalid references, oversized inputs, excessive
predicate nesting, symlinks, non-regular files, wrong-owner files, multiply
linked files, and group/world-writable files fail closed. A complete example is
checked in at `config/profiles.toml.example`.

```toml
version = 1
fallback = "default"

[[region]]
id = "center-palette"
x = 504
width = 1000

[[contribution]]
id = "volume"
items = [{ plugin = "github:cameroncooper/touchbar-controls", item = "volume", required = true }]

[[contribution]]
id = "battery"
items = [{ plugin = "github:cameroncooper/touchbar-controls", item = "battery", required = false }]

[[profile]]
id = "default"

[[profile.element]]
kind = "group"
id = "system-controls"
layout = "equal-width"
spacing = 4
visibility_priority = 1000

[[profile.element.element]]
kind = "slot"
id = "volume"
policy = "fixed"
contributions = ["volume"]

[[profile.element.element]]
kind = "slot"
id = "battery"
policy = "fixed"
contributions = ["battery"]
```

Optional items are inserted whenever their surface is connected. If any
required item is absent, that configured composition becomes unready and the
trusted system row is shown; reconnecting the item reconstructs the profile.
An optional principal item simply stops being principal while absent.
Region IDs use the same strict identifier grammar as profiles and slots. Their
nonzero rectangle must remain inside the live logical Touch Bar canvas.
Group and slot IDs are unique within a profile. Grouped slots accept item/group
content rather than fixed or flexible spaces because the group itself owns
their inter-child geometry.

The live adapter maps resolved qualified identities back to existing Wayland
surfaces, sends transition-only visibility events, updates compact geometry,
and issues configure events for changed dimensions. A package update or host
restart therefore preserves identity even though the surface object changes.

The client may receive several configure events before the compositor receives
an acknowledgement. Outstanding serial/geometry pairs therefore remain in an
ordered bounded queue. An acknowledgement promotes its matching geometry and
discards only older entries; newer configurations remain pending. This keeps
rapid focus changes legal while retaining the last acknowledged frame.

## Runtime control and context

The packaged user service connects to Hyprland's event socket. Application
classes and layer namespaces are normalized to lowercase; `activewindow`,
`workspace`, `workspacev2`, and `focusedmon` changes become typed
application/workspace facts. `openlayer` and `closelayer` maintain
reference-counted boolean `activity.NAMESPACE` facts, so a multi-monitor layer
does not deactivate until its final instance closes. Startup before Hyprland
and compositor restarts are nonfatal: the source
reconnects without restarting plugins or discarding profile state. The same
transactional controller defers context changes, manual selection, item
arrival/removal, and configuration replacement until captured gestures end.
An accepted appearance-file change also raises the host-owned
`activity.theme-change` fact for two seconds. A theme integration can use that
bounded tail to display its newly resolved palette after a picker layer closes.

The same-user bounded control socket provides:

```text
touchbarctl session status
touchbarctl session reload
touchbarctl session profile list
touchbarctl session profile select NAME
touchbarctl session profile automatic
```

Status reports whether configuration is loaded and ready, automatic versus
manual selection, the active and available profiles, and every missing
required qualified identity. `reload` reparses the profile file and reconciles
it through the same gesture-safe transaction path used by context updates. An
inotify watch on the narrowest existing configuration ancestor also detects
ordinary and atomic editor saves. Events are coalesced for 75 ms before the
strict loader runs, so the event loop remains asleep between real changes and
invalid or half-written replacements leave the live profile untouched.

`scripts/test-profile-control-live.sh` is the release acceptance. It runs three
independent GPU clients, proves that two packages may both expose local item
`shared`, switches profiles manually and automatically, reloads configuration,
falls back when a required client disappears, exposes a host-owned placeholder
in both pixels and machine-readable status, and removes it while restoring the
configured profile after that identity reconnects.
