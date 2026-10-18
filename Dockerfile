# syntax=docker/dockerfile:1
# Base images are pinned by digest; Dependabot keeps them current.
FROM rust:1.95-slim-bookworm@sha256:d7482085ff5b415f84dba5647ae71606650bdef00db7aeb69f4b3d170c3e4082 AS build
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
