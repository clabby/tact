# `docker`

The `tact` image is a minimal binary carrier. It contains `/tact` and no runtime, entrypoint, or
supporting files. Copy the binary into a broader glibc-based image that provides the shell, CA
certificates, and other tools the agent should be able to use:

```dockerfile
FROM ghcr.io/clabby/tact:latest AS tact

FROM debian:bookworm-slim
COPY --from=tact /tact /usr/local/bin/tact
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["tact"]
```

## Build

From the repository root:

```sh
docker buildx bake -f docker/docker-bake.hcl
```
