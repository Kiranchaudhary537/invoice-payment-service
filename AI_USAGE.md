# AI Usage

## Summary

- **The code is 100% AI-generated.** Claude Code wrote every line: the Rust service, the mock PSP, the SQL migrations, the Dockerfile and compose file, and the tests. I didn't hand-write code.
- **The design and architecture are mine.** I wrote them by hand in [`raw_design.md`](raw_design.md), in two iterations, before using any AI. That includes the data model, indexes, idempotency approach, API-key model, the outbox tables and the 100x plan.
- **The AI's design role was reviewer, not author.** Before any code was written, I had Claude interview me about my design. It flagged gaps and inconsistencies and, where I had an open question, offered options. I made every decision, and I overrode it where I disagreed. [`ai_log.md`](ai_log.md) records each decision with its origin.
- **The AI also drafted the documents** (`DESIGN.md`, `API.md`, `README.md` and this file) from my decisions, and I edited them.

**Tool:** Claude Code (Claude Opus 5.5) in VS Code. I didn't use Copilot, Cursor, ChatGPT or any other AI tool.

## 1. How I worked with AI

| Step | Who | What happened |
|---|---|---|
| 1. Design | Me, no AI | Wrote `raw_design.md`: iteration 1 (entities, endpoints, first state machine, webhook ideas), then iteration 2 (full schema, indexes, idempotency trade-off, 100x plan). |
| 2. Design review | Me deciding; AI questioning | Four rounds of "grill me": it asked one round of questions at a time, I answered or decided, and each decision went into `ai_log.md`. |
| 3. Implementation | AI | Claude Code wrote all the code from the agreed design. |
| 4. Verification | AI ran it, I checked it | Tests, repeated runs, live `docker compose` checks, and a cross-check of the docs against the code (§5). |
| 5. Documentation | AI drafted, I edited | Docs were drafted from `ai_log.md` and my raw design, then checked against the code. |

**Problems the review caught in my raw design.** None of these were new design ideas; they were gaps in mine:
- My iteration-1 states (`draft failed`, `failed`, `overdue`) didn't match my iteration-2 enum.
- I wrote "queue" in one place but designed outbox tables in another.
- I had no transaction boundaries for `/pay`.
- There was no way to bootstrap the first API key.

## 2. Where each design decision came from

**Mine, in `raw_design.md` before any AI:**
- `status` (not `state`) and the five invoice statuses.
- `business_id` on every table, as the tenant scope and future shard key, plus the composite FK that blocks cross-tenant invoices.
- Partial unique indexes: one succeeded and one in-flight attempt per invoice.
- The attempt id doubling as the PSP reference key, and an index for a reconciler job.
- A separate `idempotency_keys` table (Approach B over Approach A), with a 24 h TTL and a purge job.
- API keys: prefix lookup, SHA-256 rather than bcrypt, constant-time compare, multiple active keys for rotation.
- A readable (not hashed) webhook secret, because the HMAC needs the raw value.
- `events` + `webhook_deliveries` tables, retries and failure notification.
- The 100x plan: jsonb line items, monthly partitioning, keyset pagination, read replicas, sharding by `business_id`.
- The cut list (queue + DLQ, overdue and dunning, failure notifications, secret encryption, leaked-key scanning).

**Open questions in my notes, resolved by choosing from options the AI offered:**
- **ID type:** my note said "not UUIDv4, something ascending". It offered UUIDv7, and I accepted.
- **DB layer:** sqlx with plain SQL, so the conditional updates stay visible.
- **Webhook worker:** in-process. Mock PSP: a separate binary with `GET /charges/{ref}` and a webhook sink.
- **Concrete numbers:** 5 s PSP timeout, 202 for an unknown outcome, 402 for a decline, the retry schedule.
- **Dropping `failed`/`overdue`/`processing` invoice states:** I asked it to check whether they were needed; it concluded failure belongs to the attempt and overdue is derived, and I agreed.

## 3. Three decisions I made against or independent of the AI

