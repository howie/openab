# HTTP Trigger

OpenAB can expose a small HTTP API for programmatic prompts without Discord, Slack, or a Custom Gateway.

Use this for local automation, CI jobs, webhooks routed through a trusted sidecar, or internal services that need to run an ACP agent turn.

## Config

```toml
[http]
enabled = true
bind = "127.0.0.1"
port = 7865
token = "${HTTP_TRIGGER_TOKEN}"
timeout_ms = 300000

[agent]
command = "kiro-cli"
args = ["acp", "--trust-all-tools"]
working_dir = "/home/agent"
```

`token` is required when HTTP is enabled. Keep `bind = "127.0.0.1"` unless another layer provides authentication, TLS, and network access control.

## Request

```bash
curl -sS http://127.0.0.1:7865/prompt \
  -H "Authorization: Bearer $HTTP_TRIGGER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"session_id":"ci-main","prompt":"summarize the latest failed test"}'
```

Response:

```json
{
  "response": "...",
  "session_reset": false
}
```

`session_id` controls conversational continuity. Reuse the same value to keep context, or use a unique value for one-shot automation. OpenAB stores HTTP sessions internally as `http:<session_id>`.

## Limits And Errors

| Condition | Status |
|-----------|--------|
| Missing or invalid bearer token | `401` |
| Empty `session_id` or `prompt` | `400` |
| `session_id` longer than 128 bytes | `400` |
| `prompt` longer than 64 KiB | `413` |
| Session pool unavailable | `503` |
| Prompt timeout | `504` |
| Agent/internal error | `500` |

On timeout, OpenAB resets that HTTP session before returning `504`, so late ACP notifications do not leak into the next request for the same `session_id`.

## Helm

```bash
helm install openab openab/openab \
  --set agents.kiro.discord.enabled=false \
  --set agents.kiro.http.enabled=true \
  --set-literal agents.kiro.http.token="$HTTP_TRIGGER_TOKEN"
```

Default Helm values bind the HTTP listener to `127.0.0.1`, which is reachable only inside the pod. For external access, prefer a sidecar or ingress that terminates TLS and enforces access control, then forward to `127.0.0.1:7865`.

If you intentionally bind to all pod interfaces:

```bash
helm upgrade openab openab/openab \
  --reuse-values \
  --set agents.kiro.http.bind=0.0.0.0
```

Do this only with Kubernetes NetworkPolicy, an authenticated ingress, or equivalent controls.
