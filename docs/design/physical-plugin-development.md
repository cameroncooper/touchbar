# Physical plugin development

`touchbarctl plugin run` is the physical counterpart of `touchbarctl plugin
dev`. Both use the package working tree, an isolated plugin store, and a newly
launched workspace `touchbar-sessiond`. The output differs: `dev` opens a
Wayland preview window, while `run` borrows the physical hardware connection.

The privileged `touchbard` process never stops and never loads development
code. Before launching the workspace compositor, the CLI opens the installed
session's same-user control socket and requests a hardware-yield lease. The
installed session cancels captured contacts, releases generated system keys,
drops its hardware socket, and suppresses automatic reconnect. The lease lasts
exactly as long as that control connection.

The workspace session then attaches to `/run/touchbar/hardware.sock` through
the normal authenticated protocol. `touchbard` retains DRM master, input,
backlight, the DMA-BUF handoff, and its trusted fallback throughout. When the
developer presses Ctrl-C, either process fails, or the terminal disappears,
the workspace session disconnects and the lease socket closes. The installed
session immediately resumes its normal reconnect loop. The CLI arms the
workspace session with a parent-death signal, and the session handles that
signal as a graceful shutdown so its Wayland socket and lock do not remain
behind after an abrupt developer-terminal failure.

The CLI reports `plugin-run=ready` only after the development session is
hardware-connected, a plugin process is running, and its first frame is retained
as visible user content. A process that starts and then fails during its first
render therefore cannot produce a false successful preview.
The command also prints the exact session daemon, component host, and supervisor
paths selected for the run. An installed release CLI prefers a complete
workspace release set over possibly stale debug artifacts; explicit path flags
remain available when an author intentionally wants a different build.

```text
touchbarctl plugin run
        │ lease                    ordinary authenticated session
        ├────────▶ installed sessiond ─ ─ ┐
        │                                  │ yields
        └─▶ workspace sessiond ───────────▶ touchbard ─▶ hardware
```

The default is trusted local development. Component bytes are still executed
by `touchbar-plugin-host`, because that is their runtime and GPU UI bridge, but
the production supervisor is not inserted and broker capabilities report as
unavailable. This is the shortest faithful path for layout, theme, animation,
context, presentation, and physical-input work. Native packages remain ordinary
developer-owned processes.

`--sandboxed` selects the production supervisor and broker path against the
disposable store. Use it for permission, grant/revocation, broker-event,
confinement, and production crash testing. It deliberately does not inherit the
user's persistent installed-plugin state or grants.

`--when-application CLASS` creates a disposable two-profile configuration and
requires `--item`. The item is hidden until Hyprland reports that class as the
focused application. Merely creating an unfocused window does not satisfy the
condition.

The trusted system overrides remain available during a physical plugin run.
Hold Fn for F1–F12. To reveal the media row instead, tap Fn and then press and
hold it again within 400 ms; releasing Fn restores the development plugin.
