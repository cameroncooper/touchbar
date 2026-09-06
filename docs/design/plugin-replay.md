# Deterministic plugin replay

`touchbarctl plugin replay --scenario FILE` runs one sandboxed component item
without Wayland or Touch Bar hardware. Coordinates pass through the production
UI kit's hit testing, contact capture, slider mapping, and hold recognizers. The
clock advances only when the scenario says it does, so the same package and
scenario produce the same guest events and semantic snapshots.

The host emits one versioned JSON report. It contains every generated UI event,
presentation command, guest render count, resolved responsive representation,
semantic tree, primitive-kind count, and effective palette for each snapshot.
The initial scene and every step are captured; a `snapshot` step assigns a
stable author-selected name suitable for assertions.

Pass `--screenshots DIRECTORY` to rasterize each explicitly named `snapshot`
step as `NAME.png`. The host creates a surfaceless GLES3 pbuffer, renders through
the production native UI renderer, reads back RGBA pixels, corrects GL's vertical
orientation, and writes a 60-pixel-high PNG. The report records the actual GPU
renderer and filename. Output filenames come only from validated snapshot names,
and files use create-new semantics so existing goldens are never overwritten.
At most 64 raster snapshots may be requested in one scenario; automatic per-step
semantic snapshots remain JSON-only.

## Scenario v1

```json
{
  "version": 1,
  "item": "main",
  "width": 160,
  "appearance": {
    "preset": "dark",
    "motion": "full",
    "colors": {}
  },
  "context": {
    "application.id": "terminal"
  },
  "steps": [
    { "kind": "touch", "contact_id": 1, "phase": "down", "x": 80, "y": 30, "time_ms": 0 },
    { "kind": "touch", "contact_id": 1, "phase": "up", "x": 80, "y": 30, "time_ms": 50 },
    { "kind": "presentation", "event": { "kind": "started" } },
    { "kind": "context", "facts": { "application.id": "firefox" } },
    { "kind": "appearance", "preset": "light", "revision": 2, "colors": { "accent": "#246bfe" } },
    { "kind": "snapshot", "name": "after-theme" }
  ]
}
```

The top-level width is `1..2008`; height is always 60. A scenario has at most
1024 steps and a 24-hour monotonic timeline. Coordinates are finite and bounded.
Unknown fields, enum values, versions, non-monotonic time, duplicate snapshot
names, malformed colors, and oversized files fail closed.

Available steps are:

- `touch`: `down`, `motion`, `up`, or `cancel` for one contact ID and coordinate;
- `advance`: move the clock without motion, allowing stationary long presses to fire;
- `appearance`: switch the `dark`/`light` preset, motion policy, revision, and any
  semantic palette role;
- `presentation`: inject `anchor`, `started`, or any compositor end reason into
  the guest lifecycle callback;
- `context`: atomically publish new values to active context subscriptions;
- `dbus-signal`: publish one typed `PropertiesChanged` event to an already
  opened named D-Bus subscription fixture;
- `snapshot`: name the scene at the current time.

Colors use `#RRGGBB` or `#RRGGBBAA`. Palette roles are `background`, `control`,
`control_pressed`, `accent`, `track`, `foreground`, `muted`, and `destructive`;
`corner_radius` is also configurable. An appearance step preserves unspecified
roles, which makes targeted theme changes easy to express.

The optional top-level `context` object initializes fake `application.id` and/or
`workspace.id` facts. It is available only when the package declares
`context.read.v1`, and every fixture key must be inside the manifest's exact fact
scope. The host connects the component to a private sequenced-packet broker,
exercising the production async request, completion, subscription, and resource-
event path. A `context` step increments the fake generation and updates every
matching subscription before the next snapshot.

Replay intentionally supplies no ambient filesystem, network, D-Bus, command,
clipboard, secret, or desktop authority. Context and D-Bus fixtures run through
the production private sequenced-packet ABI; HTTP and command fixtures use their
production request decoders and scope authorizers. None contact an OS service or
launch a process. Requests for every capability without an explicit fixture
remain denied.

## Typed D-Bus fixtures

