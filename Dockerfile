# syntax=docker/dockerfile:1

# The workspace path-depends on ../davinci-zkvm/{rust-sdk,input-gen}, so the
# build context is the directory holding both checkouts side by side:
#
#   docker build -f davinci-sequencer/Dockerfile -t davinci-sequencer .
#
# Dockerfile.dockerignore (next to this file) trims that context to the
# sequencer workspace and the two davinci-zkvm crates.

FROM rust:1.95-bookworm AS builder
WORKDIR /src
# rust-sdk inherits its version from the davinci-zkvm workspace manifest.
COPY davinci-zkvm/Cargo.toml davinci-zkvm/Cargo.toml
COPY davinci-zkvm/rust-sdk davinci-zkvm/rust-sdk
COPY davinci-zkvm/input-gen davinci-zkvm/input-gen
COPY davinci-sequencer davinci-sequencer
WORKDIR /src/davinci-sequencer
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/davinci-sequencer/target \
    cargo build --release -p davinci-sequencer --locked && \
    cp target/release/davinci-sequencer /usr/local/bin/davinci-sequencer

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install --no-install-recommends -y ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --uid 10001 --user-group --home-dir /data --shell /usr/sbin/nologin davinci && \
    install -d -m 0700 -o davinci -g davinci /data
COPY --from=builder /usr/local/bin/davinci-sequencer /usr/local/bin/davinci-sequencer
ENV DAVINCI_DATADIR=/data
VOLUME ["/data"]
EXPOSE 9090
USER davinci
ENTRYPOINT ["davinci-sequencer"]
