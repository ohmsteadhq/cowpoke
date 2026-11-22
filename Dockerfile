# syntax=docker/dockerfile:1
FROM rust:1.93-bookworm AS build
WORKDIR /src
# libpcap for the monitor service's live capture.
RUN apt-get update && apt-get install -y --no-install-recommends libpcap-dev && rm -rf /var/lib/apt/lists/*
# Build dependencies first so code changes don't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY src src
COPY ui ui
COPY templates templates
RUN touch src/main.rs && cargo build --release
# The shared libraries the binary needs beyond what the runtime image has (libpcap and
# what it links), at their own paths; and the data folder, owned by the runtime user.
RUN mkdir -p /out/libs /out/data \
    && ldd target/release/cowpoke | awk '/=> \// {print $3}' \
       | grep -vE '/(libc|libm|libgcc_s|libdl|libpthread|librt)\.so' \
       | xargs -r -I{} cp --parents -L {} /out/libs \
    && chown 10001:10001 /out/data

# Distroless: glibc, CA certificates and timezone data, no shell or package manager.
FROM gcr.io/distroless/cc-debian12
COPY --from=build /out/libs/ /
COPY --from=build /src/target/release/cowpoke /usr/local/bin/cowpoke
COPY --from=build --chown=10001:10001 /out/data /data
COPY examples /usr/share/cowpoke/examples
# The same user id as earlier images, so existing /data volumes stay writable.
USER 10001:10001
ENV COWPOKE_DATA=/data COWPOKE_BIND=0.0.0.0:8080
VOLUME /data
# Web UI and API. Services listen on the ports their settings name: publish those too.
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s CMD ["/usr/local/bin/cowpoke", "health"]
# Cowpoke runs as PID 1: it starts no child processes and handles SIGTERM and Ctrl-C itself.
ENTRYPOINT ["/usr/local/bin/cowpoke"]
