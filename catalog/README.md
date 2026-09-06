# TouchBar pack catalog

This directory is the reviewed discovery index for independently published
TouchBar packs. It contains metadata only. Package bytes continue to come from
the publisher's GitHub Release, and the installer verifies that release's
digest, package identity, compatibility, and advertised provenance.

The catalog is compiled into `touchbarctl`. This deliberately makes aliases
part of a reviewed core release instead of allowing a mutable network document
to silently redirect an install. Users can always bypass discovery and install
any compatible public repository by canonical source or exact GitHub URL.
The initial index is intentionally empty until the corresponding standalone
repositories publish releases that pass the same online gate; local monorepo
fixtures are not presented as installable releases.

## Submit a pack

Publish a canonical `vMAJOR.MINOR.PATCH` release containing the fixed
`touchbar-plugin.touchbar` asset first. From the package checkout, run:

```bash
touchbarctl plugin submit --alias my-pack --categories productivity,system
```

The command derives the repository, display name, and description from the
validated package manifest and emits a `[[plugin]]` entry. Insert it into
`plugins.toml` in alias order and open a pull request. For the first listing,
replace the initial `plugin = []` line with the emitted table. Aliases and categories
are lowercase kebab-case; categories are uniquely sorted.

Before submission, run:

```bash
touchbarctl plugin catalog-check --catalog catalog/plugins.toml
touchbarctl plugin catalog-check --catalog catalog/plugins.toml --online
```

The online check downloads each latest stable release into an isolated
temporary store, checks GitHub's streamed digest and any advertised
attestation, unpacks the bounded archive, validates its source, version, host
API, permissions, artifacts, name, and description, then discards it.
Sandboxed component releases are also instantiated at the standard responsive
width matrix. Native releases are structurally validated but never executed by
catalog CI.

Publishers submit entries as `tier = "listed"`. Maintainers alone assign
`curated` or `first-party` during review. Those editorial signals remain
separate from the installer-derived immutable-release and attestation state.
An accepted alias can never be removed, reused, or pointed to a different
repository. Delisting changes its state from `active` to the permanent
`retired` tombstone; retired entries are excluded from search and installation.
