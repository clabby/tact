FROM rust:bookworm AS builder

WORKDIR /src
COPY . .
RUN cargo build --locked --release

FROM scratch

COPY --from=builder /src/target/release/tact /tact