**1. Optimistic concurrency instead of `SELECT … FOR UPDATE`.**
- **AI proposed:** lock the invoice row with `FOR UPDATE` in the first transaction.
- **I chose:** a version compare-and-swap on `invoices`.
- **Why:** under contention the losers should fail fast with a 409, not queue behind a lock, and nothing should be held while we wait on a slow PSP, or connections pile up. Once I'd decided, the AI helped find the rule that makes it correct: *every* invoice mutation (pay, void, write-off) must go through the same version check, or `/pay` and `/void` could race and the PSP could charge a voided invoice.

**2. `invoice.created` fires on finalize, not on draft creation.**
- **AI proposed:** emit it on `POST /invoices`.
- **I chose:** emit it on the `draft → open` transition.
- **Why:** a draft isn't visible to the customer and can't be paid. The event should mark the moment the customer can act on the invoice, not an internal edit.

**3. A separate line-items table now, with jsonb kept as the 100x option.**
- **Background:** my own raw design used `jsonb` on `invoices`, and the AI recommended keeping it.
- **I chose:** an `invoice_line_items` table, and moved jsonb into the 100x plan.
- **Why:** my raw design already noted the trade-off: "use a separate table if you want per-row constraints now". For a payments service at this size, I'd rather Postgres enforce `quantity > 0` and `amount_cents = quantity * unit_amount_cents` per row than trust application code. The jsonb join saving only matters at a scale we don't have yet.

**Smaller overrides:**
- The key prefix is `dodo_`, not the AI's `sk_`.
- DESIGN.md must explain why there's no message queue and how production rate limiting would work.

## 4. What the AI got wrong, and what I corrected

**It dropped the idempotency purge.**
- **AI proposed:** no purge job; treat expired rows as absent on read. The table would then grow forever.
- **My correction:** a real TTL with an hourly batched purge, as in my raw design.
- **The follow-up it missed:** once a key is purged, a late duplicate looks like a new request, so the system must stay safe without the key. I raised this. The answer is that domain invariants carry the money guarantee: the paid check plus `unique(invoice_id) WHERE status='succeeded'`, the unique customer email, and a duplicate invoice is at worst an unpayable draft.

**Mistakes found by cross-checking its docs against its code:**
- DESIGN.md said the reconciler claims attempts with `SKIP LOCKED`. The code doesn't need a lock, because `finish_attempt` only updates `WHERE status='pending'`. The doc was fixed.
- DESIGN.md said pagination keys on `(created_at, id)`. The code keys on the UUIDv7 `id`. The doc was fixed.
- The test API key was 31 characters instead of 32, so it would have failed format validation. A length check caught it.
- It wrote "no DB locks" for the optimistic scheme. That isn't literally true, because an `UPDATE` holds a row lock until commit. The wording became "nothing held across the PSP call; losers fail fast".

## 5. How correctness was verified

Because the AI wrote the code, I didn't trust it without evidence:
- **Tests** (`tests/payments.rs`) run against real Postgres and the real mock PSP: the three required ones, which passed 5 runs out of 5, plus a fourth for `tok_network_error`.
  - **Concurrency:** 20 parallel `/pay` calls gave exactly one 201 and one charge recorded at the PSP.
  - **Idempotency:** a replay returned an identical body, and the PSP call count stayed at 1.
  - **`tok_timeout`:** the API answered 202 within 5 s, the invoice stayed `open`, a second payment got 409, and the reconciler later marked it `paid` with one charge.
- **Live checks on `docker compose`:**
  - The README curls ran verbatim.
  - 20 parallel curl payments gave 1 × 201 and 19 × 409.
  - `tok_timeout`: 202 at 5.0 s, then `paid` about 30 s later.
  - `tok_network_error`: 202, then the attempt `failed` and the invoice could be paid again.
  - Void during a pending attempt returned 409.
  - The webhook sink verified the signatures of all three event types.
- **No floats in the money path:** `grep -rE 'f32|f64|float|numeric|double' src migrations` finds nothing. Postgres re-checks line-item math with a `CHECK` constraint.
- **Code walkthrough:** I read the files that carry the money guarantees, `state_machine.rs`, `payments.rs`, `psp.rs` and `reconciler.rs`, and walk through them in the video.