The optional top-level `broker` object accepts `dbus_calls` and
`dbus_subscriptions`. Each fixture has a unique stable `id` and a complete
typed request. Call fixtures carry an ordered, nonempty `responses` list whose
entries are `unit`, `string`, `i64`, or a named broker `error`. A
`dbus-signal` step names an opened subscription and supplies typed
`PropertiesChanged` values.

```json
{
  "broker": {
    "dbus_calls": [{
      "id": "playback-status",
      "request": {
        "bus": "session",
        "destination": "org.mpris.MediaPlayer2.playerctld",
        "path": "/org/mpris/MediaPlayer2",
        "interface": "org.freedesktop.DBus.Properties",
        "member": "Get",
        "arguments": ["org.mpris.MediaPlayer2.Player", "PlaybackStatus"],
        "reply": "variant-string"
      },
      "responses": [{ "kind": "string", "value": "Playing" }]
    }]
  }
}
```

Fixture construction and every guest request pass through the same production
D-Bus decoder, identifier checks, supported-shape checks, exact manifest-scope
matching, and physical-activation ledger as the real supervisor. Replay derives
an activation only from an `activated` event produced by replayed touch input.
An out-of-scope fixture is rejected before the component starts; an unexpected
request, mismatched response type, unopened signal, unused response, or unopened
subscription fails the replay. The JSON report records fixture IDs and outcomes
without exposing raw protocol payloads.

The complete executable example is
`examples/broker-component-plugin/tests/replay.json`.

## Typed HTTP fixtures

The top-level `broker.http_requests` list supports both `request` and
`request-stream`. Each fixture names one exact typed method, URL, accept value,
content type, and body. Text bodies use JSON strings; binary bodies use byte
arrays. An ordered response queue contains a typed inline response, typed
stream, or named broker error.

```json
{
  "id": "latest-commit",
  "operation": "request-stream",
  "request": {
    "method": "GET",
    "url": "https://api.github.com/repos/example/project/commits/?per_page=1",
    "accept": "application/vnd.github+json",
    "content_type": null,
    "body": null
  },
  "responses": [{
    "kind": "stream",
    "status": 200,
    "final_url": "https://api.github.com/repos/example/project/commits/?per_page=1",
    "content_type": "application/json",
    "etag": "fixture-v1",
    "chunks": ["{\"sha\":", "\"abc\"}"],
    "terminal_error": null
  }]
}
```

Fixture setup and every guest request pass through `HttpRequestBackend`'s
production payload, URL, method, path-prefix, request-size, response-size,
private-network, and rate authorization. The fake broker never resolves DNS or
opens a socket. Stream metadata, bounded chunks, and completion/error events
use the production wire encoders and ordered resource sequences. Redirects are
not modeled in replay v1, so `final_url` must equal the requested URL. Wrong
operation/response kinds, out-of-scope fixtures, unexpected requests, invalid
chunks, and unused responses fail the replay.

The complete streaming example is
`examples/http-component-plugin/tests/replay.json`.

## Typed command fixtures

The top-level `broker.command_runs` list matches a complete `command.run.v1`
request by manifest command ID and typed named values. Supported value kinds are
`integer`, `fixed-enum`, `text`, `approved-file`, and `url`; value order is not
significant. Every fixture request traverses the production command decoder and
the normalized manifest rule authorizer, but the replay host never resolves an
executable or starts a process.

```json
{
  "id": "command-probe",
  "request": {
    "command_id": "probe",
    "values": []
  },
  "responses": [{
    "kind": "completed",
    "output": [
      { "kind": "stdout", "body": "touchbar command probe\n" }
    ],
    "exit_code": 0,
    "signal": null
  }]
}
```

Output entries preserve stdout/stderr interleaving and accept either JSON text
or byte arrays. `completed` emits a typed exit event with host-derived stream
byte totals and requires exactly one of `exit_code` or `signal`. `failed` emits
any preceding output and a terminal named broker error; `error` rejects the
open immediately. Individual chunks use the production wire encoder and their
combined size cannot exceed the matched manifest rule's
`maximum_output_bytes`. Unexpected or unused runs fail the replay.

The executable first-party example is
`plugins/command-deck/tests/replay.json`. Its harmless probe follows the real
asynchronous open/resource/exit lifecycle and renders the terminal `OK` result
without starting a process during replay; the live UI additionally exposes its
intermediate `RUN` state.

