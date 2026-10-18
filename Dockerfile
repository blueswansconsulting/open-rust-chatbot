# syntax=docker/dockerfile:1
# Base images are pinned by digest; Dependabot keeps them current.
FROM rust:1.98-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /src/target/release/open-rust-chatbot /usr/local/bin/open-rust-chatbot
COPY examples/knowledge-base.md /data/knowledge-base.md
ENV HOST=0.0.0.0 \
    PORT=8787 \
    KNOWLEDGE_BASE_PATH=/data/knowledge-base.md
EXPOSE 8787
USER nonroot
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["open-rust-chatbot", "healthcheck"]
ENTRYPOINT ["open-rust-chatbot"]
