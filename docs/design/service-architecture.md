# Runtime service architecture

The names describe process ownership, not implementation details.

## `touchbard`: system hardware service

`touchbard` is the one always-running, privileged service. It owns ADP DRM,
raw Touch Bar contacts, observation of the physical Fn key, a discard-only
`seat0` activity source, Touch Bar backlight policy, session arbitration, and
one uinput device that advertises only the fixed v1 system-key set. It does not
load plugins, evaluate profiles, read user themes, access user files, or
provide general command execution.

At boot and whenever no authenticated user service is connected, it renders a
small built-in fallback. The default row contains media, display, keyboard,
search, microphone, and volume keys. Fn is a transient layer switch to F1–F12,
not a profile change. Active contacts are cancelled before either layer switch
or ownership handoff so a key cannot remain logically pressed. The production
service retains DRM master, raw input readers, uinput, backlight state, one
dedicated fallback buffer, and three session buffers across handoffs; the
fallback remains scanned out until the first completed user frame arrives.
The panel is lit at hardware-service startup, dims to the Tiny DFR low level
after 30 seconds without observed activity, and turns off after 60 seconds.
Touch Bar contacts and ordinary `seat0` keyboard, pointer, gesture, and touch
activity reset the timer. The libinput observer reduces each drained batch to
one activity boolean inside `touchbard`; event types, codes, coordinates, and
device identities never enter the hardware-session protocol or user process.
The dedicated Touch Bar and Fn readers remain the authoritative interaction
sources. Every contact delivered in the input batch that wakes the fully dark
panel is consumed through release, so the user cannot activate a control they
could not see; a touch on the still-visible dimmed panel remains actionable. A
user compositor's first completed frame also wakes a previously idle panel;
later animation frames do not reset the idle timer.

Holding the physical Fn key continuously for eight seconds toggles a
hardware-owned recovery latch. Entering recovery drops the authenticated
session socket, releases every virtual key, and keeps the compiled-in fallback
active; reconnect attempts are rejected until a second release-and-eight-second
hold clears the latch. The gesture requires no invisible Touch Bar target and
uses no additional keyboard data. A root-owned presence marker beside the
hardware socket exposes only the latch state to `touchbarctl hardware status`.

## `touchbar-sessiond`: per-user experience service

`touchbar-sessiond` is the GPU compositor and policy engine for one graphical
login. It owns user profiles, slot composition, focus/context rules, themes,
plugin supervision, semantic gestures, popovers, and animation scheduling. It
runs without DRM master, raw keyboard access, raw Touch Bar input, or uinput.
Its user unit is wanted by and part of `graphical-session.target`, so it follows
the actual desktop login lifetime even when the user service manager lingers
between sessions.

After `touchbard` verifies the Unix peer credentials against logind's active
`seat0` user, it lends a bounded XRGB8888 DMA-BUF swapchain and sends only
normalized Touch Bar contacts plus Fn state. The session daemon returns frame
availability and typed system-key transitions. Arbitrary evdev codes cannot
cross the protocol.

The compositor is a library inside this process, not another daemon. A name
such as `touchbar-compositor` would expose an implementation detail while
obscuring the important system-session boundary.

The built-in system scene is not a permanent opaque layer over plugins. It is
visible while the selected user composition has no completed content, hides as
soon as the first valid plugin frame is retained, and returns if that content
goes away. Holding Fn cancels captured plugin gestures and temporarily places
the trusted F1–F12 scene above the current profile; releasing Fn restores the
same profile and retained plugin state.

## `touchbarctl`: one administrative interface

`touchbarctl` is the human and automation-facing command. `touchbarctl session`
talks to `touchbar-sessiond`; `touchbarctl hardware status` reports systemd,
socket, and recovery-latch state. Explicit activation and rollback remain
polkit-guarded installed commands. Callers do not need to know which internal
daemon implements a particular operation.

`touchbarctl session status` returns mandatory fresh-v1 runtime state alongside
plugin process health: whether the authenticated hardware channel is connected,
the observed Fn state, whether the trusted system scene is visible, and the
configured/ready/mode/active/missing state of user profiles. `session profile
list|select|automatic` changes selection through the same transactional state
controller used by focus updates, while `session reload` reconciles both plugin
and profile configuration. This
lets lifecycle tests distinguish a functioning compositor/hardware handoff from
two unrelated processes that merely happen to be running.

```text
TouchBar plugins ── private Wayland ──▶ touchbar-sessiond (user)
                                             │
                         final DMA-BUF frames│ normalized touch/Fn
                                             ▼
                                      touchbard (system)
                                             │
                                      DRM / evdev / uinput
```

