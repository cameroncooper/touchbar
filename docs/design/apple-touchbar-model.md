# Apple Touch Bar model and implications

This note captures the AppKit concepts that should inform TouchBar's
public model before a plugin manifest format is designed. It intentionally
separates the enduring interaction model from macOS implementation details.

## What Apple exposes

An `NSTouchBar` is an invisible, ordered container of identified items. Apps
provide default item identifiers and may create the corresponding objects
lazily through a delegate. Stable identifiers are also the persistence keys
for user customization. A bar can declare the items shown by default, other
items a user may add, and items the user may not remove.

Sources:

- [NSTouchBar overview](https://developer.apple.com/documentation/appkit/nstouchbar)
- [Customization identifier](https://developer.apple.com/documentation/appkit/nstouchbar/customizationidentifier-swift.property)
- [Default item identifiers](https://developer.apple.com/documentation/appkit/nstouchbar/defaultitemidentifiers)

The visible bar is contextual. AppKit walks the focused responder chain,
discovers bars supplied by the application and frameworks, and composes them
according to system policy and available space. An `otherItemsProxy` marks
where content from a more-specific responder can be nested.

Sources:

- [Bar discovery, composition, and nesting](https://developer.apple.com/documentation/appkit/nstouchbar)
- [Other-items proxy](https://developer.apple.com/documentation/appkit/nstouchbaritem/identifier-swift.struct/otheritemsproxy)

Layout uses semantic hints rather than fixed zones. Items have visibility
priorities, the bar can nominate a principal item that the system attempts to
center, and fixed or flexible spacing participates in the ordered item list.
Groups can apply compression and customization to a set of nested items.

Sources:

- [NSTouchBarItem configuration](https://developer.apple.com/documentation/appkit/nstouchbaritem)
- [Visibility priority](https://developer.apple.com/documentation/appkit/nstouchbaritem/priority)
- [Spacing identifiers](https://developer.apple.com/documentation/appkit/nstouchbaritem/identifier-swift.struct)
- [Group items](https://developer.apple.com/documentation/appkit/nsgrouptouchbaritem)

The content model is not limited to buttons. Apple supplies pickers, sliders,
steppers, color pickers, candidate lists, sharing controls, and scrubbers.
Custom items can contain arbitrary views. Popovers provide a collapsed item,
an expanded replacement bar, and optionally a distinct press-and-hold bar.
Custom views can use animation, gesture recognizers, or raw touch events.

Sources:

- [Touch Bar API collection](https://developer.apple.com/documentation/appkit/touch-bar)
- [Popover items](https://developer.apple.com/documentation/appkit/nspopovertouchbaritem)
- [NSScrubber](https://developer.apple.com/documentation/appkit/nsscrubber)

The Control Strip is system-owned and normally remains available beside app
content, though the user can hide it. Apple describes the Touch Bar primarily
as an input device and discourages display-only content. Accessibility labels
are used both by the customization UI and assistive technology.

Source: [NSTouchBar overview](https://developer.apple.com/documentation/appkit/nstouchbar)

## Model we should adopt

```text
registry
  plugin package -> item factories + optional bar definitions

context engine
  focused app / window / workspace / mode -> eligible bars

bar composer
  nesting + overlays + user profile -> one ordered bar tree

layout resolver
  sizing + spacing + principal + visibility policy -> rectangles

surface compositor
  rectangles -> Wayland configure, rendering, clipping, and input
```

A plugin package does not own a permanent rectangle. It registers one or more
stable item factories and may offer reusable bar definitions. For example, a
media plugin could provide a compact play item, a timeline scrubber, a grouped
transport item, and a full-width expanded bar.

A user profile—not a plugin—selects defaults, ordering, required items, and
system-reserved content. Plugins may suggest preferred sizing and priority,
but they cannot make themselves unremovable or reserve a privileged region.

The layout resolver accepts an already composed bar tree. Its public inputs are
deliberately independent of TOML, JSON, process supervision, and Wayland:

- globally unique item identifiers;
- minimum, preferred, and maximum widths;
- compression and visibility priorities;
- atomic visibility groups;
- recursive natural and equal-width groups with explicit spacing;
- policy-supplied required item identifiers;
- fixed and weighted flexible spaces;
- an optional principal item.

Profiles and package presentation manifests serialize bounded group trees, but
the compositor still resolves them to leaf surface rectangles. Popovers,
context selection, and overlays remain policies above the resolver; a plugin
never defines the global runtime scene directly.

## Deliberate differences from AppKit

TouchBar should retain Apple's semantic organization while allowing
more capable rendering. Every item may use a lightweight software surface or
a DMA-BUF/GLES surface; the layout and input contracts remain identical.

The core may provide standard controls for consistency, but advanced plugins
can draw arbitrary animated 2D content. Rendering capability does not grant
layout authority, DRM access, global input access, or permission to synthesize
system actions.

“Required” is reserved for user or compositor safety policy. Full-width
interactive content is represented as a temporary scene or expanded bar with
an explicit return path. A separately assigned backdrop role may fill the
canvas beneath interactive items, but never participates in hit testing or
layout.
