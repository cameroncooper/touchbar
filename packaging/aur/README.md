# Arch packaging

Two `PKGBUILD`s are kept in-tree so packaging changes land with the code that
requires them. Publishing to the AUR is a separate manual step: each AUR
package is its own git repository containing only `PKGBUILD`, `.SRCINFO`, and
`touchbar.install`.

| Directory | AUR package | Source |
|---|---|---|
| `touchbar/` | `touchbar` | the `vMAJOR.MINOR.PATCH` release tarball |
| `touchbar-git/` | `touchbar-git` | the `main` branch |

Both install the layout described in `packaging/README.md` and depend on
`tiny-dfr`, which is the verified rollback target: activation refuses to
proceed without its unit installed.

Installation is inert. Neither package starts a service or takes over the
Touch Bar; that requires `touchbar-activate`, which writes the root-owned
`/etc/touchbar/enabled` marker and masks `tiny-dfr`. Removing the package
hands the strip back first.

`x86_64` is listed because Intel T2 MacBooks are x86_64. Their presentation
path is implemented but not yet hardware-verified.

## Updating a release

```bash
cd packaging/aur/touchbar
# Point at the new tag, then refresh the checksum and metadata.
updpkgsums
makepkg --printsrcinfo > .SRCINFO
makepkg -f            # verify it builds before publishing
```

Copy the three files into the AUR repository and push.
