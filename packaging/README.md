# Packaging and service activation

The installed layout is intentionally conventional:

```text
/usr/lib/touchbar/touchbard
/usr/lib/touchbar/touchbar-sessiond
/usr/bin/touchbarctl
/usr/lib/touchbar/touchbar-plugin-host
/usr/lib/touchbar/touchbar-plugin-supervisor
/usr/lib/touchbar/touchbar-secret-helper
/usr/lib/touchbar/service-admin-root
/usr/lib/systemd/system/touchbar.service
/usr/lib/systemd/user/touchbar-session.service
/usr/lib/udev/rules.d/99-touchbar.rules
```

The hardware service has a runtime dependency on libinput and udev. Its
libinput callback accepts only exact `/dev/input/eventN` paths, rejects write
access, and opens with `O_NOFOLLOW`. The systemd unit independently confines
it to read-only `char-input` devices and the Unix/netlink address families
needed by libinput device discovery. Seat event payloads are discarded inside
`touchbard`; only an activity boolean reaches the backlight policy.

Package installation alone does not take over the Touch Bar. After the guarded
physical handoff test passes, activate the installed services from the
graphical user account:

```bash
./scripts/test-packaging.sh
./scripts/install-development-build.sh
touchbar-activate
```

The packaging test builds the exact release artifacts, validates temporary
copies of both units with `systemd-analyze`, verifies the udev rules, and checks
that every installed executable maps to the expected build artifact. It does
not write to `/usr`, change service state, or stop tiny-dfr.

Activation creates `/etc/touchbar/enabled` and masks
`tiny-dfr.service` because tiny-dfr's package also has udev rules that start it
directly; merely disabling the unit cannot establish one deterministic DRM
owner. Package installation without that root-owned marker is inert. The
tiny-dfr package and configuration remain installed.

The user compositor is enabled under `graphical-session.target`, not the
longer-lived user `default.target`; logout tears it down and a subsequent
graphical login starts it again without relying on compatibility aliases or a
desktop autostart wrapper.

The recovery command is:

```bash
touchbar-rollback
```

It stops the user compositor and hardware service, unmasks and starts tiny-dfr,
and deliberately leaves the TouchBar package installed for diagnosis
or another activation attempt. The rollback helper does not require the
`touchbard` binary or unit to remain intact; recovery depends only on the
already-installed tiny-dfr unit. It fails closed if systemd cannot prove that
`touchbard` stopped, so the two DRM owners are never intentionally started
together.

Validate an active installation without changing service state:

```bash
./scripts/test-installed-lifecycle.sh
```

`touchbarctl hardware status` reports both service readiness and whether the
hardware-owned recovery fallback is latched. Holding physical Fn for eight
seconds enters that mode without relying on a visible Touch Bar control;
release and repeat the hold to resume authenticated user sessions.

After installing the current build, exercise that complete disconnect/latch/
reconnect sequence interactively with:

```bash
./scripts/test-installed-recovery.sh
```

The script only observes the already-active services and waits for the two
physical Fn holds; it does not stop a Touch Bar manager or request privileges.

The suspend/resume acceptance is deliberately explicit because it suspends the
machine. Save work first, then run it from the graphical session:

```bash
./scripts/test-installed-lifecycle.sh --suspend
```

After resume it waits for both services and their control sockets, prints the
relevant system and user journals, and fails if the hardware/session boundary
did not recover. `touchbar-rollback` remains the recovery command.
