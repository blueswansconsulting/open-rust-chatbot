# Changelog

All notable changes are documented here. This project follows [Semantic Versioning](https://semver.org/).

## [0.1.0] - unreleased

### Added
- Rust (axum) server with a single embedded, dependency-free JavaScript widget.
- OpenAI-compatible streaming with tool calling, retries and timeouts; OpenClaw bridge provider.
- Markdown knowledge base search tool.
- Lead capture with visitor confirmation and HMAC-signed webhooks.
- Origin allowlist, connection caps, rate limits, size limits, idle timeout, optional Turnstile.
- Docker image (distroless, non-root) and compose file.
- Multi-site mode: many bots from one server under `/b/<id>/...`, with settings, knowledge base and
  allowed origins loaded from a sites API (`SITES_API_URL`), cached, and invalidated on demand.
- Directory listings datasets: bots can name a listings dataset (`datasets.listings`), fetched by
  sha256, verified and cached; every turn a deterministic lookup by service, place and business name
  adds complete, counted listing rows to text-only prompts, and a `search_directory_listings` tool
  for tool-calling models.

### Fixed
- Text-only replies: a `LEAD: {…}` written on the same line as the reply (or pretty-printed, or
  in a code fence) is now parsed and never shown to the visitor. A parsed lead is the only card
  offered; the typed-number fallback card now takes the visitor's name ("I'm …", "my name is …",
  "this is …") and a message without their contact details.
- Text-only prompts no longer carry the tool-era lead wording ("call propose_lead … name and
  email"): a phone number or an email is enough, a typed phone number with a call-back request
  offers the card, and directory bots say the directory team follows up.
