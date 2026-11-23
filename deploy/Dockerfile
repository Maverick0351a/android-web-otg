FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder

ARG TARGETARCH
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN set -eux; \
    host_arch="$(uname -m)"; \
    if [ "${TARGETARCH:-}" = "arm64" ] && [ "${host_arch}" != "aarch64" ]; then \
      apt-get update && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu && rm -rf /var/lib/apt/lists/*; \
      rustup target add aarch64-unknown-linux-gnu; \
      CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
        cargo build --release --locked --target aarch64-unknown-linux-gnu; \
      cp target/aarch64-unknown-linux-gnu/release/android-web-otg /build/android-web-otg; \
    else \
      cargo build --release --locked; \
      cp target/release/android-web-otg /build/android-web-otg; \
    fi

FROM debian:bookworm-slim

RUN groupadd -f plugdev \
    && useradd -r -g plugdev -s /sbin/nologin otg

COPY --from=builder /build/android-web-otg /usr/local/bin/android-web-otg

USER otg
EXPOSE 8095
# Binds 0.0.0.0:8095 and fails closed unless ANDROID_WEB_OTG_TOKEN is set at runtime.
ENTRYPOINT ["/usr/local/bin/android-web-otg", "--host", "0.0.0.0", "--port", "8095"]
