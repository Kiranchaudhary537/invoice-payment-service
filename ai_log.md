# AI decision log (working file — source for AI_USAGE.md)

Tool: Claude Code (Opus) in VS Code, used as a design reviewer ("grill me" interview) before any code.
Format: what AI recommended → what I decided → agreed / overrode.

## Decisions made before any AI involvement (raw_design.md, iterations 1–2)
- `status` not `state` on invoices, to avoid confusion with payment-attempt state.
- API keys hashed with SHA-256 (not bcrypt/argon): keys are high-entropy random, slow hashing buys nothing. Prefix lookup + constant-time compare.
- Idempotency via a separate `idempotency_keys` table (Approach B) over key columns on payment_attempts (Approach A).
- Line items as jsonb on invoices (immutable, always read with invoice).
- Partial unique indexes on payment_attempts: one succeeded per invoice, one in-flight per invoice.
- `business_id` denormalized on every table (tenant scoping + future shard key).
- Webhook secret must be retrievable (HMAC needs raw value), so it cannot be hashed.

## Round 1
- AI flagged inconsistencies in raw design: iteration-1 states (draft-failed / failed / overdue) vs iteration-2 enum; "queue" vs outbox tables; missing /pay transaction boundaries; no API-key bootstrap; endpoint list not matching spec.
- ID type: AI recommended UUIDv7 (app-generated) → agreed (index-friendly, non-enumerable).
- DB layer: AI recommended sqlx + plain SQL over an ORM → I said ORM is fine either way; went with sqlx so lock/conditional SQL is visible.
- Mock PSP: AI recommended separate binary, idempotent on `reference`, with `GET /charges/{ref}` → agreed, asked that endpoints be added as the tests need.
- Bootstrap: AI recommended env-seeded business + key and `POST/DELETE /api_keys` → agreed, wanted both.
- Invoice states: I asked AI to check whether failed / overdue / processing are needed → AI concluded none (failure is per-attempt, overdue is derived, in-flight lives on attempt) → agreed. Final: draft, open, paid, void, uncollectible.
- **`invoice.created` timing: AI recommended firing on create (draft) → I overrode: fire when the invoice becomes `open` (finalize), because that is when the customer-facing invoice exists.**
- Webhooks: AI recommended Postgres outbox + in-process worker → agreed; I required DESIGN.md to document why no message queue.
- Webhook secret: AI recommended plaintext + documented gap → agreed; I required DESIGN.md to describe how to encrypt it.

## Round 2
- **Concurrency: AI recommended `SELECT … FOR UPDATE` + partial unique index → I asked for optimistic locking instead.**
- PSP timeout: 5 s client timeout, attempt stays `pending`, return 202 → agreed. I required that nothing is locked while waiting on the PSP.
- Reconciler for pending attempts → agreed.
- Crash + same idempotency key → 409 until reconciler resolves → agreed.
- Decline → HTTP 402 → agreed.
- Webhook signing/retry numbers → agreed. Webhook sink on mock PSP → agreed ("best for demo and for reviewers to test").
- **API key prefix: AI proposed `sk_…` → I changed to `dodo_…`.**
- **Tests: AI proposed extra tests (separate network-error test etc.) → I overrode: only the tests the assignment requires.**
- API docs as Markdown → agreed.

## Round 3
- Optimistic locking: AI designed version-CAS on invoices (every invoice mutation bumps `version`, losers get 409) + partial unique index backstop → agreed. My reason: scalability, no lock held while waiting.
- **Idempotency TTL: AI recommended no purge job (just treat expired rows as absent) → I overrode: the table must not grow unbounded, so build a TTL purge, AND the system must stay safe when a duplicate arrives after its key was purged.**
- AI recommended not storing transient 409s in idempotency records → agreed.
- Tests: three required tests as Rust integration tests, timeout test uses `tok_timeout` → agreed.
- Cut list taken from my own raw-design ideas (queue+DLQ, overdue/dunning, failed-webhook notification, secret encryption, leaked-key scanning) → agreed.
- Production gaps: observability, rate limiting, audit log → agreed; **I added: DESIGN.md must describe how production rate limiting would work (docs only, no code).**
- **Line items: raw design (mine) said jsonb; AI recommended keeping jsonb → I changed to a separate `invoice_line_items` table now, with jsonb documented as the 100x option.**
- Authorship: Claude Code writes the implementation code; design decisions are mine (this log is the evidence).
- Compose: fixed demo key in compose, bootstrap also registers the webhook sink → agreed.

## Round 4 (final read-back, confirmed)
- TTL: 24 h expiry, hourly batched purge, expired rows treated as absent on read → agreed.
- Duplicate after TTL purge: rely on domain invariants (paid-invoice check + unique succeeded index for /pay, unique email for customers, duplicate invoice is only a harmless draft) → agreed. "The key is a retry convenience; invariants are the money guarantee."
- Locking wording: AI pointed out "no DB locks" is not literally true (an UPDATE takes a row lock for its transaction) → agreed on accurate wording: no explicit locks, nothing held across the PSP call, losers fail fast.
- invoice_line_items with DB CHECKs incl. `amount_cents = quantity * unit_amount_cents` → agreed.
- DESIGN.md ≈1,500 words, 7 spec sections; detail pushed to migrations + API.md → agreed.
- DESIGN.md written before code; AI_USAGE.md drafted now, finalized after implementation → agreed.
- Video failure mode: (b) tok_timeout → agreed.
- uncollectible → void only (no late payment on uncollectible) → agreed.

## Implementation (Claude Code wrote the code; facts recorded for AI_USAGE.md)
- Stack as agreed: Rust 1.86, Axum 0.8, sqlx 0.8 (plain SQL), one migration, one Docker image with `api` + `mock-psp`.
- All 3 required tests passed on first run, then 5/5 repeated runs.
- Live checks on docker compose: README curls verbatim, 20 parallel pays (1×201, 19×409), tok_timeout (202 at 5.0 s → paid ~30 s later), tok_network_error (202 → failed psp_unreachable → payable again), webhook retry scheduled ~10 s after a failure, purge SQL deletes expired keys.
- AI-side mistakes caught: DESIGN.md claimed `SKIP LOCKED` in the reconciler and `(created_at, id)` pagination; the code uses neither (status-guarded finish, UUIDv7 id cursor) → DESIGN.md corrected. Test API key was 31 chars instead of 32 → caught by a length check.
- Choices made during implementation that weren't explicitly grilled (flag if you disagree):
  - Status columns are `text + CHECK`, not Postgres enums (easier to evolve, simpler with sqlx).
  - `POST /api_keys` and `POST /webhook_endpoints` ignore Idempotency-Key, because the stored response would persist the secret.
  - Idempotency hash is over canonical JSON (sorted keys), so whitespace/key order don't cause false 422s.
  - Mock PSP `tok_network_error` returns 500 and records nothing (so reconciler sees 404 → failed).
  - Webhook sink returns 400 on a bad signature (so a misconfigured secret visibly triggers retries).
  - `GET /v1/events` includes per-endpoint delivery status (to show deliveries in the demo).
