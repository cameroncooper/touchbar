# Secret component demo

This sandboxed component requests one manifest-local logical secret after a
fresh physical tap. The user—not the package—maps that name to an exact Secret
Service item when granting permission.

Run its isolated replay from the workspace root:

```sh
package_dir=$(./scripts/build-secret-component-demo.sh)
cargo run --quiet -p touchbar-plugin-host -- "$package_dir" --replay \
  examples/secret-component-plugin/tests/replay.json
```

Replay supplies conspicuously fake bytes through the production broker ABI. It
does not create a D-Bus connection or contact Secret Service, and its JSON
report contains only the fixture ID, outcome, and guest-rendered byte count—not
the value. Never put real credentials in a replay file.
