# Sandboxed package assets

Component plugins can render PNG images and symbolic SVGs without receiving a
package directory, pathname, byte buffer, file descriptor, or GPU object. Assets
are immutable package inputs selected by stable logical IDs and decoded by the
trusted native host.

## Manifest contract

Each asset is explicit:

```toml
[[asset]]
id = "touchbar-wordmark"
path = "assets/touchbar.svg"
kind = "symbolic-svg"
width = 171
height = 40
```

IDs are unique lowercase kebab-case. Paths are unique normalized relative paths
beneath `assets/`; `png` requires `.png` and `symbolic-svg` requires `.svg`.
The declared dimensions are the exact PNG dimensions or the fixed SVG raster
target. A package may declare at most 64 assets, each no larger than 2008×240,
with at most 2,097,152 decoded pixels in total. Encoded files are limited to 1
MiB each and the complete sealed bundle to 8 MiB.

The component UI references only the ID:

```rust
let logo = view.image(
    "touchbar-wordmark",
    "TouchBar",
    ImageFit::Contain,
    ImageTint::Mask(ColorRole::Accent),
);
```

`Contain`, `Cover`, and `Stretch` define fitting. Tint can preserve original
pixels, multiply them by a semantic theme role, or treat alpha as a mask filled
with a semantic theme role. Tint is resolved while building each native scene,
so a theme revision recolors a retained image without re-decoding or uploading
it.

## Trust boundary

1. The installer includes every declared asset in both the content-addressed
   package digest and its installer-owned per-artifact digest map.
2. `touchbar-sessiond` re-inspects installed content and requires its manifest,
   package digest, and complete artifact map to equal the durable lock.
3. The session daemon gives the supervisor only logical asset IDs and their
   installer-owned SHA-256 values.
4. The supervisor pins the package root and opens each declared path with
   `openat2` using beneath, no-magic-link, no-symlink, and in-root resolution.
   It accepts only bounded, single-link regular files and hashes the bytes it
   copies.
5. Assets are serialized in manifest order into one versioned memory file,
   sealed against write, growth, shrink, and further seal changes, and placed in
   fixed child descriptor slot 6. Every descriptor from slot 7 upward is closed.
6. The host accepts slot 6 only with all required seals, parses the bounded
   format exactly, and requires its count, order, and IDs to match the already
   sealed manifest. Missing, unsolicited, duplicate, truncated, or trailing
   data fails startup.
7. The host applies component confinement before parsing or decoding the asset
   payload. It then instantiates Wasm without preopened directories or inherited
   package descriptors. The guest receives only its WIT render request and the
   declared logical IDs it already knows.

Changing a package path after the supervisor opens it cannot change the sealed
bytes delivered to the host. Tests also prove that forged digests stop launch
and that the inherited bundle cannot be written.

## Decode and GPU policy

PNG allocation is based on manifest dimensions. The PNG header must match those
dimensions before an output buffer is allocated, decoder memory has a separate
bound, 16-bit channels are stripped, palette data is expanded, and the result
is normalized to RGBA8.

Symbolic SVG source must be UTF-8 and remains subject to the UI toolkit's source,
raster-edge, pixel, and cache bounds. External or active constructs—including
images, scripts, foreign objects, hrefs, CSS URLs/imports, doctypes, and
entities—are rejected. The host rasterizes once to the manifest dimensions;
theme color is applied later by the GLES mask shader.

Each component-host process owns a maximum of 64 immutable package images and a
2-megapixel decoded set. `touchbar-ui` keys textures by stable image ID and
revision, replaces stale revisions, and frees every texture with the renderer.
The first-party Media package exercises this path with the project wordmark.
Headless/GPU acceptance is
`scripts/test-asset-component-ui.sh`; the physical demo is:

```bash
./scripts/run-logo-physical.sh 15
```
