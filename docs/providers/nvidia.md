# NVIDIA (build.nvidia.com / NIM)

NVIDIA hosts open models behind an OpenAI-compatible API at `https://integrate.api.nvidia.com/v1`.
Create a key at [build.nvidia.com](https://build.nvidia.com/settings/api-keys).

## Settings

| Variable | Default | Notes |
| --- | --- | --- |
| `LLM_PROVIDER` | — | Set to `nvidia`. |
| `NVIDIA_API_KEY` | — | Required. Sent as a bearer token; never logged or shown in debug output. |
| `NVIDIA_MODEL` | `nvidia/nemotron-3-nano-30b-a3b` | Any model id from build.nvidia.com. |
| `NVIDIA_BASE_URL` | `https://integrate.api.nvidia.com/v1` | A self-hosted NIM. Must be https; plain http is accepted only for `localhost` or `127.0.0.1`. |
| `NVIDIA_TEMPERATURE` | `0.3` | 0 to 2. |
| `NVIDIA_ENABLE_THINKING` | `false` | Sent as `chat_template_kwargs.enable_thinking`. |
| `NVIDIA_TOOLS` | `false` | `true` offers the model tools (knowledge-base search, lead cards, directory search). |
| `LLM_MAX_TOKENS` | `400` | Shared with the generic provider. |

`LLM_API_KEY`, `LLM_BASE_URL` and `LLM_MODEL` are ignored when `LLM_PROVIDER=nvidia`.

## Reasoning output

Nemotron and several other NVIDIA-hosted models are reasoning models. The server asks for thinking
to be off (`"chat_template_kwargs": {"enable_thinking": false}`), and as a second guard removes any
`<think>…</think>` block from the streamed reply before it reaches the visitor or the conversation
history, even when the tags are split across stream chunks. An unterminated `<think>` block is
dropped. Reasoning sent in a separate `reasoning_content` field is ignored.

## Tool calling

NVIDIA runs **text only** by default, like the OpenClaw bridge: the server retrieves knowledge-base
excerpts (and directory listings) itself, puts them in the prompt, and offers the lead card from the
visitor's own message. This works with every model and needs no tool-calling support.

Nemotron 3 Nano is documented as supporting tool calls, but tool calls through the hosted streaming
API have not been verified for this project, and a model that fumbles a tool call leaves the
visitor with no answer. Set `NVIDIA_TOOLS=true` only after checking the model you chose with a few
real conversations (questions that need the knowledge base, and a visitor leaving their details).

## Errors and retries

| Response | What happens |
| --- | --- |
| 401 or 403 | Not retried. Logged as `auth failed`; counts as an outage for `/availability` (fix the key or its entitlements). |
| 429 | Not retried (the free tier's quota is spent for now; retrying burns more). Counts as an outage. |
| 5xx, connection error, timeout | Retried once, before any text is streamed. Counts as an outage if the retry fails too. |
| Other 4xx (bad model id, bad request) | Not retried; not an outage. |

Visitors only ever see the generic error message. Usage reporting and per-bot caps work exactly as
for other providers (they count replies, not provider tokens).

## Switching a deployment

```sh
LLM_PROVIDER=nvidia
NVIDIA_API_KEY=nvapi-...           # from build.nvidia.com
# optional:
# NVIDIA_MODEL=nvidia/nemotron-3-nano-30b-a3b
```

Restart the server. A missing key, a non-https base URL or an unknown `LLM_PROVIDER` stops it at
startup with a clear message.
