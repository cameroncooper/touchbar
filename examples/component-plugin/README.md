# Sandboxed component demo

This is a standalone Rust plugin pack for the
`touchbar:plugin/plugin@1.0.0` Component Model world. It exports two stable
items, receives the complete semantic theme and assigned viewport on every
render, returns a flat retained UI arena, and changes state after an activation
event.

The crate is excluded from the native workspace because it is built for
`wasm32-wasip2`:

```bash
rustup target add wasm32-wasip2
./scripts/build-component-demo.sh
./scripts/run-component-host.sh hello 160 1
./scripts/run-component-host.sh theme 100
```

The component uses ordinary Rust `std`. The host supplies default-deny WASI
runtime plumbing: closed standard streams, clocks, random, and polling, with no
arguments, environment, preopened filesystem paths, or allowed network
addresses. Plugin-specific capabilities will be separate WIT imports granted
from the package manifest in E3.
