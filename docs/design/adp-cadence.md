# ADP presentation cadence

The compositor and plugin pipeline can render at 60 frames per second on the
Apple GPU. Physical presentation is currently gated by the vblank cadence that
the Linux ADP DRM driver exposes. Three independent physical runs measured
29.90–29.92 vblank-paced updates per second while the connector advertised a
60 Hz mode.

This note separates facts from hypotheses. A reported 60 Hz DRM mode does not
prove that this panel is physically scanning at 60 Hz: the current ADP driver
does not implement a mode-setting callback or program display timing. It uses
display state initialized by firmware and publishes a fixed software mode.

## Confirmed observations

- The fixed Summit mode is 60 by 2008 pixels, with totals of 188 by 2030 and a
  22,898 kHz clock. Its calculated refresh is 59.999 Hz.
- Blocking `DRM_IOCTL_WAIT_VBLANK` calls return at about 29.90 Hz.
- Framebuffer-changing atomic commits retire at the same rate. Adding a
  separate vblank wait before each blocking commit halves throughput again.
- Direct GPU composition reaches 60 FPS; the presenter drops superseded frames
  at the 60-to-29.9 boundary. This excludes the GPU and plugin scheduler as the
  source of the physical limit.
- The driver turns front-end interrupt status bit 0 into the DRM vblank count.
  It discovers a display-backend IRQ but never requests it, and its atomic
  flush path contains `FIXME: use adbe flush interrupt`.
- The upstream device-tree binding describes the front-end IRQ as the primary
  interrupt carrying vsync and the display-backend IRQ as having an unknown
  function. Therefore the unused backend IRQ is a promising flush-completion
  source, but is not evidence of a second vblank source.
- The original m1n1 `touchbar_bad_apple.py` experiment submits frames with an
  explicit 33 ms sleep. That is consistent with a 30 Hz path, but does not
  establish the panel's physical scan frequency.

The running kernel's ADP and Summit sources match the current Asahi `asahi`
branch for these paths. Upstream history contains correctness fixes around
vblank enablement and locking, but no cadence change.

## Remaining ambiguity

Two explanations still fit the evidence:

1. Firmware configured the display pipe and panel near 30 Hz, while the fixed
   DRM mode incorrectly describes it as 60 Hz.
2. The panel scans at 60 Hz, but the front-end reports only every other frame.

The blocking atomic path is not itself the primary divider because bare
vblank waits already measure the same 29.9 Hz cadence.

It would be unsafe to synthesize 60 Hz release events in userspace. Releasing
an ADP buffer before hardware has stopped scanning it can cause tearing or let
the compositor overwrite an in-flight buffer.

## Reproducible probe

The source-side assumptions are executable rather than prose-only:

```bash
./scripts/audit-adp-cadence-source.sh /path/to/linux
```

The audit fails if the selected kernel no longer has the exact state this
investigation analyzed: a computed 60 Hz Summit mode, front-end vblank
handling, an unrequested backend IRQ, no ADP timing-programming callback, and
the backend-flush FIXME. A failure means the kernel changed and the conclusions
below must be revisited; it is not papered over with a compatibility path.

The physical correlation probe is:

```bash
./scripts/run-adp-cadence-physical.sh 5
```

The guarded runner preserves the current Touch Bar owner and reports:

- the vblank rate calculated from kernel timestamps;
- whether returned DRM frame sequence numbers are contiguous;
- the `adp-fe` IRQ delta from `/proc/interrupts` over the same interval; and
- whether the result is near half-rate, full-rate, or unexpected.

This distinguishes a userspace sequencing problem from a low-rate front-end
interrupt. It still cannot distinguish a 30 Hz physical scan from a 60 Hz scan
with half-rate interrupts.

## Safe path to a kernel fix

1. Run the correlation probe. A near-30 Hz raw IRQ rate together with
   contiguous DRM sequences proves the divider is below the DRM userspace API.
2. Measure physical refresh independently with a high-speed camera or optical
   sensor while alternating black and white buffers. This resolves the actual
   panel cadence without relying on undocumented register assumptions.
3. Trace the firmware/macOS display-pipe setup with m1n1, including both
   interrupt lines and writes around `ADBE_FIFO`. Identify the backend IRQ's
   status and acknowledgement registers before requesting it in Linux.
4. If the panel is physically 30 Hz, correct the mode metadata first. Reaching
   60 Hz would then require complete clock/timing programming rather than an
   interrupt workaround.
5. If the panel is physically 60 Hz but the front-end IRQ is half-rate, locate
   the real per-scan vsync source. Keep backend flush completion separate from
   DRM vblank unless tracing proves they are the same event.
6. If the backend IRQ reliably signals FIFO latch completion, use it to retire
   page-flip events and buffers. Do not acknowledge an undocumented level IRQ
   speculatively; a wrong handler can create an interrupt storm.

Acceptance for a 60 Hz fix requires 55–65 distinct physical updates per second,
safe triple-buffer retirement with no early reuse, no visible tearing, and
successful service handoff plus suspend/resume recovery.

The investigation is complete at the boundary of what can be established
without undocumented-hardware tracing. It intentionally does not ship a guessed
kernel patch: the remaining alternatives require an optical measurement and
m1n1/macOS register and interrupt tracing before a safe change can be designed.

## Primary references

- [Asahi Linux ADP driver](https://github.com/AsahiLinux/linux/blob/asahi/drivers/gpu/drm/adp/adp_drv.c)
- [Asahi Linux Summit panel driver](https://github.com/AsahiLinux/linux/blob/asahi/drivers/gpu/drm/panel/panel-summit.c)
- [Initial ADP driver commit](https://github.com/AsahiLinux/linux/commit/332122eba628d537a1b7b96b976079753fd03039)
- [Vblank enablement correction](https://github.com/AsahiLinux/linux/commit/c082a52125d9007b488d590c412fd126aa78c345)
- [ADP driver patch discussion](https://patchew.org/linux/20250205-adpdrm-v5-0-4e4ec979bbf2@gmail.com/20250205-adpdrm-v5-2-4e4ec979bbf2@gmail.com/)
- [m1n1 Touch Bar experiment](https://github.com/AsahiLinux/m1n1/blob/main/proxyclient/experiments/touchbar_bad_apple.py)
