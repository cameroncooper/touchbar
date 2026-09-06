# DMA-BUF synchronization

TouchBar v1 keeps plugin rendering and composition on the GPU while making
buffer ownership explicit. The protocol supports both modern explicit acquire
synchronization and a compatibility fallback to Linux DMA-BUF implicit
synchronization. Release is always asynchronous.

## Acquire contract

The Rust GLES client runtime renders into the EGL window, creates an
`EGL_SYNC_NATIVE_FENCE_ANDROID`, flushes the producer command stream, duplicates
the resulting Linux `sync_file`, and sends it with
`touchbar_surface_v1.set_acquire_fence` before `wl_surface.commit`.

The request is single-use, double-buffered surface state. It applies only to
the DMA-BUF attached by the next commit. `touchbar-sessiond` consumes the fd
atomically with that buffer, validates it with `SYNC_IOC_FILE_INFO`, imports it
as an EGL native-fence sync, and calls `eglWaitSync` before importing and
sampling the DMA-BUF. `eglWaitSync` queues a dependency in the compositor GPU
stream; it does not wait on the CPU event thread.

A client may omit the request when its EGL implementation does not expose
`EGL_ANDROID_native_fence_sync`. That commit follows the normal implicit-sync
DMA-BUF path. The runtime reports explicit and implicit frame counts separately
so a conformance test cannot mistake fallback behavior for explicit sync.

These are fatal `touchbar_surface_v1` protocol errors:

- supplying two acquire fences before one commit;
- committing an acquire fence without an attached buffer;
- supplying an acquire fence with a SHM or otherwise non-DMA-BUF buffer;
- supplying an fd that is not a valid Linux `sync_file`.

The compositor never interprets a generic pollable fd as a fence. Failed
validation closes the received descriptor and disconnects only the offending
client.

## Release contract

After the acquire dependency, the compositor copies the submitted image into
that plugin's retained layer and inserts a GLES completion fence. It polls that
fence with a zero timeout during normal event-loop iterations and emits
`wl_buffer.release` only after the copy is complete. A slow producer therefore
does not erase other retained layers, and a slow consumer does not block the
Wayland loop.

Every fd has one owner at each step:

1. Wayland duplicates the client's borrowed acquire fd for transport.
2. The server owns the received `OwnedFd` while it is pending.
3. Successful EGL import transfers that fd to EGL; failed validation or import
   closes it in Rust.
4. A dropped pending commit, role destruction, or client teardown drops any
   unconsumed fence.
5. The client owns its buffer again only after `wl_buffer.release`.

No migration or older custom synchronization request exists: this is the sole
fresh v1 contract.

## Acceptance

`scripts/test-explicit-sync.sh` is nonphysical: it uses the Apple M1 render node
but never opens ADP, changes service state, or interrupts the active Touch Bar
owner. It requires 120 explicitly fenced DMA-BUF frames, zero implicit frames,
zero invalid frames, complete buffer release, and at least 55 FPS. It then runs
separate duplicate-fence, no-buffer, SHM-buffer, and fake-eventfd clients,
requires the exact v1 error code, and requires each connection to be terminated.
