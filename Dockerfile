FROM rust:1.98-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY dashboard ./dashboard
RUN cargo build --release --locked
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/kubeweasel /usr/local/bin/kubeweasel
RUN mkdir -p /data && chown 65532:65532 /data
USER 65532:65532
ENV KUBEWEASEL_BIND=127.0.0.1:7878
ENV KUBEWEASEL_AUTH_DB=/data/kubeweasel.db
VOLUME ["/data"]
EXPOSE 7878
ENTRYPOINT ["kubeweasel"]
