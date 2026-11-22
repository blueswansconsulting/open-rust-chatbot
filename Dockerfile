# syntax=docker/dockerfile:1
# Base images are pinned by digest; Dependabot keeps them current.
FROM rust:1.100-slim-bookworm@sha256:7c7d4eb4d05b7781187a327809466cfddf29753f6defdc3d19a0e291951b7a37 AS build
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
