# Contributing

Thanks for helping! Small, focused pull requests are easiest to review.

## Dev setup

```sh
DEV_MODE=true MOCK_LLM=true KNOWLEDGE_BASE_PATH=examples/knowledge-base.md cargo run
cargo test          # unit + end-to-end WebSocket tests, a few seconds
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo build && (cd tests/browser && npm ci && npx playwright install chromium && npm test)   # widget browser tests
```

The widget (`static/widget.js`) is plain JavaScript with no build step and is embedded in the binary.

## Guidelines

- Add or update a test for every behaviour change. Bug fixes should include a test that failed before the fix.
- Keep the widget dependency-free and small, and never render server or visitor text with `innerHTML`.
- New tools the model can call must be read-only or require explicit visitor confirmation.
- New dependencies need a good reason and must pass `cargo deny check`.
- Security issues: see [SECURITY.md](SECURITY.md), not public issues.

By contributing you agree your work is licensed under the MIT license and that you'll follow the
[Code of Conduct](CODE_OF_CONDUCT.md).
