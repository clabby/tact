# `docker`

The `tact` image purposefully ships as a minimal binary carrier. It contains `/tact` and no runtime,
entrypoint, or supporting files. This makes it a composable build stage rather than a prescribed
development environment.

Copy the binary into an image suited to the developer's needs. That downstream image can provide
the preferred shell, CA certificates, language toolchains, package managers, source-control tools,
and any other utilities the agent should be able to use. It can also enforce deployment-specific
network policy, such as routing outbound traffic through a default-deny egress proxy like
[iron.sh](https://iron.sh/):

```dockerfile
FROM ghcr.io/clabby/tact:latest AS tact

FROM debian:bookworm-slim
COPY --from=tact /tact /usr/local/bin/tact
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["tact"]
```

Versioned and `latest` images contain the exact signed Linux binaries attached to the corresponding
GitHub Release. The release workflow verifies the archive checksum and signature, then compares the
binary copied back out of each image byte-for-byte with the archived binary before publishing the
multi-platform manifest.

## Build

From the repository root:

```sh
docker buildx bake -f docker/docker-bake.hcl
```
