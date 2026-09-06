# Media widget showcase

The first complex reference plugin is a standalone, unrestricted process. It
uses simulated playback data so UI, layout, GPU, and interaction behavior can
be validated deterministically before adding an MPRIS adapter.

## Responsive compact surface

The same stable `media.now-playing` item supports three compositor-assigned
widths:

- 80 pixels selects a theme-tinted symbolic SVG and progress line;
- 160 pixels selects artwork, title/artist/progress, and play/pause;
- 420 pixels selects the richer form with larger artwork and metadata.

Artwork is generated once per simulated track and uses one stable image ID
with a changing revision. The renderer therefore replaces the texture only
when the track changes rather than uploading it every frame. Optional artwork
can disappear before required text and transport controls when space becomes
tight.

The outer media surface is a composable pressable. Tapping artwork or metadata
requests a persistent anchored presentation; holding requests a transient
presentation. The nested play/pause control has a later hit target and wins
inside its own bounds without changing the outer item's identity.

## Expanded interaction

The 420-pixel anchored presentation contains artwork, marquee track metadata,
a captured timeline slider with stable-width time labels, an animated waveform,
and previous/play/next
transport controls. The two supported flows are:

```text
tap compact -> persistent presentation -> tap or drag timeline -> outside tap
hold compact -> transient presentation -> capture transfers to timeline -> release
```

The compositor continues to own anchor placement, surface resize, contact
capture, outside dismissal, and compact restoration. Playback and presentation
page state stay in the plugin process.

## Theme and GPU behavior

Every built-in paint uses a semantic theme role. The accent play/pause control
uses `OnAccent`, which derives black or white from live accent luminance.
Ordinary surfaces use the theme's control/foreground pair, while metadata,
progress, and sliders use muted, track, and accent roles.

The waveform now uses the reusable `TinyGraph` node, and the timeline uses a
tick-marked `StyledSlider`; both resolve semantic colors from the current
theme. Simulated artwork remains full color. The minimal symbol comes from the
built-in SVG catalog and uses alpha-mask coloring, so it follows the live
foreground color without reparsing the asset. Track changes use render-time
translation, scale, and opacity while layout and hit targets stay stable.

The showcase requests continuous frames for progress, marquee, waveform, and
transition animation. A production media adapter should request continuous
frames only while one of those effects is visible, and otherwise sleep until
MPRIS state, input, appearance, visibility, or geometry changes.

## Demonstration

Run a hardware-GPU headless acceptance at any reference width:

```bash
./scripts/run-media-ui.sh 180 80 tap
./scripts/run-media-ui.sh 180 160 tap
./scripts/run-media-ui.sh 180 420 hold
```

Run it on the physical Touch Bar:

```bash
./scripts/run-media-ui-physical.sh 15 160
```

The physical runner uses real Z2 input and restores `tiny-dfr` when it exits.
The simulated source changes track after eight seconds so artwork replacement
can be observed without an external player.

## Boundary to the production plugin

The eventual MPRIS integration should replace only `Playback` and its fake
clock. It remains an ordinary user process using the session D-Bus directly.
No player command, source-selection API, or media-specific state belongs in
`touchbar-sessiond`.
