# Releasing

A release is a tag on master. Nothing is published from a machine:
`.github/workflows/release.yml` publishes it, and the job's token is its
only secret.

## Cut one

1. Set `version` under `[workspace.package]` in `Cargo.toml`: `X.Y.Z`, or
   `X.Y.Z-rc.N` for a pre-release. Run `cargo update --workspace`, which
   writes it into `Cargo.lock`.
2. Commit that to master, and wait for CI to pass.
3. Tag the commit with a message, which becomes the release's notes
   (verify refuses a tag without one), and push the tag:

```bash
git tag -a v0.2.0 -m "What changed, as the release's notes."
git push origin v0.2.0
```

Push one tag at a time. Releases run one after another, and while one runs
GitHub keeps one more waiting: a third tag pushed then cancels the one that
waits.

## What it publishes

Its jobs run one after another, each only once the one before has passed:

1. **verify** refuses a tag that is not `v` and the workspace's version,
   is not on master, or is released already.
2. **ci** runs every CI job on the tagged commit. Among them, `binaries`
   builds `mensor`, `mensor-mind` and `mensor-hand` with the `postgres`
   feature, static (musl), for x86-64 and arm64, each on its own
   architecture with no cache, and `image` builds the image from them
   without pushing it.
3. **image** builds the image `ghcr.io/savid/mensor` from those binaries
   for both architectures, with provenance and an SBOM, and pushes it as
   `X.Y.Z`; as `X.Y` while it is the newest of `X.Y`; and as `latest`
   while it is the newest stable release. A pre-release's image is tagged
   with its version alone.
4. **release** makes the GitHub Release: `mensor-X.Y.Z-<target>.tar.gz` for
   each target, holding the three binaries, the license and the README;
   `SHA256SUMS`; and the attestation bundles. It is marked latest only as
   the image is, and as a pre-release for a tag with a suffix.

The image and the tarballs are attested. The README's Install shows how to
check them with `gh attestation verify`. To check one without reaching
GitHub, pass the release's bundle (`--bundle
mensor-X.Y.Z-binaries.sigstore.json`, or `-image` for the image) and a root
from `gh attestation trusted-root` (`--custom-trusted-root`).

## When it fails

- If **verify** or **ci** fails, nothing is published. Delete the tag
  (`git push --delete origin vX.Y.Z` and `git tag -d vX.Y.Z`), fix master,
  and tag again.
- Once **image** has pushed, the version is out: never move or reuse its
  tag. If **release** then fails, use "Re-run failed jobs" on the run,
  which reuses the run's binaries; its artifacts are kept for 7 days. After
  that, or if the fix needs a change, release the next patch.
- Once the release exists, **verify** refuses its tag, so re-running the
  whole workflow publishes nothing twice.

## Once

After the first release:

- Check that the package `ghcr.io/savid/mensor` is public, in its settings:
  a user's package can start private.
- A tag ruleset on `refs/tags/v*` stops a release's tag being moved or
  deleted; an admin bypasses it to delete a tag that published nothing.
  Immutable releases, in the repository's settings, stop a published
  release's tag and assets changing.
