FROM rust:1.97-bookworm@sha256:14bc9c5966e7b3a385794b3d5389a8765668342025fbcc7b2e3d2866ac4bd8c3 AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets

RUN cargo build --locked --release && \
    install -Dm755 target/release/opencode-go-usage /out/opencode-go-usage

FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241 AS runtime

RUN apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 10001 opencode && \
    useradd --uid 10001 --gid opencode --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin opencode

COPY --from=builder /out/opencode-go-usage /usr/local/bin/opencode-go-usage

WORKDIR /app
USER 10001:10001
EXPOSE 8787
STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/local/bin/opencode-go-usage"]
