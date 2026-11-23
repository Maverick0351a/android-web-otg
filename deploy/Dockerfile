FROM rust:1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN groupadd -f plugdev \
    && useradd -r -g plugdev -s /sbin/nologin otg

COPY --from=builder /build/target/release/android-web-otg /usr/local/bin/android-web-otg

USER otg
EXPOSE 8095
# Binds 0.0.0.0:8095 and fails closed unless ANDROID_WEB_OTG_TOKEN is set at runtime.
ENTRYPOINT ["/usr/local/bin/android-web-otg", "--host", "0.0.0.0", "--port", "8095"]
