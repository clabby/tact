# Releases

Releases are driven by a `v`-prefixed Git tag. The same tag starts the binary release and container
workflows, so publish it only after the release commit is on `main` and all checks pass.

## Prerequisites

- Push access to `clabby/tact` and permission to create tags and releases.
- A current Rust stable toolchain, `jj`, and the GitHub CLI (`gh`) authenticated with permission to
  create repository refs. The GitHub CLI is needed because `jj git push` does not push tags.
- GitHub Actions must have read/write workflow permissions. The workflows use the automatic
  `GITHUB_TOKEN` to create the GitHub Release and push to GHCR; no maintainer-managed secret is
  needed for those jobs.

Start from an up-to-date, clean `main`. If `@` already contains work, finish it rather than mixing
unrelated changes into the release revision. Otherwise create a revision for the release:

```sh
jj git fetch --remote origin
jj new main@origin
```

## Prepare the release

1. Choose a semantic version such as `0.2.0`. Set the package `version` in `Cargo.toml`, then update
   and verify the lockfile:

   ```sh
   cargo check
   cargo metadata --locked --no-deps --format-version 1 >/dev/null
   ```

   The first command refreshes the root package entry in `Cargo.lock`. Review the lockfile and do
   not accept unrelated dependency updates accidentally.

2. Run the release checks:

   ```sh
   cargo fmt --all --check
   cargo clippy --all-targets --all-features --locked -- -D warnings
   cargo test --all-targets --all-features --locked
   cargo build --release --locked
   ```

   Also confirm that `cargo metadata --no-deps --format-version 1` reports the intended `tact`
   version. The release workflow rejects a tag whose version does not exactly match it.

3. Describe and publish the release revision:

   ```sh
   jj describe -m "chore: release 0.2.0"
   jj bookmark set main -r @
   jj git push --remote origin --bookmark main
   ```

   Wait for the `main` CI run to pass before tagging. Confirm that `main@origin` resolves to the
   release commit.

## Tag and publish

Create the local tag with `jj`, then create the corresponding lightweight tag ref on GitHub. Replace
the version in both commands and ensure `@` is still the release commit:

```sh
jj tag set v0.2.0 -r @
gh api repos/clabby/tact/git/refs \
  --method POST \
  -f ref=refs/tags/v0.2.0 \
  -f sha="$(jj log -r @ --no-graph -T 'commit_id')"
```

Creating the remote tag is the point of no return: it starts both workflows. Never reuse or move a
published release tag. If repository policy requires signed annotated tags, create the remote tag
with the approved signing process instead; the GitHub API command above creates a lightweight tag.

The tag publishes:

- `.github/workflows/release.yaml`: validates the tag/version match; builds `tact` for Linux x86-64
  and ARM64 and macOS x86-64 and ARM64; packages each binary with `README.md` and `LICENSE.md`;
  writes SHA-256 sidecars; and creates a GitHub Release with generated notes and all archives.
- `.github/workflows/docker.yml`: builds the scratch-based binary-carrier image for `linux/amd64`
  and `linux/arm64`, then publishes a multi-platform manifest as `ghcr.io/clabby/tact:<version>`
  (without the leading `v`) and `ghcr.io/clabby/tact:latest`. The image contains only `/tact`; see
  `docker/README.md` for how to copy it into a runnable image.

## Verify

Watch both tag-triggered runs in GitHub Actions and require every job to succeed. Then verify:

```sh
gh release view v0.2.0
gh release download v0.2.0 --pattern '*.sha256' --dir /tmp/tact-v0.2.0-checksums
docker buildx imagetools inspect ghcr.io/clabby/tact:0.2.0
docker buildx imagetools inspect ghcr.io/clabby/tact:latest
```

Check that the release has four archives and four checksum files, and that the image manifest lists
both `linux/amd64` and `linux/arm64`. Download at least one archive, validate it against its sidecar,
and run `tact --version` from the extracted binary.

## Failures and retries

Do not delete and recreate a published tag to retry a transient failure: consumers may already have
observed the original ref, and moving it can associate artifacts with different source. Re-run only
the failed GitHub Actions jobs or the entire failed workflow for the same tag. The GitHub Release
action and GHCR publication may have partially succeeded, so inspect existing release assets and
image manifests before retrying. If the source itself is wrong, fix it on `main`, increment the
version, and publish a new tag.

The Docker workflow also supports manual dispatch, but a branch dispatch produces a branch-named
image rather than the release version and `latest`; it is useful for diagnosis, not for completing a
tagged release. A rerun of the original tag workflow preserves the intended tags.
