# Performance and power policy

**Status:** event-driven idle slice implemented and Apple-M1 verified,
2026-09-05. Battery-aware animation cadence is implemented and Apple-M1
verified with a fake kernel power-supply tree. Installed suspend/resume and the
complete OLED dim/off/wake-only lifecycle passed on 2026-09-06.
Battery-discharge characterization was explicitly waived on this development
machine because its failing battery cannot provide a valid unplugged sample.

The Touch Bar is mostly static. A compositor that redraws nothing but wakes a
thousand times per second is still a poor laptop citizen, so frame count alone
is not the idle-power contract.

## Session event loop

`touchbar-sessiond` waits in one `ppoll` over:

- the Wayland client backend and listening socket;
- its same-user control listener;
- an `eventfd` signalled by the blocking hardware-protocol reader;
- an `eventfd` signalled by the blocking Hyprland-context reader.

The wait is shortened only by a real deadline:

- the next requested theme/power-capped animation frame;
- an outstanding nonblocking GPU completion fence;
- a hardware reconnect attempt;
- the 250 ms theme-file or supervised-child maintenance check;
- deterministic development replay timing;
- the one-second bounded kernel power-source status check.

A surface commit, Touch Bar/Fn event, hardware disconnect, context update,
control request, or new client therefore wakes the loop immediately. Static
content has no frame deadline and submits no repeated frame. A long-idle frame
deadline advances arithmetically rather than replaying every missed interval.

The two reader threads never call compositor state. They place bounded protocol
events on their existing channels and increment an `eventfd`; the main loop
drains both. This retains single-threaded scene and input ownership without a
millisecond channel poll.

## Acceptance

`scripts/test-sessiond-idle.sh` runs the release compositor on the native Apple
GPU with one static system scene. Across a two-second settled interval it
requires:

- exactly one rendered frame in total;
- no more than 32 voluntary event-loop context switches.

The 2026-09-05 M1 result was one frame, eight context switches, and zero CPU
ticks during the measured interval. The preceding fixed one-millisecond loop
produced about 1,800 voluntary switches over the same interval. The existing
Canvas/effect acceptance simultaneously proves that requested animation still
delivers 60 changing Apple-GPU DMA-BUF frames, while reduced motion settles
after one.

Raw native clients have the same event-driven option. Returning
`FrameFlow::Wait` submits the current GLES frame without requesting another
frame callback; input, appearance, visibility, configuration, and explicit
external events can still request a redraw. `scripts/test-static-native-client.sh`
runs the real native SDK and compositor over Wayland and DMA-BUF. The
2026-09-05 M1 result held one committed frame for two seconds with zero client
CPU ticks and zero voluntary context switches while the client remained
connected.

The threshold deliberately allows scheduler and 250 ms maintenance jitter; it
is a regression boundary, not a power-consumption claim. Actual battery energy
depends on panel brightness, animation, system load, charge state, and kernel
sensor availability.

## Battery-aware animation cadence

The compositor reads only bounded `type` and `online` attributes beneath
`/sys/class/power_supply`. An online kernel-classified external source wins over
a present battery; a present battery with no online source selects battery
mode; missing or malformed data remains `unknown` and never silently reduces
performance. It polls once per second and changes only the frame-callback
deadline—no power setting, charging state, or device is mutated.

The theme owns both caps:

```toml
animation_hz = 60
battery_animation_hz = 30
```

Both values are strict integers from 1 through 60. Defaults are 60 Hz on
external or unknown power and 30 Hz on battery. Users who want full-rate
visuals while unplugged can set `battery_animation_hz = 60`; quieter themes can
choose a lower rate. This cadence is independent of `motion`: `full` motion is
sampled at the selected rate, while `reduced` and `disabled` still settle and
stop continuous scheduling. Raw native clients remain subject to the separate
per-client commit budget even if they ignore frame callbacks.

`touchbarctl session status` exposes `power_source` and
`animation_frame_rate_hz`. `scripts/test-power-aware-animation.sh` supplies a
private fake sysfs tree and proves 60 Hz external and 30 Hz battery rendering
through the real Apple GPU without changing hardware state.

## OLED idle policy

The privileged hardware service owns a two-stage policy matching Tiny DFR's
timing: dim after 30 seconds of input inactivity and switch the backlight off
after 60 seconds. Keyboard activity or Touch Bar input wakes the strip. When it
was fully off, the complete waking touch contact is swallowed through release;
the following contact is the first one eligible for hit testing. This rule does
not apply while merely dimmed because the controls remain visible.

This timing is taken directly from Tiny DFR 0.3.7 rather than inferred from
runtime behavior: its [main loop defines a 10-second base timeout](https://github.com/AsahiLinux/tiny-dfr/blob/v0.3.7/src/main.rs),
and its [backlight manager uses three intervals before dimming and six before
off](https://github.com/AsahiLinux/tiny-dfr/blob/v0.3.7/src/backlight.rs). Tiny
DFR also updates activity before rejecting Touch Bar events while brightness is
zero, so the waking contact does not activate a key. This project tracks that
observable contract explicitly across every event in the contact's lifetime.

## Physical acceptance status

The installed acceptance records:

1. Passed: service and authenticated hardware-session recovery across a real
   ten-second s2idle suspend/resume on 2026-09-06.
2. Passed: full brightness through 30 seconds, level-1 dim by 35 seconds, fully
   off at 60 seconds, and a first dark-panel contact that restored brightness
   without activating a control or changing audio state.
3. Waived for this machine: the battery is failing and cannot support a valid
   unplugged static-versus-animated energy sample. Do not run that measurement
   on this target.

These operations are never hidden in the normal test suite because suspending
or asking the user to unplug a workstation is an explicit physical action.

The battery comparison has a guarded runner:

```bash
./scripts/measure-installed-power.sh --check
./scripts/measure-installed-power.sh --run 20
```

`--check` is nonphysical and only validates the requested duration. `--run`
requires the fresh installed services, an unplugged battery reporting
`Discharging`, no online AC/USB supply, and a usable `energy_now` counter. It
temporarily replaces only the user's compositor service and restores it on
every exit; the system hardware service retains ownership of the panel.

Four samples run in ABBA order: static, animated, animated, static. Both cases
use the same full-width shader, fixed Touch Bar brightness, and fresh first
frame wake. The static client renders once and waits; the animated client uses
the compositor's battery-aware callback cadence. Each measured interval is
limited to 10–20 seconds and begins after a two-second handoff settle, keeping
it below the 30-second dim threshold. The retained TSV records energy-derived
watts, `power_now` when available, whole-system CPU busy percentage, and both
requested and actual backlight brightness. This is a controlled on-machine
comparison, not a claim that the short result generalizes to every workload.
