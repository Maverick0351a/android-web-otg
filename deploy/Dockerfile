FROM rust:1-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY static ./static
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends adb ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/android-web-otg /usr/local/bin/android-web-otg

EXPOSE 8095
ENTRYPOINT ["/usr/local/bin/android-web-otg", "--host", "0.0.0.0", "--port", "8095"]
