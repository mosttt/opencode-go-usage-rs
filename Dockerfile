FROM rust:1.94-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets

RUN cargo build --locked --release && \
    install -Dm755 target/release/opencode-go-usage /out/opencode-go-usage

FROM debian:bookworm-slim AS runtime

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
