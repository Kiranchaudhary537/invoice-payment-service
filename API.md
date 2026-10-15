# API Reference

Base URL is `http://localhost:3000`. Every `/v1` route requires `Authorization: Bearer <api key>`. Bodies are JSON. Money is always integer cents (`*_cents`, USD). Ids are UUIDv7. Timestamps are RFC 3339 in UTC.

## Conventions

### Errors

Every error has the same shape. The response also carries an `X-Request-Id` header, which matches the server logs.

```json
{ "error": { "code": "invalid_state_transition", "message": "Cannot pay an invoice in status 'draft'", "details": { "from": "draft", "action": "pay" } } }
```

| HTTP | `code` | When |
|---|---|---|
| 400 | `validation_error` | Malformed JSON, unknown field (e.g. a client-sent `total_cents`), out-of-range value |
| 400 | `idempotency_key_required` | `POST /pay` without an `Idempotency-Key` |
| 401 | `unauthorized` | Missing, malformed, unknown or revoked API key |
| 402 | `card_declined`, `insufficient_funds`, `invalid_token`, `psp_unreachable` | The payment attempt failed. `details.payment_attempt` holds the attempt |
| 404 | `not_found` | Unknown id, or an id belonging to another business |
| 405 | `method_not_allowed` | The route exists but not with this HTTP method |
| 409 | `invalid_state_transition` | The action is not allowed from the invoice's current status |
| 409 | `invoice_already_paid` | `POST /pay` on a paid invoice |
| 409 | `payment_in_progress` | A pending attempt exists. Blocks pay, void and mark_uncollectible |
| 409 | `concurrent_modification` | Lost an optimistic-concurrency race. Safe to retry, even with the same key |
| 409 | `request_in_progress` | The same `Idempotency-Key` is still being processed |
| 409 | `customer_already_exists` | Email already used for this business. `details.customer_id` is the existing customer |
| 422 | `idempotency_key_reuse` | The same `Idempotency-Key` was sent with a different method, path or body |
| 422 | `amount_overflow`, `zero_total` | The total doesn't fit in 64 bits, or a $0 invoice was finalized |
| 500 | `internal_error` | Unexpected server error. Details are in the logs, not the response |

### Idempotency

Send an `Idempotency-Key` header (1–255 characters) to make a POST safe to retry.
- **Where:** required on `/pay`, and honoured on `POST /customers`, `POST /invoices` and the invoice actions.
- **Scope:** keys are scoped to the business and kept for 24 h.
- **Replay:** a repeat of the same request returns the stored response with the header `Idempotent-Replayed: true`.
- **Not stored:** errors whose answer can change aren't stored, so the same key can be retried: the transient 409s (`concurrent_modification`, `payment_in_progress`, `request_in_progress`) and `invalid_state_transition` (e.g. paying a draft works once it is finalized). Permanent answers such as `invoice_already_paid` are stored and replayed.
- **Not supported:** `POST /api_keys` and `POST /webhook_endpoints` ignore the header, because storing their response would persist a secret.

### Pagination

The customer, invoice and event lists take `?limit=` (1–100, default 20) and `?cursor=` (the `next_cursor` from the previous page). They return `{ "data": [...], "next_cursor": "<id>" | null }`. Customers and invoices are listed newest first; events oldest first.

## Customers

**`POST /v1/customers`**: request `{ "name": "Ada Lovelace", "email": "ada@example.com" }`, response **201**:
```json
{ "id": "01a1…", "name": "Ada Lovelace", "email": "ada@example.com", "created_at": "2026-10-14T10:31:08.345Z" }
```
The email is trimmed, lower-cased and unique per business.

**`GET /v1/customers/{id}`**: **200** with a customer.

**`GET /v1/customers`**: **200** with a page of customers.

## Invoices

**`POST /v1/invoices`** creates a **draft**. The server computes the total, and a client-supplied total is rejected.
```json
{ "customer_id": "01a1…", "due_date": "2026-12-31T00:00:00Z",
  "line_items": [ { "description": "Consulting", "quantity": 3, "unit_amount_cents": 15000 } ] }
```
Validation:
- 1–100 line items.
- `quantity` is an integer from 1 to 10,000.
- `unit_amount_cents` is an integer of at least 0.
- `due_date` is in the future.

Response **201**:
```json
{ "id": "01a1…", "customer_id": "01a1…", "status": "draft", "currency": "USD", "total_cents": 45000,
  "due_date": "2026-12-31T00:00:00Z", "is_overdue": false,
  "line_items": [ { "description": "Consulting", "quantity": 3, "unit_amount_cents": 15000, "amount_cents": 45000 } ],
  "created_at": "…", "finalized_at": null, "paid_at": null, "voided_at": null, "marked_uncollectible_at": null }
```

**`GET /v1/invoices/{id}`**: **200** with the invoice plus `payment_attempts` (see below). Poll this to learn the result of a `202` payment.

