# Lead webhooks

When a visitor presses **Confirm and send**, the server POSTs to `LEAD_WEBHOOK_URL`:

```http
POST /your/endpoint
Content-Type: application/json
X-ORC-Timestamp: 1760000000
X-ORC-Signature: sha256=<64 hex characters>

{"type":"lead","bot":"Sparky","lead":{"leadId":"…","name":"Sam","email":"sam@example.com","phone":"07700 900123","bestTime":"after 5pm","message":"Need a rewire quote"},"submittedAt":"2026-10-18T12:00:00.000Z"}
```

Every lead has a `name` and at least one of `phone` or `email`; `email`, `phone`, `bestTime` (when the
visitor would like a call back, in their words) and `message` are omitted when empty. Any 2xx response counts as delivered. 429 and 5xx
responses and network errors are retried twice; other 4xx responses are not retried.

**Deduplicate on `leadId`.** If a delivery times out after your endpoint received it, the visitor may
confirm again and you will get the same lead twice.

**Treat every field as untrusted visitor input.** Escape it before showing it in HTML or email, and
never use it in a query or command without parameterising it.

## Verifying the signature

Compute `HMAC-SHA256(LEAD_WEBHOOK_SECRET, "<X-ORC-Timestamp>.<raw body>")`, hex-encode it, compare it to
the header in constant time, and reject timestamps more than 5 minutes old. Always use the **raw**
request body, not re-serialised JSON.

### Node.js

```js
import crypto from 'node:crypto';

export function verify(rawBody, timestamp, signature, secret) {
  if (Math.abs(Date.now() / 1000 - Number(timestamp)) > 300) return false;
  const expected = 'sha256=' + crypto.createHmac('sha256', secret).update(`${timestamp}.${rawBody}`).digest('hex');
  const a = Buffer.from(expected);
  const b = Buffer.from(String(signature));
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}
```

### Python

```python
import hashlib, hmac, time

def verify(raw_body: bytes, timestamp: str, signature: str, secret: str) -> bool:
    try:
        if abs(time.time() - int(timestamp)) > 300:
            return False
    except (TypeError, ValueError):
        return False
    mac = hmac.new(secret.encode(), timestamp.encode() + b"." + raw_body, hashlib.sha256)
    return hmac.compare_digest("sha256=" + mac.hexdigest(), signature)
```

### Go

```go
func verify(rawBody []byte, timestamp, signature, secret string) bool {
	ts, err := strconv.ParseInt(timestamp, 10, 64)
	if err != nil || math.Abs(float64(time.Now().Unix()-ts)) > 300 {
		return false
	}
	mac := hmac.New(sha256.New, []byte(secret))
	mac.Write([]byte(timestamp + "."))
	mac.Write(rawBody)
	expected := "sha256=" + hex.EncodeToString(mac.Sum(nil))
	return hmac.Equal([]byte(expected), []byte(signature))
}
```
