# open-rust-chatbot

**Add a self-hosted AI chatbot to any website with one `<script>` tag.**
One small Rust binary, any OpenAI-compatible model, your data stays on your server.

[![CI](https://github.com/blueswansconsulting/open-rust-chatbot/actions/workflows/ci.yml/badge.svg)](https://github.com/blueswansconsulting/open-rust-chatbot/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/blueswansconsulting/open-rust-chatbot/badge)](https://scorecard.dev/viewer/?uri=github.com/blueswansconsulting/open-rust-chatbot)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Live demo:** [electriciansranked.com](https://electriciansranked.com) (bottom-right bubble)

## Why

- **Your model, your data.** Works with OpenAI, OpenRouter, Groq, or a local Ollama. No per-seat SaaS fees, no chat logs on someone else's servers.
- **Tiny and fast.** One ~6 MB binary, single-digit MB of RAM at idle, and a widget that's about 4 KB gzipped (JS + CSS), so it won't slow your site down.
- **Safe by default.** Origin allowlist, rate limits, size limits, no chat content in logs, and the model can never send a visitor's details without the visitor pressing confirm.

## Quickstart (60 seconds)

Try it locally with the built-in mock model (no API key needed):

```sh
git clone https://github.com/blueswansconsulting/open-rust-chatbot
cd open-rust-chatbot
DEV_MODE=true MOCK_LLM=true KNOWLEDGE_BASE_PATH=examples/knowledge-base.md cargo run
# open http://127.0.0.1:8787 and click the bubble
```

Run it for real with Docker:

```sh
cp .env.example .env      # set ALLOWED_ORIGINS and LLM_API_KEY
docker compose up -d
```

Then add this to your website, just before `</body>`:

```html
<script src="https://chat.your-domain.com/widget.js" defer></script>
```

## Features

- **Knowledge base from Markdown.** Write `# Headings` and paragraphs about your business; the bot searches it before answering and is told never to guess.
- **Streaming replies** over WebSockets, rendered safely (`textContent` only, never `innerHTML`).
- **Lead capture with visitor consent.** The bot proposes a card with the visitor's details; nothing is sent until they press *Confirm and send*. Confirmed leads go to your webhook, signed with HMAC-SHA256. Without a webhook, lead capture is switched off so leads are never silently lost.
- **Any OpenAI-compatible model,** plus an [OpenClaw](https://github.com/openclaw/openclaw) gateway bridge.
- **Abuse protection:** exact-origin allowlist, per-IP and global connection caps, message rate limits, message size limits, idle timeouts, optional Cloudflare Turnstile.
- **Themeable:** override the `.orc-*` CSS classes, or set title, subtitle, greeting and placeholder text.

## Model providers

| Provider | `LLM_BASE_URL` | `LLM_MODEL` example | Key |
| --- | --- | --- | --- |
| OpenAI | `https://api.openai.com/v1` | `gpt-4o-mini` | `LLM_API_KEY` |
| OpenRouter | `https://openrouter.ai/api/v1` | `openai/gpt-4o-mini` | `LLM_API_KEY` |
| Groq | `https://api.groq.com/openai/v1` | `llama-3.3-70b-versatile` | `LLM_API_KEY` |
| Ollama (local) | `http://localhost:11434/v1` | `llama3.1` | not needed |
| OpenClaw bridge | set `LLM_PROVIDER=openclaw` | — | `OPENCLAW_BRIDGE_TOKEN` |

Models must support tool calling for knowledge-base search and lead cards. With the OpenClaw bridge (text only), the server retrieves knowledge-base excerpts itself and offers the lead card when a visitor types an email address. **The bridge agent must have no tools**, because every visitor is untrusted.

### Deploying

Run it behind a TLS reverse proxy (Caddy, nginx, Traefik or Cloudflare). The proxy terminates HTTPS and
enforces header timeouts. Set `TRUST_PROXY_HEADERS=true` so rate limits use the real visitor IP, and
`CLIENT_IP_HEADER=cf-connecting-ip` if Cloudflare is in front.

If your site sends a Content-Security-Policy, allow the chatbot's origin in `script-src`, `style-src`
and `connect-src` (as `wss://chat.your-domain.com`).

## Customising the widget

```html
<script>
  window.openRustChatbot = {
    title: 'Ask Sparky',
    subtitle: 'Usually replies instantly',
    greeting: 'Hi! Need an electrician? Ask me anything.',
    placeholder: 'Type your question...',
    // serverUrl: 'https://chat.your-domain.com',   // only if the script is served from elsewhere
    // turnstileToken: () => getFreshTurnstileToken(), // if TURNSTILE_SECRET_KEY is set (tokens are single-use)
  };
</script>
<script src="https://chat.your-domain.com/widget.js" defer></script>
```

## Knowledge base

Point `KNOWLEDGE_BASE_PATH` at a Markdown file. Each heading starts a section; long sections are split into overlapping chunks. See [`examples/knowledge-base.md`](examples/knowledge-base.md). Restart the server after editing it.

## Lead webhooks

Confirmed leads are POSTed as JSON to `LEAD_WEBHOOK_URL` with `X-ORC-Timestamp` and `X-ORC-Signature` headers. Verification examples for Node, Python and Go are in [docs/webhooks.md](docs/webhooks.md). Failed deliveries are retried twice, and the visitor is asked to try again if delivery still fails.

## Configuration

Every setting is an environment variable, documented in [`.env.example`](.env.example). The server refuses to start with an unsafe configuration (for example no `ALLOWED_ORIGINS`, a `*` origin, plain-http origins, or a weak webhook secret) and tells you exactly what to fix.

## Security

Please read [SECURITY.md](SECURITY.md) for the threat model and how to report a vulnerability privately. In short:

- The API key never leaves the server; the browser only talks to your server.
- Visitor text is treated as untrusted. The model has two narrow tools (read-only search and a lead *proposal*) and cannot trigger anything a visitor hasn't confirmed.
- Conversation state lives on the WebSocket connection, so one visitor can never see or act on another visitor's data.
- Logs contain events, not chat content, and email addresses and phone numbers are redacted.

## How it compares

| | open-rust-chatbot | Hosted chatbot SaaS | Larger self-hosted platforms |
| --- | --- | --- | --- |
| Hosting | Your server, one binary | Vendor | Your server, several services |
| Model | Any OpenAI-compatible, or local | Vendor's choice | Varies |
| Footprint | ~6 MB binary, ~4 KB widget | n/a | Databases, workers, admin UI |
| Admin UI, inbox, analytics | No (by design, for now) | Yes | Yes |
| Price | Free (MIT) + your model costs | Per seat / per message | Free or paid tiers |

If you need a full support inbox, use a bigger platform. If you want a small, auditable bot you can drop on a site in minutes, this is for you.

## Roadmap

- Prebuilt binaries and a multi-arch (amd64 + arm64) image on every release
- Multiple knowledge-base files and hot reload
- Optional conversation transcripts to a webhook
- Widget translations

Ideas welcome in [Discussions](https://github.com/blueswansconsulting/open-rust-chatbot/discussions).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues labelled `good first issue` are a great place to start. `cargo test` runs the whole suite, including end-to-end WebSocket tests, in a few seconds.

## License

[MIT](LICENSE). Built by [Tradesites.ai](https://chat.tradesites.ai).
