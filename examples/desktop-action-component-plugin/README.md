# Desktop action component demo

This sandboxed Rust component demonstrates the two bounded desktop actions:
plain notifications and activation-gated URI opening. The manifest allows one
notification category and one HTTPS origin/path prefix. Both buttons use the
asynchronous broker API and rerender only after the completion arrives.

Run its deterministic, portal-free replay from the workspace root:

```sh
package_dir=$(./scripts/build-desktop-action-component-demo.sh)
cargo run --quiet -p touchbar-plugin-host -- "$package_dir" --replay \
  examples/desktop-action-component-plugin/tests/replay.json
```

The URI request receives authority only because the replayed tap produces a
trusted physical-origin activation. The replay broker uses a transport that
cannot open a portal, so neither a notification nor a browser is launched.
