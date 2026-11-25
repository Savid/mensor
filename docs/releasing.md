# Releases

Each release is built and published by `.github/workflows/release.yml`
from an annotated `vX.Y.Z` tag on master, never from a machine.

## Verify

The binaries and the image are attested by that workflow. Check either
with a logged-in `gh`:

```bash
v=0.1.6 f="mensor-$v-$(uname -m)-unknown-linux-musl.tar.gz"
gh attestation verify "$f" -R Savid/mensor \
  --signer-workflow Savid/mensor/.github/workflows/release.yml --source-ref "refs/tags/v$v"
gh attestation verify "oci://ghcr.io/savid/mensor:$v" -R Savid/mensor \
  --signer-workflow Savid/mensor/.github/workflows/release.yml --source-ref "refs/tags/v$v"
```

Offline, add the release's bundle (`--bundle
mensor-X.Y.Z-binaries.sigstore.json`, or `-image`) and a root from
`gh attestation trusted-root` (`--custom-trusted-root`).

## Cut one

1. Set `version` under `[workspace.package]` in `Cargo.toml` (`X.Y.Z`, or
   `X.Y.Z-rc.N` for a pre-release), and run `cargo update --workspace`.
2. Commit to master, and wait for CI to pass.
3. Tag it with a message, which becomes the release's notes, and push.
   Push one tag at a time: releases queue one deep, so a third tag pushed
   while one runs cancels the one waiting.

```bash
git tag -a v0.2.0 -m "What changed."
git push origin v0.2.0
```

![A release, from a pushed tag. verify checks the tag, ci runs every CI job and builds the static binaries and the image, image pushes ghcr.io/savid/mensor as X.Y.Z, X.Y and latest, attested, and release makes the GitHub Release with the tarballs, SHA256SUMS and attestation bundles. A failure before the push publishes nothing; after it, the version is out.](images/release.svg)

A tarball holds `mensor`, `mensor-mind`, `mensor-hand`, the license and the
README. `X.Y` and `latest` move only forward; a pre-release is tagged with its
version alone.

## When it fails

- Before **image** pushes, nothing is out: delete the tag
  (`git push --delete origin vX.Y.Z` and `git tag -d vX.Y.Z`), fix master,
  and tag again.
- After, never move or reuse the tag. Until the Release exists, use
  "Re-run failed jobs", which reuses the run's binaries for 7 days; a full
  re-run would push the image again. Once it exists, **verify** refuses the
  tag. Past 7 days, or for a fix, release the next patch.

## Once

- Make the package `ghcr.io/savid/mensor` public in its settings: a user's
  package can start private.
- A tag ruleset on `refs/tags/v*` stops a release's tag being moved or
  deleted; an admin bypasses it to delete a tag that published nothing.
  Immutable releases, in the repository's settings, freeze a published
  release's tag and assets.
