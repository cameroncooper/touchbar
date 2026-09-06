# Touch Bar framework research

This note records the September 2026 review of Apple's public Touch Bar APIs,
Apple's first-party uses, and the established customization ecosystem. Its
purpose is to define the durable capabilities TouchBar should expose
before package manifests make those decisions expensive to change.

## Conclusions

The strongest Touch Bar interactions are direct continuous manipulation,
quick context-relevant actions, horizontal collection selection, and compact
controls that expand temporarily. Persistent controls are valuable when the
user chooses them. Passive status and decorative animation are also popular in
community tools, but should remain optional, actionable where possible, and
power-aware.

`touchbar-sessiond` should remain a small compositor and policy engine. A plugin owns
its local UI and behavior, while the user and compositor own placement,
visibility, composition, capture, and presentation. The plugin SDK should
offer two equal rendering paths:

```text
semantic retained UI                 raw GPU surface
--------------------                 ---------------
standard controls and layout         arbitrary GLES and shaders
theme and accessibility metadata     complete visual ownership
built-in gesture recognition         raw captured contacts
demand-driven scheduling             explicit animation cadence
```

Raw GPU access must remain available even when a plugin uses UI-kit controls
in the same frame.

## Apple's public model

An [`NSTouchBar`](https://developer.apple.com/documentation/appkit/nstouchbar)
is an ordered container of stable, identified items rather than an
application-owned bitmap. AppKit discovers eligible bars through the focused
responder chain and can insert a more-specific bar into an outer bar at an
`otherItemsProxy`. Bars can therefore contribute context at several levels
without replacing every persistent control.

The [Touch Bar API
collection](https://developer.apple.com/documentation/appkit/touch-bar)
includes buttons, sliders, steppers, segmented pickers, color pickers,
candidate lists, sharing controls, groups, popovers, and arbitrary custom
views. A custom view can draw and animate like another Retina view and can use
gesture recognizers or direct multitouch input.

Layout is semantic and responsive:

- fixed and flexible spacing;
- a principal item or group the system attempts to center;
- per-item visibility priorities;
- equal-width and preferred-width groups;
- ordered compression strategies such as hiding text or imagery;
- nested groups, popovers, scrubbers, and finally scrolling for overflow.

Apple intentionally provides no API for querying the total available bar
width, because the Control Strip, system policy, and nested bars can change it.
This suggests discrete responsive representations in addition to continuous
minimum, preferred, and maximum widths:

```text
full       icon + label + value
compact    icon + value
minimal    icon
hidden
```

Customization also depends on stable identifiers. A bar declares default,
allowed, and required items; the user can add, remove, and reorder them in an
onscreen editor. Logical group boundaries remain meaningful during
customization even when they have no visible border.

### Popovers and held interactions

An
[`NSPopoverTouchBarItem`](https://developer.apple.com/documentation/appkit/nspopovertouchbaritem)
has a compact representation, a bar opened by tapping, and optionally a
different bar shown during press-and-hold. Apple recommends held presentation
only for simple direct interactions such as a slider or segmented picker and
warns against using it for complex or scroll-heavy content.

The generalized compositor presentations should therefore be:

- anchored overlay;
- in-place expansion;
- replacement of the owning slot or content region;
- explicit full-bar replacement;
- a hold presentation tied to one captured contact;
- release, selection, outside-touch, toggle, or application-controlled
  dismissal.

Every transient presentation needs an explicit return path and must preserve
the previous composition.

### Scrubbers

[`NSScrubber`](https://developer.apple.com/documentation/appkit/nsscrubber) is
a Touch-Bar-native virtualized horizontal collection. It provides reusable
text, image, or custom cells; continuous and discrete selection; highlight
and selection state; snapping and alignment; flow, proportional, and custom
layouts; visible-range callbacks; and begin, finish, and cancellation events.

A scrubber is a foundational control rather than a media-specific widget. It
supports timelines, history, tabs, workspaces, applications, emoji, palettes,
thumbnails, and parameter presets.

### Input, performance, and accessibility

Apple exposes standard controls, gesture recognizers, and raw `NSTouch`
snapshots with stable identity and cancellation. Because the device is a thin
strip, the horizontal component is the meaningful geometric axis. Our raw
contact protocol is therefore the right base; the UI kit should layer a
cancellation-safe gesture arena over it without removing raw access.

Apple says the main display and Touch Bar share CPU and GPU resources, gives
no frame-rate guarantee, and recommends tuning on physical hardware rather
than the simulator. Foreground animation should be interaction-related and
interruptible. The compositor should separately support user-enabled
animated backdrops with frame-rate, dimming, and battery controls.

Standard AppKit views expose accessibility automatically, but pixels from a
custom renderer do not describe themselves. Each semantic UI node must carry
its role, label, value, state, actions, and bounds so a later AT-SPI bridge,
onscreen mirror, and inspector do not require changing plugin UI APIs.

Apple repeatedly describes the bar as an input device, discourages alerts and
display-only widgets, and says essential functionality must remain available
elsewhere. TouchBar can intentionally allow richer status content, but it
should not make hidden gestures or the Touch Bar the sole route to an action.

## Evidence from useful applications

Apple's [Final Cut Pro Touch Bar
guide](https://support.apple.com/en-euro/guide/final-cut-pro/verb85d859e9/mac)
shows selection- and workspace-sensitive tools, direct timeline navigation,
range manipulation, text formatting, and audio controls. The important
context is often the selected object or active tool, not merely the focused
application.

[Logic Pro](https://support.apple.com/en-euro/guide/logicpro/lgcp94a1c475/mac)
uses transport controls, selected-track and effect parameters, sliders,
nested screens, musical keyboards, drum pads, and press-and-hold button to
slider transitions. It also lets users define command screens and modifier
variants.

[Photoshop](https://helpx.adobe.com/photoshop/using/touchbar.html) uses modes,
history scrubbing, brush parameters, color selection, layer opacity, and
contextual confirmation controls.

Community systems broaden the desired uses:

- [MTMR](https://github.com/Toxblh/MTMR) demonstrates scriptable buttons,
  groups, sliders, system status, media, calendars, networking, and
  application visibility rules. Its documented technical debt warns against
  enum-closed widget registries, coupled construction/composition, in-process
  scripts, and hard-coded left/center/right layout.
- [Pock/PockKit](https://github.com/pock/pock) demonstrates installable widgets,
  arbitrary custom views, lifecycle hooks, preferences, user reordering, and
  stack navigation. Its in-process widgets provide visual freedom but weaker
  crash isolation than standalone TouchBar plugin processes.
- [BetterTouchTool](https://docs.folivora.ai/docs/touch-bar/widgets/) supports
  host-styled text, native custom views, scripts, variables, named actions,
  conditional activation, multitouch gestures, and global plus app-specific
  configurations.
- [GoldenChaos-BTT](https://github.com/GoldenChaos/GoldenChaos-BTT) combines a
  persistent home strip, app-specific controls, persistent information,
  modifier layers, nested groups, and presentations that replace only part of
  the bar.

These systems show demand for system controls, media, clocks, calendars,
weather, CPU/network graphs, workspaces, app launchers, browser/editor actions,
scripts, album art, visualizers, games, and state-driven animation. Those are
example plugins, not compositor policy.

## Required framework primitives

### Composition and layout

- Stable namespaced item and widget identities.
- User-owned profiles and arbitrary slots.
- Persistent, collected, and context-selected contributions.
- Nested groups and contextual insertion points.
- Intrinsic minimum/preferred/maximum sizes plus full, compact, minimal, and
  hidden representations.
- Flex grow/shrink, fixed and flexible space, equal-width groups, alignment,
  visibility priority, and semantic centering.
- Text measurement, ellipsis, localization, and explicit layout direction.

### UI and rendering

- A retained scene tree whose visual nodes, hit testing, and semantic tree use
  the same resolved geometry.
- Text, template icons, images, rounded geometry, paths, gradients, clipping,
  opacity, transforms, masks, and z-order.
- Buttons, toggles, segmented controls, sliders, range sliders, steppers,
  progress, groups, labels, images, and virtualized scrubbers.
- Theme-aware standard widgets and unrestricted GLES in the same plugin.

### Input and presentation

- Down, move, up, and cancel contacts with stable capture.
- Tap, multi-tap, hold, repeat, horizontal pan, scrub, swipe, and multitouch
  recognizers, plus raw-contact access.
- Anchored, in-place, slot, region, and bar-level transient presentations.
- Modifier-key and touch-count context.

### Lifecycle and scheduling

- Render only when dirty for static content.
- Animation cadence or next-frame-deadline requests.
- Visibility suspension and resume invalidation.
- Damage tracking, frame callbacks, and compositor-side rate/resource
  diagnostics.
- Push-based application facts and plugin state rather than mandatory polling.

### Accessibility and customization

- Semantic role, label, value, hint, state, actions, and resolved bounds.
- Adjustable increment/decrement actions and visible focus state.
- An onscreen mirror/inspector for discovery, customization, debugging, and
  future zoom/accessibility support.
- Reorder, pin, hide, reset, profile variants, and user-owned required items.

## Engineering sequence

1. UI Kit Foundation: retained semantic scene, responsive variants,
   flex/group layout, core rendering resources, common controls, dirty-frame
   scheduling, and an inspector snapshot.
2. Collection and Gesture Foundation: virtualized scrubber and composable
   recognizers.
3. Presentation Generalization: anchored/in-place/slot/region/bar policy and
   nested navigation.
4. Control Plane: context facts, variables, actions, temporary modes,
   inspection, health, and frame telemetry.
5. Customization and Configuration: persistent profiles, settings schemas,
   onscreen editor, and package metadata.

This sequence keeps the compositor surface protocol stable while the
plugin-side authoring experience becomes substantially more capable.
