# syntax=docker/dockerfile:1
FROM rust:1.93-bookworm AS build
WORKDIR /src
# Build dependencies first so code changes don't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY src src
COPY ui ui
COPY templates templates
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data cowpoke \
    && mkdir /data && chown cowpoke /data
COPY --from=build /src/target/release/cowpoke /usr/local/bin/cowpoke
COPY examples /usr/share/cowpoke/examples
USER cowpoke
ENV COWPOKE_DATA=/data COWPOKE_BIND=0.0.0.0:8080
VOLUME /data
# Web UI and API. Services listen on the ports their settings name: publish those too.
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/cowpoke"]