## Typed filesystem-read fixtures

The top-level `broker.filesystem_reads` list supports `read-file`,
`list-directory`, and `read-file-stream`. Requests name only a logical mount
from the manifest plus a normalized relative path; read operations also carry
an offset and maximum byte count, while lists carry `maximum_entries`. Replay
creates a synthetic non-openable binding for each declared label so requests
traverse the production decoder, mount/path checks, kind policy, and normalized
scope without selecting or reading any host directory.

Directory responses contain uniquely name-sorted `regular-file` or `directory`
entries plus `truncated`. Inline file responses contain `total_size` and a text
or binary body. Stream responses contain `total_size`, ordered text/binary
chunks, and an optional terminal broker error. The host derives offsets, EOF,
end offset, and total bytes exactly as production does.

Every response is checked against the declared `maximum_file_bytes`, the
request's `maximum_bytes` or `maximum_entries`, allowed entry kinds, production
chunk encoding, and the claimed total file size. A successful stream must
either fill the requested range or reach EOF; partial streams require an
explicit terminal error. Response-kind mismatches, traversal, unsorted or
duplicate names, unexpected requests, and unused responses fail closed.

The complete list-then-stream example is
`examples/filesystem-component-plugin/tests/replay.json`.

## Typed filesystem-write fixtures

The top-level `broker.filesystem_writes` list covers `create-file`,
`replace-file`, `append-file`, `delete-file`, `rename`, `create-directory`, and
the three corresponding `*-file-stream` operations. Every request uses a
manifest-declared logical mount and a normalized relative path. Inline bodies
accept text or byte arrays; mutation results report exact bytes written and the
resulting size where the production operation has one.

```json
{
  "id": "create-demo",
  "operation": "create-file",
  "request": {
    "mount": "workspace",
    "path": "touchbar-replay.txt",
    "body": "TouchBar replay\n"
  },
  "responses": [{
    "kind": "mutation",
    "bytes_written": 16,
    "resulting_size": 16
  }]
}
```

Stream fixtures declare `expected_bytes`, ordered `chunks`, and ordered
`commits`. Each successful chunk advances the exact production offset; a
failed chunk does not. Commit is accepted only after exactly the declared byte
count and produces the same terminal resource event as the real broker. Closing
an uncommitted replay resource aborts it, and an open error cannot carry later
traffic.

Fixture construction and guest requests both traverse the production
filesystem-write authorization functions. Replay enforces operation bits,
mount bindings, traversal and reserved-name rejection, inline/chunk/file caps,
rolling-hour byte quota, exact request order, resource lifecycle, and receipt
consistency without opening or changing a host path. Unexpected requests,
unused responses, unclosed resources, mismatched offsets, partial commits, and
topology operations outside policy fail closed.

The executable create-file example is
`examples/filesystem-component-plugin/tests/write-replay.json`.

## Typed local-service fixtures

The top-level `broker.local_connections` list models a framed
`local.connect.v1` service without creating a Unix socket. Each fixture matches
one exact endpoint label and protocol, may deliver initial peer events, and
contains an ordered set of send exchanges. Every exchange expects one exact
text or binary frame, returns either success or a named broker error, and can
then deliver ordered peer frames, clean closure, or a terminal broker error.

```json
{
  "id": "custom-action-service",
  "request": {
    "endpoint": "command-deck",
    "protocol": "io.github.cameroncooper.touchbar.command-deck.v1"
  },
  "exchanges": [{
    "send": "{\"action\":1}",
    "result": { "kind": "success" },
    "events": [
      { "kind": "frame", "body": "{\"ok\":true,\"action\":1}" },
      { "kind": "closed" }
    ]
  }]
}
```

Connect and send both use the same extracted production authorization
functions as the real Unix-socket backend. Replay supplies a synthetic
non-connectable binding for each declared endpoint and never resolves or opens
it. Frame encoding, the manifest frame-size and aggregate traffic limits, the
production 60-event/second bound, resource sequences, and terminal ordering are
validated before the guest starts. `open_error` models a failed connection;
events after `closed` or `error` are invalid. Unknown connections, frames sent
out of order, unopened fixtures, and unused exchanges fail closed.

The complete connect-send-receive-close example is
`plugins/command-deck/tests/custom-replay.json`; the sandboxed Command Deck guest
decodes the peer frame and rerenders `OK 22B`.

