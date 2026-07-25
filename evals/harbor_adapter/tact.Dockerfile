# syntax=docker/dockerfile:1.7

FROM rust:1.90-alpine3.22 AS build

ARG CARGO_PROFILE=release
ARG TARGETARCH

WORKDIR /build
RUN apk add --no-cache musl-dev
COPY . .
RUN --mount=type=cache,id=tact-harbor-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=tact-harbor-target-${TARGETARCH},target=/build/target \
    cargo build --locked --profile "${CARGO_PROFILE}" --features harbor-evals && \
    case "${CARGO_PROFILE}" in \
        dev) artifact_dir=debug ;; \
        *) artifact_dir="${CARGO_PROFILE}" ;; \
    esac && \
    mkdir /out && \
    cp "target/${artifact_dir}/tact" /out/tact

FROM scratch AS artifact
COPY --from=build /out/tact /tact
