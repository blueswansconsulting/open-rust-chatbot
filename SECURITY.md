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
| Other websites embedding your bot | Exact-origin allowlist checked on every WebSocket upgrade. Note: browsers enforce `Origin`, but scripts and bots can forge it, so the allowlist is not a cost control on its own. |
| Bots spending your model budget | Rate limits and caps below, `LLM_MAX_TOKENS`, and optional Cloudflare Turnstile (tokens are verified server-side, single-use, and must have been issued for one of your allowed hostnames) |
| Flooding / cost abuse | Per-IP (IPv6 per /64) and global connection caps, sliding-window message rate limit, message and frame size limits, turn cap per connection, idle timeout and absolute session lifetime, and sockets that keep sending rejected messages are closed |
| Slow-header (slowloris) connections | Run behind a reverse proxy (Caddy, nginx, Traefik, Cloudflare) that enforces header timeouts. The server binds to 127.0.0.1 by default for this reason. |
| Prompt injection making the bot act | The model only has read-only search and a lead *proposal*. Sending a lead requires the visitor to press confirm in the widget; the model cannot confirm. |
| One visitor reading or confirming another's data | All conversation and lead state lives on the visitor's own WebSocket connection; lead ids are random UUIDs |
| Hijacking someone's conversation | Resume tokens are random (244 bits from the OS RNG), single-use (each page gets a new one), expire after 30 minutes, live only in server memory, and are sent in the first WebSocket message rather than the URL so they don't reach access logs. The widget keeps them in per-tab `sessionStorage`. Expired chats are swept from memory within about 30 seconds of expiry, even with no traffic, (`RESUME_TTL_SECONDS`, default 30 minutes), and a restart forgets them all. |
| XSS through model output | The widget renders all text with `textContent`, never `innerHTML` |
| Leaking secrets or provider errors to visitors | Visitors see generic errors; details go to server logs. The API key is never sent to the browser. |
| PII in logs | Chat content is never logged; emails and phone numbers in log events are redacted |
| Forged lead webhooks | HMAC-SHA256 signature over timestamp and body (see docs/webhooks.md); the secret must be at least 32 characters |
| Spoofed client IPs | Proxy headers are ignored unless `TRUST_PROXY_HEADERS=true`, and then only the single header named by `CLIENT_IP_HEADER` is used |
| OpenClaw bridge abuse | The transcript is sent as JSON so visitors cannot forge turns. The bridge agent must have no tools; a public visitor must never reach an agent that can run commands. |
| Multi-site: one bot's widget used for another | Each bot's `/b/<id>/ws` checks the `Origin` against that bot's own `allowedOrigins` (exact origins only; `*`, paths and plain-http non-local origins are dropped). Resume tokens are bound to the bot they were issued on. |
| Multi-site: sites API and internal token | `INTERNAL_API_TOKEN` (at least 32 characters) is sent to the sites API and required, compared in constant time, on `/internal/…` routes. Keep `/internal/` reachable only from your private network (do not route it through your public proxy) and keep the sites API off the public internet. The sites API must never return secrets or leads; it is fetched with a 5 s timeout and a 1 MB response limit. |
| Multi-site: guessing bot ids | Ids must match `[a-z0-9-]{2,40}` before any lookup, unknown ids are cached as missing for 10 s, at most 16 lookups run at once, and a request waits at most 2 s for one (then `503`). Known bots are served from cache, and refreshed in the background, so a flood of unknown ids doesn't slow them down. Ids are not secret (they are in the install snippet); a bot is only usable from its allowed origins. Rate-limit `/b/` at your proxy if id-guessing floods are a concern. |
| Multi-site: cost abuse with a forged `Origin` | Optional per-bot `caps` (replies per hour and day), a daily ceiling across all of a partner's bots, and at most a tenth of a bot's daily cap per visitor. Counters are kept outside the bot cache and restored from the dashboard on startup. Caps assume one server process. |
| Multi-site: a bot owner's instructions removing the safety wording | A fixed set of safety rules is always appended after each bot's instructions. |
| Multi-site: replaying a signed lead against another bot | The bot id is inside the signed body (`siteId`); receivers check it matches `?site=`. |
| Multi-site: usage reports | Sent with a separate `USAGE_TOKEN` (it must differ from `INTERNAL_API_TOKEN`), and contain bot ids and counts only, never chat text. A failed batch is resent with the same `batchId`, so the receiver can store it exactly once. |
| Insecure deployment | The server refuses to start without an origin allowlist, with `*` or plain-http origins, or with mock/dev settings in production. The Docker image is distroless, non-root, and works with a read-only filesystem. |

## Out of scope

- Attacks that need control of a site listed in `ALLOWED_ORIGINS` (for example XSS on your own site).
- What your chosen model says. Review your system prompt and knowledge base; the bot is told never to invent facts, but models can still be wrong.

## Supply chain

`Cargo.lock` is committed; CI runs `cargo-deny` (advisories, licenses, sources), gitleaks, CodeQL and
OpenSSF Scorecard. GitHub Actions are pinned to commit SHAs and updated by Dependabot. Release
images are built with provenance and an SBOM.
