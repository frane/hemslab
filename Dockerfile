# Cross-compiles on the build machine's own architecture: building eebus under QEMU
# emulation would take an hour.
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
RUN if [ "$TARGETARCH" = "arm64" ]; then \
      apt-get update && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
      && rm -rf /var/lib/apt/lists/* && rustup target add aarch64-unknown-linux-gnu; \
    fi
WORKDIR /src
COPY . .
RUN case "$TARGETARCH" in \
      arm64) TARGET=aarch64-unknown-linux-gnu; \
             export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
                    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc ;; \
      *)     TARGET=x86_64-unknown-linux-gnu ;; \
    esac \
    && cargo build --release --locked --target "$TARGET" \
    && cp "target/$TARGET/release/hemslab" /hemslab

FROM debian:bookworm-slim
COPY --from=build /hemslab /usr/local/bin/hemslab
ENV HEMSLAB_STATE=/data
VOLUME /data
ENTRYPOINT ["hemslab"]
CMD ["run", "heat-pump", "pv-inverter", "wallbox"]