**`GET /v1/invoices?status=open`**: **200** with a page of invoice summaries, without line items or attempts. `status` is optional and must be one of `draft|open|paid|void|uncollectible`.

### Invoice actions (no request body): **200** with the updated invoice

| Endpoint | Transition | Notes |
|---|---|---|
| `POST /v1/invoices/{id}/finalize` | draft → open | Emits `invoice.created`. Rejects a $0 total (422 `zero_total`) |
| `POST /v1/invoices/{id}/void` | draft / open / uncollectible → void | 409 `payment_in_progress` while an attempt is pending |
| `POST /v1/invoices/{id}/mark_uncollectible` | open → uncollectible | 409 `payment_in_progress` while an attempt is pending |

Any other combination returns 409 `invalid_state_transition`. `paid` and `void` are terminal.

### `POST /v1/invoices/{id}/pay`

Headers: `Idempotency-Key` (required). Body: `{ "card_token": "tok_success" }`.

| HTTP | Meaning | Invoice afterwards |
|---|---|---|
| **201** | Charged | `paid` |
| **402** | PSP declined (`details.payment_attempt.failure_code`) | `open`, retry with a new key |
| **202** | Outcome unknown (PSP timed out or errored). Attempt is `pending` | `open`. Resolved by the reconciler, then delivered by webhook |

Response for 201 or 202:
```json
{ "invoice_status": "paid",
  "payment_attempt": { "id": "01a1…", "invoice_id": "01a1…", "amount_cents": 47599, "status": "succeeded",
                       "psp_ref": "01a1…", "failure_code": null, "last_error": null,
                       "created_at": "…", "completed_at": "…" } }
```
`payment_attempt.status` is `pending`, `succeeded` or `failed`. On a 202, `last_error` explains why the outcome is unknown (`psp_timeout`, `psp_http_500`, …).

## Webhooks

**`POST /v1/webhook_endpoints`**: request `{ "url": "https://example.com/hooks" }`, response **201** `{ "id", "url", "enabled", "created_at", "secret": "whsec_…" }`. The secret is shown only here.

**`GET /v1/webhook_endpoints`**: lists endpoints, without secrets.

**`DELETE /v1/webhook_endpoints/{id}`**: disables the endpoint and stops its pending deliveries.

**Delivery.** Each event is sent as `POST <url>`:
```json
{ "id": "<event id>", "type": "invoice.paid", "created_at": "…", "data": { "invoice": { … }, "payment_attempt": { … } } }
```
Event types:
- `invoice.created`, sent on finalize, with `data.invoice`.
- `invoice.paid`, with `data.invoice` and `data.payment_attempt`.
- `invoice.payment_failed`, with `data.invoice` and `data.payment_attempt`.

Headers:
- `Webhook-Id`: the event id. Delivery is at-least-once, so receivers should de-duplicate on it.
- `Webhook-Signature: t=<unix>,v1=<hex>`, where `v1 = HMAC-SHA256(secret, "<t>.<raw body>")`.

To verify, recompute the signature over the raw bytes, compare in constant time, and reject if `|now − t| > 300 s`.

**Retries.** Any non-2xx response or timeout (5 s) is retried after 10 s, 1 m, 5 m, 30 m, 2 h and 6 h (±20% jitter), for 7 attempts in total. After that the delivery is marked `failed`.

**`GET /v1/events?cursor=<last processed event id>`**: every event oldest first, each with its `deliveries` (`status`, `attempt_count`, `next_attempt_at`, `last_response_code`, `last_error`). Use it to reconcile missed webhooks.

## API keys

**`POST /v1/api_keys`**: **201** `{ "id", "prefix", "created_at", "last_used_at", "revoked_at", "key": "dodo_<prefix>_<secret>" }`. The key is shown only here.

**`GET /v1/api_keys`**: lists keys (prefix and usage only).

**`DELETE /v1/api_keys/{id}`**: revokes the key immediately. Its next request gets 401.

## Health

**`GET /health`**: **200** `{ "status": "ok" }` once the database is reachable. No auth.

## Mock PSP (port 4000)

| Route | Purpose |
|---|---|
| `POST /charges` `{reference, amount_cents, token}` | Outcome is decided by the token: `tok_success`, `tok_insufficient_funds`, `tok_card_declined`, `tok_timeout` (recorded as `processing`, succeeds after 30 s), `tok_network_error` (500, nothing recorded; the API treats a dropped connection the same way). Any other token fails with `invalid_token`. A repeated `reference` returns the existing charge |
| `GET /charges/{reference}` | Status lookup used by the reconciler. 404 if the charge was never received |
| `POST /webhook-sink` | Demo receiver: verifies the signature with `WEBHOOK_SINK_SECRET`. Returns 400 if verification fails |
| `GET /webhook-sink` | Webhooks received so far, newest first |
