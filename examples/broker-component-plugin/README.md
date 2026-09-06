# Broker component demo

This is the smallest event-driven Rust component using
`touchbar-component-sdk`. The status button opens an exact MPRIS
`PropertiesChanged` subscription and reads the initial `PlaybackStatus`; the
other submits a typed `dbus.call.v1` request for `PlayPause`. Requests return
immediately and `handle_host_event` receives both completions and ordered
resource events. The manifest grants only MPRIS player properties,
`PropertiesChanged`, and Play/Pause; it disables service activation and
requires a real Touch Bar activation for the action.

Build it for the Component Model target:

```sh
cargo build --release --target wasm32-wasip2 \
  -p touchbar-broker-component-demo
```

Copy the resulting Wasm file to `component/plugin.wasm` beside the manifest.
The supervisor reconstructs the D-Bus message after exact scope matching. Run
it through the physical host path so the Play/Pause call carries trusted touch
activation; synthetic headless activation is intentionally rejected.

From the workspace root, the complete isolated physical demo is:

```sh
./scripts/run-broker-ui-physical.sh 20 320
```

It does not contact media players on the desktop session bus. The runner starts
a private bus and deterministic fake `playerctld`, writes exact digest-bound
development grants, and launches the component through the production
supervisor. Tap `STATUS` to open the live subscription, then tap `PLAY/PAUSE`
to test the physical-activation-gated method.