## Failure and ownership rules

- The hardware service starts independently of a graphical login.
- Exactly one active-seat user session may attach.
- The hardware fallback remains the recovery scene; it returns after logout,
  protocol failure, compositor crash, or rejected authentication.
- An eight-second physical Fn hold latches that fallback even when the user
  compositor is still running; release and repeat to admit sessions again.
- SIGTERM/SIGINT are received through `signalfd` in the hardware event loop;
  shutdown releases every tracked virtual key, dims the OLED, removes the
  service socket, and closes DRM before systemd starts a replacement manager.
- DRM device removal stops `touchbard` through its bound systemd device unit;
  udev starts it again when the display returns. Raw touch/Fn descriptor
  hangups are treated as fatal hardware loss rather than a busy loop. The user
  compositor drops the retired swapchain and retries the authenticated hardware
  connection once per second without restarting plugins or profiles.
- User theming never affects the recovery dependency chain. The fallback uses
  compiled-in Tiny DFR/Apple-style appearance values: black canvas, 20% gray
  inactive controls, 40% gray pressed controls, 16-pixel horizontal spacing,
  15%/85% vertical bounds, 8-pixel radii, bold 32-pixel function labels, and
  the same 48-pixel Material symbols shipped by Tiny DFR.
- A normal user session may theme and reorganize its built-in system controls
  like other trusted contributions. Fn remains a transient layer owned by the
  compositor, so it does not destroy or replace the selected profile.
- This is a greenfield v1 contract. There are no old daemon aliases, protocol
  adapters, migration readers, or compatibility shims.

`scripts/test-system-scene-handoff.sh` deterministically verifies on the Apple
GPU that the empty-profile scene hides for live DMA-BUF content and returns
after an ungraceful client disconnect.

`scripts/run-hardware-fallback-physical.sh 15` is the isolated visual check for
the system daemon's recovery scene. It starts no user compositor and performs
no timed hardware-daemon restart: the media row must be visible immediately,
Fn must show F1–F12 only while held, and releasing Fn must restore media. This
separates initial fallback behavior from every session-handoff timing effect.
All guarded physical runners detect the current hardware owner, temporarily
pause either an installed `touchbard` or `tiny-dfr`, and restore that exact
owner on success, failure, or interruption. Ambiguous simultaneous ownership
fails before either service or hardware is touched.

`scripts/run-service-handoff-physical.sh 20` passed on the M1 hardware on
2026-09-05. The hardware service initialized ADP, the 60×2008 mode, raw touch,
Fn, restricted uinput, and panel-summit backlight; accepted UID 1001 through
logind/SO_PEERCRED; scanned out the first user DMA-BUF; restored its fallback
after session termination; then released keys, dimmed the OLED, closed cleanly,
and restored tiny-dfr to `active/running`.

The strengthened restart acceptance also passed on 2026-09-05. It terminated
the first hardware daemon cleanly, retained the running user compositor, started
a fresh hardware daemon and swapchain, observed `hardware-output=reconnected`,
and queried the v1 control plane while it reported
`hardware_connected: true`. The second daemon restored fallback after the user
session exited and again returned ownership to active tiny-dfr.

The first installed activation exposed two production-only integration bugs
and now supplies regression coverage for both. The TouchBar udev rule sorts
after tiny-dfr's rule and appends rather than replaces device aliases, while
activation retriggers exactly the one recognized ADP card after installing a
new rule. The hardened user unit keeps `PrivateTmp=true`; the compositor now
selects Mesa's surfaceless EGL platform directly instead of inheriting an X11
display dependency through `/tmp/.X11-unix`.

`scripts/test-sessiond-hardening.sh` launches the release compositor in a
bounded transient user unit with the packaged restrictions and requires a
non-llvmpipe renderer, DMA-BUF feedback, and protocol readiness before the
intentional timeout. `scripts/test-packaging.sh` runs this probe after its
static unit, udev, and installer checks.

The runner performs a foreground polkit authorization preflight that makes no
hardware or service changes, then starts the bounded root phase. This keeps an
authorization prompt from racing the hardware-socket readiness deadline.

`scripts/test-installed-lifecycle.sh` checks the installed service boundary;
its explicitly gated `--suspend` mode performs the final suspend/resume
acceptance and prints both journals after the machine wakes.

`scripts/test-installed-recovery.sh` observes an already-active installation
while the user performs the two Fn holds. It requires the fallback latch,
session disconnect, latch release, and automatic authenticated reconnect in
that order without stopping either service itself.
