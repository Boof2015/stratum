# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM rust:1.88.0-bookworm AS build
ARG BUILDARCH
ARG TARGETARCH
# Compile natively even for another target; running rustc under QEMU is fragile.
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu; compiler=gcc-x86-64-linux-gnu; headers=libc6-dev-amd64-cross ;; \
      arm64) target=aarch64-unknown-linux-gnu; compiler=gcc-aarch64-linux-gnu; headers=libc6-dev-arm64-cross ;; \
      *) echo 'Supported targets: linux/amd64, linux/arm64' >&2; exit 1 ;; \
    esac \
    && rustup target add "$target" \
    && if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
         apt-get update && apt-get install -y --no-install-recommends "$compiler" "$headers" \
         && rm -rf /var/lib/apt/lists/*; \
       fi
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
    esac \
    && cargo build --locked --release --target "$target" --bin sidecar \
    && install -D -m 0755 "target/$target/release/sidecar" /out/sidecar

# Optional targets; neither test tools nor build tools enter the runtime image.
FROM build AS test
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg systemd \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 sidecar \
    && useradd --uid 10001 --gid sidecar --create-home sidecar \
    && chown -R sidecar:sidecar /build /usr/local/cargo
COPY tests ./tests
COPY examples ./examples
COPY packaging/sidecar.service /etc/systemd/system/sidecar.service
RUN install -m 0755 /out/sidecar /usr/local/bin/sidecar \
    && systemd-analyze verify --man=no /etc/systemd/system/sidecar.service
USER sidecar
RUN cargo test --locked -- --test-threads=1

# Isolated installer checks: root only inside this disposable container.
FROM test AS installer-test
USER root
COPY --from=build /out/sidecar /installer/sidecar
COPY packaging/install.sh packaging/sidecar.service packaging/test-installer.sh /installer/
ENV SIDECAR_INSTALLER_TEST=disposable
CMD ["sh", "/installer/test-installer.sh", "/installer"]

FROM build AS package
COPY packaging ./packaging
COPY docs/install.md ./docs/install.md
RUN SIDECAR_BUILD_TARGET_DIR=/build/target SIDECAR_TARGET_ARCH="$TARGETARCH" sh packaging/build-linux.sh

FROM scratch AS archive
COPY --from=package /build/dist/ /

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 sidecar \
    && useradd --uid 10001 --gid sidecar --no-create-home --home-dir /var/lib/sidecar --shell /usr/sbin/nologin sidecar \
    && install -d -m 0700 -o sidecar -g sidecar /var/lib/sidecar
COPY --from=build /out/sidecar /usr/local/bin/sidecar
COPY --chmod=755 packaging/docker-entrypoint.sh /usr/local/bin/sidecar-entrypoint
USER 10001:10001
EXPOSE 7843
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/sidecar-entrypoint"]
CMD ["serve"]
