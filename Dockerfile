FROM rust:1.97-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets

RUN cargo build --locked --release && \
    install -Dm755 target/release/opencode-go-usage /out/opencode-go-usage

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818 AS runtime

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
