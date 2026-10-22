# Changelog

All notable changes are documented here. This project follows [Semantic Versioning](https://semver.org/).

## [0.1.0] - unreleased

### Added
- Rust (axum) server with a single embedded, dependency-free JavaScript widget.
- NVIDIA provider (`LLM_PROVIDER=nvidia`, `NVIDIA_API_KEY`): build.nvidia.com / NIM, default model
  `nvidia/nemotron-3-nano-30b-a3b`, thinking off, `<think>` blocks stripped from streamed replies,
  text only unless `NVIDIA_TOOLS=true`; 401/403 are logged as `auth failed` and count as an
  outage, 5xx is retried once, 429 is not.
- A model reply with no visible text (for example only reasoning) is now an error the visitor sees,
  not a blank answer; it does not count as an outage.
- An unknown `LLM_PROVIDER` is now a startup error (previously it silently meant OpenAI-compatible).
  LLM API keys and bridge tokens are redacted from debug output.
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
- Postcode-aware listings: a UK postcode or an outward code with postcode context ranks listings
  by distance (outcode centres from OS Code-Point Open, OGL v3), within 25 km, nearest first, with
  honest counts and an explicit "No listings within 25 km of …" note; follow-ups keep the postcode.

### Fixed
- Red-team findings (#27-#31): lead fields (card, inquiry and webhook) are one line of plain text,
  cut at any `LEAD:` marker, without JSON, links, emails or hosts, capped (name 80, best time 60,
  message 300); names and times with markup or links are rejected. Replies are plain text
  (Markdown links become `text: url`) and only carry links to the bot's own origins, its knowledge
  base's hosts and its listings' pages. A per-visitor hourly cap (`VISITOR_REPLIES_PER_HOUR`,
  default 15) stops one visitor making a bot busy for everyone; refusals are `visitor_limit` or
  `bot_busy` (was `budget_exceeded`). A reply shown with a lead card points at the card instead of
  asking for details again.
- Text-only replies: a `LEAD: {…}` written on the same line as the reply (or pretty-printed, or
  in a code fence) is now parsed and never shown to the visitor. A parsed lead is the only card
  offered; the typed-number fallback card now takes the visitor's name ("I'm …", "my name is …",
  "this is …") and a message without their contact details.
- Text-only prompts no longer carry the tool-era lead wording ("call propose_lead … name and
  email"): a phone number or an email is enough, a typed phone number with a call-back request
  offers the card, and directory bots say the directory team follows up.
- Text-only fallback card: a bare call-back request next to a number ("call 07700 900123", "give
  me a ring", "can you call", "07700 900123, please call") now offers the card; a listed business's
  or the knowledge base's number, or one about someone else ("call them on …"), never does. A
  reference word ("job", "quote") only hides the number straight after it, and never a UK mobile.
  Call-back times ("after 4pm", "tomorrow morning") go in the card's best time, leftover greetings
  and connectors ("Hi", "and", "!") are dropped, and everyday words after "I'm" ("Retired",
  "Desperate") are no longer taken as names.
