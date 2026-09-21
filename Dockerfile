FROM rust:1.97-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS builder

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
