# syntax=docker/dockerfile:1
FROM rust:1.95-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/open-rust-chatbot /usr/local/bin/open-rust-chatbot
COPY examples/knowledge-base.md /data/knowledge-base.md
ENV HOST=0.0.0.0 \
    PORT=8787 \
    KNOWLEDGE_BASE_PATH=/data/knowledge-base.md
EXPOSE 8787
USER nonroot
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["open-rust-chatbot", "healthcheck"]
ENTRYPOINT ["open-rust-chatbot"]
