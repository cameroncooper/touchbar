# Onscreen compositor simulator

The developer preview is an output of `touchbar-sessiond`, not a second UI
implementation. It therefore displays the same final 2008×60 scene used by the
physical Touch Bar: installed package contributions, responsive profile layout,
backdrops, semantic theme changes, alpha composition, animations, and every
anchored, in-place, slot, region, or full-bar presentation all pass through the
production compositor first.

The normal package workflow opens the complete package in an isolated store:

```text
touchbarctl plugin build
touchbarctl plugin dev
```

The preview defaults to every contribution in the pack. Isolate one item or
override its requested compact width when useful:

```text
touchbarctl plugin dev --item volume --width 240
touchbarctl plugin dev --scale 1
```

`--scale 2` is the default and presents the 2008×60 buffer as a 1004×30
high-density Wayland surface. Scales 1, 2, and 4 are supported. Press Escape or
close the window to end the complete temporary runtime. Pressing `F` simulates
holding Fn when previewing the system row.

## Rendering path

Plugins render through their normal EGL/GLES DMA-BUF path. The session
compositor imports those buffers, resolves production geometry, and performs
the final alpha blend on the GPU. For the desktop-only output it reads the
completed scene back, converts OpenGL bottom-up RGBA into Wayland top-down
ARGB8888, and presents through three `wl_shm` buffers paced by `wl_surface`
frame callbacks. A busy buffer is never rewritten; if the desktop compositor
falls behind, only the newest pending frame is retained.

The readback is specific to the simulator. Physical ADP output remains a
GPU-to-DMA-BUF path. A future desktop DMA-BUF presentation path could remove
the preview readback, but it is not needed to validate plugin behavior or
physical performance.

## Input and authority

Mouse presses, pointer drags, and native Wayland touch contacts map from the
surface's display scale back into exact Touch Bar coordinates and enter the
same capture, hit-testing, presentation, and profile-transaction router as
hardware contacts. Preview touch IDs occupy a separate namespace.

The v1 surface protocol labels every contact as `physical` or `synthetic`.
`touchbar-plugin-host` preserves that origin when it constructs a broker
activation. Synthetic contacts can exercise all local UI behavior, but the
broker rejects them for activation-gated D-Bus, command, clipboard, secret,
portal, and similar operations. The disposable preview store begins with no
grants and is deleted on exit. The simulator can never be combined with the
physical hardware-output flags.

## Low-level compositor mode

For compositor or system-row work, the output can be enabled directly:

```text
touchbar-sessiond --socket touchbar-preview --preview --no-plugins --system-bar
```

`--preview-scale 1|2|4` selects an explicit scale. This mode can also run the
normal installed-plugin manager and user profile by omitting `--no-plugins`.
