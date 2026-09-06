# Clipboard component demo

This sandboxed component demonstrates separately granted clipboard read and
write operations. Both are limited to one exact MIME type, bounded bytes, a
user-selected compositor binding, a rolling operation rate, and a fresh
physical activation.

Run the deterministic replay from the workspace root:

```sh
package_dir=$(./scripts/build-clipboard-component-demo.sh)
cargo run --quiet -p touchbar-plugin-host -- "$package_dir" --replay \
  examples/clipboard-component-plugin/tests/replay.json
```

Replay uses a non-connectable synthetic clipboard binding and never contacts
Wayland. Fake clipboard bytes stay inside the private broker/guest exchange and
the report exposes only the rendered byte count.
