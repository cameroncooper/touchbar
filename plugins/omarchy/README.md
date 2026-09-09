# Omarchy

The Touch Bar as part of an Omarchy desktop: the pixel field from the Omarchy
homepage re-composed for a 2008x60 ribbon, plus a palette strip that proves it
is wearing the current theme.

![The animated pixelated Omarchy wordmark in its themed field](../../docs/images/packs/omarchy/screensaver-2008-dark.gif)

The drawing component remains isolated: three validated effects, one retained
canvas, and host-timed motion. A separate sandboxed worker requests the
revocable `appearance.provide.v1` capability. Its scope binds the Omarchy
runtime, current-state, user-theme, and system-theme roots plus one
package-local provider ID and strict file-size/update-rate limits. It grants
the renderer no filesystem, network, D-Bus, command, or write access.

## Items

| Item | What it is |
|---|---|
| `screensaver` | A sparse pixel field with the homepage's five-band wordmark, a light sweeping the long axis, expanding centre ripples, and a wake that hands the strip back |
| `palette` | Every resolved role side by side, so a theme change is either visibly right or visibly wrong |

## Try it

```bash
touchbarctl plugin build --package plugins/omarchy
touchbarctl plugin dev   --package plugins/omarchy --item screensaver
```

Tap the strip in the simulator to watch the wake. Tap again to go back.

For a disposable full-width preview on the real Touch Bar:

```bash
touchbarctl plugin run \
  --package plugins/omarchy --item screensaver --width 2008
```

To test the real focus-driven handoff, bind the disposable preview profile to
Omarchy's screensaver class, then activate and focus the screensaver while the
runner is active:

```bash
touchbarctl plugin run \
  --package plugins/omarchy --item screensaver --width 2008 \
  --when-application org.omarchy.screensaver

omarchy-launch-screensaver force
```

For the installed behavior, use the normal one-step installer:

```bash
touchbarctl plugin install --path plugins/omarchy
```

This is safe to run if an earlier build was installed with `plugin add`; the
installer adopts that package and completes any missing consent or activation.
The plugin supplies `xdg-state:omarchy/current` as its logical mount default.
The host resolves that name, displays the exact directory and requested
`appearance.provide.v1` access, and installs, grants, and enables only after one
confirmation. The plugin never chooses the authoritative path itself. An
unusual layout can override the default with
`--bind omarchy-current=/absolute/path`. Revoking the grant later stops the
provider without disabling the screensaver renderer.

The manifest-provided `screensaver` profile then appears automatically while
`org.omarchy.screensaver` is focused or Omarchy's
`omarchy-image-selector` layer is open. The item is deliberately excluded from
the normal fallback bar. Any completed TouchBar theme change also holds this
profile for two seconds, long enough to show the newly resolved palette after
the picker closes. Disable only this behavior with
`touchbarctl plugin profile github:cameroncooper/touchbar-omarchy screensaver disable`.

The earlier `plugin run` command builds and validates the local package,
installs it only into an isolated temporary store, asks the installed user
session to yield its hardware connection, and launches the workspace session
compositor. The privileged hardware service remains running throughout. Ctrl-C
restores the installed user session automatically. Add `--sandboxed` when
specifically testing production permissions and broker behavior.

`plugin replay --scenario tests/replay.json --screenshots DIR` renders the whole
sequence — rest, a light and ripple crossing the field, a theme change
mid-animation, a light theme over the black canvas, reduced motion, and both
wake frames — without hardware.
Run `scripts/generate-omarchy-gif.sh` from the repository root to regenerate
the animated README preview from one complete deterministic effect period.

## Designed for this strip, not a phone

The Touch Bar is 2008x60: a 33:1 ribbon that is mostly negative space. At full
height, the site's 81x19 wordmark becomes a crisp 3px grid, 243px wide. Its
roaming light becomes a minute-long end-to-end sweep, while a circular pulse
reads on the shallow panel as two fronts moving away from centre. The panel is
also OLED, so the mark breathes and drifts slowly around centre instead of
holding one intensity and position forever.

The screensaver's full-width canvas is always opaque black, matching the
keyboard and Touch Bar bezel. Theme changes affect the pixel field and wordmark
rather than turning the strip into a colored rectangle; accent and foreground
colors are lifted only when needed for contrast. This is a choice made by the
Omarchy plugin, not a renderer restriction. The published palette and the
`palette` diagnostic item remain exact.

Everything is paced for ~30 Hz. Physical presentation runs at 29.9 Hz and the
animation cadence drops to 30 Hz on battery. Under `motion = "reduced"` the same
composition holds still: the host freezes the effect's clock, and the component
drops its motion nodes rather than leaving them frozen mid-drift.

## One shared pixel grid

The field uses three small validated effects—quiet pixels, the roaming light,
and the expanding ripple—all quantized onto one logical grid aligned to the
wordmark. Splitting those passes keeps each program inside the host's strict
straight-line shader budget. Cell energy is snapped to four theme-derived tones
rather than rendered as a smooth gradient.

The exact 81x19 wordmark bitmap is drawn as contiguous canvas runs. Its upper
rows combine `accent` and `foreground` into crest and hover bands; its lower
rows fade `accent` over the opaque background into mid and dim bands. This
retains the homepage silhouette and stepped vertical color treatment without a
texture, browser, network access, or literal theme colors.

## Wearing the Omarchy theme

The package publishes an `omarchy` appearance-provider contribution for exact
`DESKTOP_SESSION=omarchy` matches. It runs a separate sandboxed WASM worker,
independent of the visual screensaver component. The worker watches
`$XDG_RUNTIME_DIR/omarchy-theme-selection`; while the picker is open it resolves
the selected preview name against the approved user and system theme roots. At
all other times it reads the committed `current/theme/colors.toml` palette.

All four roots are installer-owned bindings under `appearance.provide.v1`.
Reads are bounded and resolved beneath those roots without parent traversal,
symlink traversal, magic links, or mount crossing. The worker parses
Omarchy-specific TOML and publishes a typed scheme plus six semantic colors.
`touchbar-sessiond` knows no Omarchy paths or file format: it validates provider
identity, arbitrates providers, assigns the generation, derives control states,
and broadcasts the atomic snapshot. A missing or malformed transient palette
leaves the last valid publication live.

An explicit `TOUCHBAR_THEME` or existing `~/.config/touchbar/theme.toml` remains
a higher-priority user override. No symlink, generated copy, or theme hook is
needed.

This is also what gives the field its identity. Black anchors the field's dim
pixels, while contrast-safe `accent` and `foreground` supply the lit, hover,
and crest tones without the package knowing the theme's name.

## What is still a prototype

- **The image-picker namespace is shared.** Current Omarchy uses
  `omarchy-image-selector` for both theme and background selection, so both
  activate the profile. A purpose-specific namespace in Omarchy can be added to
  the manifest when available. The simulator's press-to-wake remains a local
  preview affordance; installed handoff is context-driven.
- **Backlight.** `touchbard` drops to its idle level on seat inactivity, so
  today this plays dim. Dim is arguably correct and power-honest, but it should
  be a decision rather than a discovery.
- **The wordmark.** Its bitmap follows the public Omarchy homepage wordmark.
  Shipping that branding in a third-party package remains a trademark question;
  reading the user's own `~/.config/omarchy/branding/` through a path-scoped
  `filesystem.read` grant would make the desktop and strip share one source.
