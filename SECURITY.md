# Security policy

## Reporting a vulnerability

Please **do not** open a public issue. Report it privately through
[GitHub Security Advisories](https://github.com/blueswansconsulting/open-rust-chatbot/security/advisories/new).
We aim to acknowledge reports within 3 working days and to ship a fix or mitigation for confirmed
high-severity issues within 30 days. We're happy to credit you in the advisory.

Only the latest release is supported with security fixes.

## Threat model

open-rust-chatbot runs on your server and is embedded on websites you list in `ALLOWED_ORIGINS`.
Anyone on the internet can open the widget, so every visitor is treated as untrusted.

| Threat | Mitigation |
| --- | --- |
| Other sites embedding your bot and spending your model budget | Exact-origin allowlist checked on every WebSocket upgrade; optional Cloudflare Turnstile |
| Flooding / cost abuse | Per-IP and global connection caps, sliding-window message rate limit, message and frame size limits, turn cap per connection, idle timeout, `LLM_MAX_TOKENS` |
| Prompt injection making the bot act | The model only has read-only search and a lead *proposal*. Sending a lead requires the visitor to press confirm in the widget; the model cannot confirm. |
| One visitor reading or confirming another's data | All conversation and lead state lives on the visitor's own WebSocket connection; lead ids are random UUIDs |
| XSS through model output | The widget renders all text with `textContent`, never `innerHTML` |
| Leaking secrets or provider errors to visitors | Visitors see generic errors; details go to server logs. The API key is never sent to the browser. |
| PII in logs | Chat content is never logged; emails and phone numbers in log events are redacted |
| Forged lead webhooks | HMAC-SHA256 signature over timestamp and body (see docs/webhooks.md); the secret must be at least 32 characters |
| Spoofed client IPs | `X-Forwarded-For` is ignored unless `TRUST_PROXY_HEADERS=true` |
| Insecure deployment | The server refuses to start without an origin allowlist, with `*` or plain-http origins, or with mock/dev settings in production. The Docker image is distroless, non-root, and works with a read-only filesystem. |

## Out of scope

- Attacks that need control of a site listed in `ALLOWED_ORIGINS` (for example XSS on your own site).
- What your chosen model says. Review your system prompt and knowledge base; the bot is told never to invent facts, but models can still be wrong.

## Supply chain

`Cargo.lock` is committed; CI runs `cargo-deny` (advisories, licenses, sources), gitleaks, CodeQL and
OpenSSF Scorecard. GitHub Actions are pinned to commit SHAs and updated by Dependabot. Release
images are built with provenance and an SBOM.