## Typed desktop-action fixtures

The top-level `broker.notification_requests` list models exact `send` and
`remove` operations. Send requests contain the plugin-local ID, category,
`low`/`normal`/`critical` urgency, title, and body; remove requests contain only
the ID. `broker.uri_opens` matches one exact URI. Both fixture types carry an
ordered nonempty list of `success` or named broker-error responses.

```json
{
  "broker": {
    "notification_requests": [{
      "id": "build-finished",
      "operation": "send",
      "request": {
        "id": "build-finished",
        "category": "status",
        "urgency": "normal",
        "title": "Build complete",
        "body": "All checks passed"
      },
      "responses": [{ "kind": "success" }]
    }],
    "uri_opens": [{
      "id": "project-docs",
      "request": { "uri": "https://example.com/docs/start" },
      "responses": [{ "kind": "success" }]
    }]
  }
}
```

Notification fixtures use the production payload decoder and exact
category/urgency/text/ID scope authorizer. The replay clock applies the
manifest rolling-minute send cap. URI fixtures use the production URL,
scheme, origin, path, and physical-activation authorizer; only an `activated`
event derived from replayed touch supplies runtime authority. A rejecting
transport sits behind both paths, so replay cannot contact the desktop portal,
display a notification, or open a browser. Out-of-scope fixtures, unexpected
requests, shape mismatches, rate violations, and unused responses fail the
replay.

The complete executable example is
`examples/desktop-action-component-plugin/tests/replay.json`.

## Typed clipboard fixtures

The top-level `broker.clipboard_requests` list models `read` and `write` as
separate capabilities. A read request names one MIME type and returns either a
typed value or broker error. A write request includes an exact text or byte-array
body and returns success or an error.

```json
{
  "broker": {
    "clipboard_requests": [
      {
        "id": "read-text",
        "operation": "read",
        "request": { "mime_type": "text/plain;charset=utf-8" },
        "responses": [{
          "kind": "value",
          "mime_type": "text/plain;charset=utf-8",
          "body": "fake test value"
        }]
      },
      {
        "id": "write-text",
        "operation": "write",
        "request": {
          "mime_type": "text/plain;charset=utf-8",
          "body": "copied by plugin"
        },
        "responses": [{ "kind": "success" }]
      }
    ]
  }
}
```

Fixture construction and runtime requests share the production MIME, binding,
byte-limit, and physical-activation authorizer. Reads and writes share one
scenario-clock rolling-rate ledger just as they share one source-bound durable
ledger in production. A synthetic absolute binding cannot connect to Wayland,
and replay never constructs or executes a clipboard transport. Read response
MIME must equal the requested MIME, response and request bodies must fit the
manifest maximum, and only replayed physical-origin taps can authorize either
operation. Shape mismatches, unexpected traffic, and unused responses fail.

Use conspicuously fake fixture values—never copy a real secret or clipboard
history into a repository. The complete executable example is
`examples/clipboard-component-plugin/tests/replay.json`.

## Typed secret-read fixtures

The top-level `broker.secret_reads` list matches one exact manifest-local
logical name. Each ordered response is either a typed value with content type
and text/binary body, or a named broker error.

```json
{
  "broker": {
    "secret_reads": [{
      "id": "demo-credential",
      "request": { "logical_name": "demo-token" },
      "responses": [{
        "kind": "value",
        "content_type": "application/octet-stream",
        "body": "fixture-only-token"
      }]
    }]
  }
}
```

Replay synthesizes a valid, non-resolvable Secret Service item binding for
every allowed logical name. Fixture construction and runtime requests share the
production name/scope/binding and fresh physical-activation authorizer. Values
use the production 48 KiB and content-type validator and wire encoder, but no
secret transport is constructed and no D-Bus connection is possible.

The value travels only in the private fake broker response required to exercise
guest decoding. Reports record the fixture ID and outcome, never the request
payload or returned bytes. Errors likewise contain no value material. Plugins
should render only safe derived state such as availability or byte length in
tests. Always use conspicuously fake values; never place a real credential in a
scenario, snapshot, or source file. The executable redaction example is
`examples/secret-component-plugin/tests/replay.json`.
