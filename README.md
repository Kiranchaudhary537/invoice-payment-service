# Invoice & Payment Service

A minimal invoicing backend. Businesses create customers and invoices, customers pay invoices through a payment processor, and businesses get signed webhooks. It's written in Rust (Axum, sqlx, Postgres).

| Document | What's in it |
|---|---|
| [DESIGN.md](DESIGN.md) | Architecture, data model, state machine, payment failure modes, webhooks, API keys, cuts, production gaps |
| [API.md](API.md) | Every endpoint, request and response shapes, error format |
| [AI_USAGE.md](AI_USAGE.md) | How AI was used. [ai_log.md](ai_log.md) has the decision-by-decision record |

## Run it

```bash
docker compose up --build
```

No other steps are needed. This starts:

| Service | Port | What |
|---|---|---|
| `api` | 3000 | Invoice API. Runs migrations on start, plus the webhook worker, payment reconciler and idempotency purge |
| `mock-psp` | 4000 | Mock payment processor and a webhook sink that verifies signatures |
| `db` | 5433 | Postgres 16 (host port 5433, to avoid clashing with a local Postgres) |

On startup the API creates a **demo business** with a fixed API key and registers the mock PSP's `/webhook-sink` as its webhook endpoint. The key is demo-only; see DESIGN.md §5.

```bash
export KEY=dodo_demo0001_LocalDemoKeyDoNotUseInProduction
```

## Try it

**1. Create a customer**
```bash
curl -s -X POST localhost:3000/v1/customers \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"name": "Ada Lovelace", "email": "ada@example.com"}'
# copy "id" into CUSTOMER_ID
```

**2. Create an invoice, then finalize it.** It is created as `draft`; finalizing moves it to `open` and sends `invoice.created`.
```bash
curl -s -X POST localhost:3000/v1/invoices \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"customer_id": "'$CUSTOMER_ID'", "due_date": "2030-01-01T00:00:00Z",
       "line_items": [{"description": "Consulting", "quantity": 3, "unit_amount_cents": 15000},
                      {"description": "Expenses",   "quantity": 1, "unit_amount_cents": 2599}]}'
# total_cents is computed by the server: 47599. Copy "id" into INVOICE_ID

curl -s -X POST localhost:3000/v1/invoices/$INVOICE_ID/finalize -H "Authorization: Bearer $KEY"
```

**3. A failed payment.** This returns 402 `card_declined`; the invoice stays `open` and `invoice.payment_failed` is sent.
```bash
curl -s -X POST localhost:3000/v1/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: pay-attempt-1' \
  -d '{"card_token": "tok_card_declined"}'
```

**4. A successful payment.** This returns 201; the invoice becomes `paid` and `invoice.paid` is sent. Repeating the same command replays the same response without charging again.
```bash
curl -s -X POST localhost:3000/v1/invoices/$INVOICE_ID/pay \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: pay-attempt-2' \
  -d '{"card_token": "tok_success"}'
```

**See the webhooks.** The sink verifies each signature before accepting the delivery.
```bash
curl -s localhost:4000/webhook-sink                                   # what the receiver got
curl -s localhost:3000/v1/events -H "Authorization: Bearer $KEY"      # events + delivery status
docker compose logs -f mock-psp                                       # "webhook received, signature verified"
```

**Slow PSP.** On a fresh `open` invoice, `"card_token": "tok_timeout"` returns **202** after 5 s with a `pending` attempt instead of hanging. About 30 s later the reconciler finds out from the PSP that the charge succeeded, marks the invoice `paid` and sends `invoice.paid`. `tok_network_error` takes the same path but resolves to `failed` (`psp_unreachable`), and the invoice stays payable.

**Everything at once.** With the stack running, [demo.sh](demo.sh) runs every flow above, plus idempotency edge cases, a 20-way payment race, `tok_timeout` and `tok_network_error`, and prints PASS/FAIL per check (needs `jq`).
```bash
./demo.sh            # ~1 min, waits for the slow-PSP cases to settle
FAST=1 ./demo.sh     # ~5 s, skips them
```

