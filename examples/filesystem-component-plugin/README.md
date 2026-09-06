# Filesystem component demo

This sandboxed component has no ambient filesystem access. Its manifest asks
for a logical `gallery` mount; the demo grant tool binds that label to a
directory selected by the user. Tapping the item lists the mount and streams
the first regular file through bounded broker resource events, then displays
its name and total size. File bytes are counted but never rendered.

From the workspace root:

```sh
./scripts/run-filesystem-ui-physical.sh 20 320 "$HOME/Pictures"
```

For a deterministic test that never reads the host filesystem:

```sh
./scripts/build-filesystem-component-demo.sh
target/debug/touchbar-plugin-host target/filesystem-component-demo \
  --replay examples/filesystem-component-plugin/tests/replay.json
```

The supervisor resolves every relative path beneath the approved root with
Linux `openat2`. Absolute paths, parent traversal, symlinks, magic links, mount
crossings, and special files are not available to the component.
Files with multiple hard links are rejected so a link placed inside an approved
directory cannot expose an inode that also has a name outside it.
