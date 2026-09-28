FROM rust:1.98-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets

RUN cargo build --locked --release && \
    install -Dm755 target/release/opencode-go-usage /out/opencode-go-usage

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS runtime

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