## Tests

The tests are in [tests/payments.rs](tests/payments.rs): the three the assignment requires, plus a second PSP-failure test for `tok_network_error`.

| Test | Asserts |
|---|---|
| `concurrent_payments_charge_exactly_once` | 20 parallel `/pay` calls on one invoice give one 201 and 19 × 409, exactly one PSP charge, and a `paid` invoice |
| `retried_payment_with_same_key_replays_without_second_charge` | The same key replays an identical response with no second PSP call. The same key with a different body gets 422 |
| `psp_timeout_leaves_invoice_consistent_and_reconciles` | `tok_timeout` returns 202 within the client timeout and the invoice stays `open`. A second payment gets 409 `payment_in_progress`. The reconciler then marks the invoice `paid` with exactly one charge |
| `psp_network_error_fails_attempt_and_leaves_invoice_payable` | `tok_network_error` returns 202 with a `pending` attempt. The reconciler finds no charge at the PSP, fails the attempt (`psp_unreachable`), and the invoice stays `open` and can be paid with another card |

Each test starts the API and the mock PSP in-process against a fresh Postgres database, so the mock's charge counter can be asserted directly. The PSP-failure tests shorten the delays (the client gives up at 1 s, and the PSP settles `tok_timeout` at 3 s) so they run in seconds.

```bash
docker compose up -d db
cargo test            # Rust 1.86+; connects to postgres://dodo:dodo@localhost:5433 (override with TEST_DATABASE_URL)
```

Each test creates its own `dodo_test_*` database; `docker compose down -v` removes them. There are no per-handler tests, as the assignment suggests.

## Configuration

All settings are environment variables. The defaults match DESIGN.md.

| Variable | Default | |
|---|---|---|
| `PSP_TIMEOUT_MS` | 5000 | Client timeout for each PSP call |
| `RECONCILE_INTERVAL_MS` / `RECONCILE_MIN_AGE_MS` | 5000 / 10000 | How often the reconciler runs, and how old a pending attempt must be before it's checked. The API refuses to start unless the min age exceeds `PSP_TIMEOUT_MS` |
| `WEBHOOK_RETRY_DELAYS_SECS` | `10,60,300,1800,7200,21600` | Retry schedule; max attempts is the number of delays + 1 |
| `IDEMPOTENCY_TTL_SECS` / `IDEMPOTENCY_PURGE_INTERVAL_SECS` | 86400 / 3600 | Key lifetime and how often expired keys are purged |
| `MOCK_LATENCY_MS` / `MOCK_TIMEOUT_DELAY_MS` | 100 / 30000 | Mock PSP response latency, and how long `tok_timeout` takes |

## Layout

```
src/payments.rs       POST /pay: optimistic-concurrency reservation, PSP call, finish_attempt
src/reconciler.rs     resolves pending attempts by asking the PSP
src/state_machine.rs  the invoice transition table
src/idempotency.rs    Idempotency-Key claim / replay / TTL purge
src/webhooks.rs       endpoints API + delivery worker (outbox, signing, retries)
src/mock_psp.rs       mock processor + webhook sink (binary: mock-psp)
migrations/           schema
```

## Demo Video

**[Watch the walkthrough on Google Drive](https://drive.google.com/drive/folders/1h3PuWSDlAKTZ4ulHQC_Gmk3eGbA1dC-W?usp=sharing)** (no login needed)

In order, it covers:
1. **Architecture overview**: the services, the data model, and how a request flows from the API to the database to webhook delivery.
2. **Live demo**: `docker compose up`, create a customer and an invoice, a successful payment, a `tok_card_declined` payment, and the resulting webhook deliveries.
3. **State machine walkthrough**: the invoice statuses, the allowed transitions, the terminal states, and where I deliberated.
4. **Failure-mode walkthrough**: `tok_timeout`, traced through `src/payments.rs` and the reconciler.
