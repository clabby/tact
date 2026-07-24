# Releases

Releases are driven by a `v`-prefixed Git tag. The release workflow validates, builds, signs, and
publishes every distribution from that tag.

## One-time setup

- Add a repository Actions secret named `CARGO_REGISTRY_TOKEN` containing a crates.io API token
  with permission to publish the `tact` crate.
- Give GitHub Actions read/write workflow permissions so `GITHUB_TOKEN` can create GitHub Releases
  and publish images to GHCR.
- Ensure the releaser can push `main` and create repository tags.

## Publish a release

Start from a clean, up-to-date `main`. Bump the package version in `Cargo.toml` and refresh the root
package entry in `Cargo.lock`:

```sh
cargo check
```

Commit and push that version bump, then wait for the `main` CI run to pass. Create the matching tag
from the release commit and publish it to GitHub:

```sh
release_revision=main@origin
version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "tact") | .version')
release_commit=$(jj log -r "$release_revision" --no-graph -T 'commit_id')
jj tag set "v${version}" -r "$release_revision"
gh api repos/clabby/tact/git/refs \
  --method POST \
  -f ref="refs/tags/v${version}" \
  -f sha="$release_commit"
```

Creating the remote tag is the point of no return. Never reuse or move a published release tag.

## What CI publishes

`.github/workflows/release.yaml` performs the complete release:

- verifies that the tagged commit belongs to `main` and the tag exactly matches the Cargo package
  version;
- builds official Linux x86-64 and ARM64 and macOS Intel and Apple Silicon binaries;
- packages, checksums, and signs all four archives with an ephemeral minisign key;
- injects the public key into the crate metadata and publishes the crate to crates.io;
- creates the GitHub Release with the archives, checksums, signatures, and public key;
- builds the `linux/amd64` and `linux/arm64` GHCR images from the exact signed Linux binaries;
- verifies each image binary byte-for-byte against its release archive; and
- publishes `ghcr.io/clabby/tact:<version>` and `ghcr.io/clabby/tact:latest`.

GitHub Release publication waits for crates.io publication, and container publication waits for the
GitHub Release. A release is complete only when every job in the tag-triggered workflow succeeds.

## Retries

Rerun failed jobs against the original tag. The signing job retains its signed bundle as a workflow
artifact so later publication jobs can reuse the same key and signatures. Before rerunning a
partially completed release, check whether crates.io, the GitHub Release, or GHCR already accepted
its publication. If the source itself is wrong, bump to a new version and create a new tag.
