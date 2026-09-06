# First-party pack vertical slices

Five sandboxed Component packs now exercise the fresh v1 package, UI, broker, install, and daemon
lifecycle. They are deliberately small vertical slices, not claims that every integration listed in
the product research is complete.

| Pack | Contributions | Platform behavior proved |
|---|---|---|
| System Controls | volume, brightness, microphone, battery, power profile | continuous sliders, glanceable progress, toggles, responsive collapse, command templates |
| Media | now playing, transport, timeline | MPRIS D-Bus calls, compact metadata, transport, scrubbing |
| Hyprland Navigator | workspaces, window actions, move window | context disclosure, workspace scrubbers, typed `hyprctl` templates |
| Capture Studio | screenshots, recording, capture tools | multi-action palettes, destructive recording state, long-running commands |
| Command Deck | desktop menu, launchers, session, custom actions | fixed safe actions, hold confirmation, user-owned local action service |

Every color in these packs is a semantic theme role. Text contrast, fills, progress tracks,
pressable backgrounds, destructive state, and accent state are re-resolved by the host after every
theme snapshot. No pack caches raw host-theme colors.

Every pack passes `touchbarctl plugin check` and headless host rendering for every contribution at 80,
160, 320, 1004, and 2008 pixels plus declared presentation widths. Build all package artifacts with:

```text
./scripts/build-first-party-packs.sh
```

Run one on hardware with:

```text
./scripts/run-first-party-physical.sh controls 20
```

The present slices default-degrade when optional grants are absent, so rendering and touch state can
be tested safely. Permission review and session/persistent consent, component-requested
multi-item presentations, deterministic interaction/D-Bus/HTTP/command/local-IPC/desktop-action/clipboard/secret replay, and an onscreen
production-compositor simulator are now available. Product work still includes live system-state
adapters for every glanceable item (rather than initial demo state for some battery/metadata views)
and broader pack-level fixtures.
Command Deck custom actions already use
the intended boundary: the component connects only to a user-approved logical endpoint speaking
`io.github.cameroncooper.touchbar.command-deck.v1`; the user-owned service remains free to implement arbitrary
actions without granting an interpreter to the sandbox.
