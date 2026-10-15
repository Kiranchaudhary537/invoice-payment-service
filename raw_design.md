## This file contains all iteration till final design. 
## Note: This file contains no A.I, it's all human design/decision. it's may contains grammar and spelling mistake.

### iteration - 1  - basic details
#### Data Model 

 + Business 
   - Id (unique)
   - Name -string (don't required because we don't have to display anything,  this can help to trigger mails/notification)
   - email - string (needed to trigger mails)
 
 + Customer 
   - Id (unique)
   - Email - string (not unique bcs same customer pay belong to multiple business)
   - BusinessId - string  (many customer to one business)
  
 + Invoice
   - Id (unique)
   - CustomerId (it's created for customer)
   - BusinessId (it's created by business for customer)
   - total - int
   - status - enum(draft, open, paid, void, uncollectible). 
            (note: using status instead of state, reason don't want to be confused with transaction/payment states vs individual tx state)
   - due date - timestamp 
   - timestamp - timestamp (it's needed to verify time)

 + Payment attemps 
   - Id 
   - Invoice Id 
   - status - enum
   - timestamp - timestamp

 + Items 
  - Id
  - InvoiceId
  - description
  - quantity
  - unit_amount_cents


  
Q: 
 1. data type of Id ? (we have to decide best data type) (can't use uuidv4 other version can be used, ascending number ) 

#### Endpoints 
   - /health 
   - customer 
      + get /
      + post /
      + put /{id}/pay
   - invoice 
      + get  /
      + post /
      + put /{id}
      + pay /{id}/pay
    - apiKey
      + get /
      + post /
      + put /{id}
    - webhook 
      + post / (create - register)
      + get / get details of register webhook

### ApiKey design
- every business can generate manage their apy key from dashboard needs  
- we need to store api key in hashing formate can use argon/bcrypt
- on create only one time they view/copy api key after than only revocation possible from dashboard.
- transmission- not sure yet. 
- rotation - not sure yet. 
- revocation - one user request or we can build own system which check github and other sources and flag exposed keys. 
- in case of keys are leaked - 1. we have to revoke api key -> send new api key to client  

Q:
1. on api key revoke existing flow stop working? how to handle this 
2. how to design endpoint which handle unknown future cases 


###  Invoice State Machine
State: 
   1. Client created invoice (draft) 
   2. Send invoice to client 
      2.1 on success: state:open
      2.2 on failure -> retry mechanism -> still failed -> description on failed, state: draft failed
    3. client pays:
      3.1 on success: state: paid 
      3.2 on failure: retry mechanism -> description why client not paid, state: failed
    4. after due date - state: overdue  (auto by system )
    5. business can update to state: uncollectible

### Webhooks
  - on every invoice change -> goes to queue with details
  - queue takes invoice check their status on based on status if fires webhook endpoint
  - in case of failure handle retry and dead queue letter 
  - also handle mecahnism where if x number of webhhook call failed for client then send them notification 






### iteration - 2 - Final raw thought before final design.md
- fill missing missing details, edge cases


### Data Model
+ businesses
  - id (pk)
  - name        (not needed for the flow; used in notification/email text)
  - email       (needed to send mails/alerts)
  - created_at

+ api_keys
  - id (pk)
  - business_id (fk)
  - prefix       (unique, first ~8 chars, used for lookup, safe to show in dashboards/logs)
  - key_hash     (SHA-256 of the full key; keys are high-entropy random, so slow hashes like bcrypt are not needed)
  - revoked_at   (null = active; revoke = set timestamp, never delete, for audit)
  - last_used_at
  - created_at
  index: unique(prefix)
  flow: lookup by prefix -> constant-time compare hash -> check revoked_at is null
  rotation: create new key, then revoke old (multiple active keys allowed)

+ customers
  - id (pk)
  - business_id (fk)
  - name, email
  - created_at
  unique(business_id, email)   (not globally unique: same person can be a customer of many businesses)
  unique(business_id, id)      (target for composite FK from invoices)

+ invoices
  - id (pk)
  - business_id
  - customer_id
  - status       enum(draft, open, paid, void, uncollectible)
                 (named "status" not "state" to avoid confusion with payment/tx states)
  - total_cents  bigint, check (total_cents >= 0)   (server-computed from line items)
  - line_items   jsonb  (see "100x" below; use a separate table if you want per-row constraints now)
  - due_date     timestamptz
  - paid_at, voided_at   (nullable, set on transition)
  - created_at, updated_at
  fk: (business_id, customer_id) -> customers(business_id, id)
  indexes:
    (business_id, status, created_at desc, id)  -> "list by state" + keyset pagination
    (business_id, customer_id, created_at desc) -> invoices of a customer
    (due_date) where status = 'open'            -> future dunning / overdue job

+ payment_attempts
  - id (pk)              (also sent to PSP as its idempotency/reference key)
  - invoice_id (fk), business_id
  - amount_cents         (copied from invoice at attempt time; immutable record)
  - status   enum(pending, succeeded, failed, unknown)
             (unknown = PSP timed out / network error, result not known yet)
  - card_token
  - psp_ref              (nullable until PSP answers)
  - failure_code         (insufficient_funds, card_declined, timeout, ...)
  - idempotency_key      (the client key that created it)
  - created_at, completed_at
  indexes:
    unique(invoice_id) where status = 'succeeded'  -> never two successful charges
    unique(invoice_id) where status in ('pending','unknown') -> only one in-flight attempt
    (invoice_id, created_at)
    (status, created_at) where status in ('pending','unknown') -> reconciler job

+ idempotency_keys
  - business_id, key       (pk together; scoped per business, not global)
  - request_method_path    (same key on a different endpoint = error)
  - request_hash           (sha256 of the body)
  - status    enum(in_progress, completed)
  - response_code, response_body
  - created_at, expires_at (24h TTL, purge job)
  behavior:
    same key + same hash + completed   -> replay stored response, no PSP call
    same key + different hash          -> 422 idempotency_key_reuse
    same key + in_progress             -> 409 (request still running)

+ webhook_endpoints
  - id (pk), business_id
  - url
  - secret   (stored encrypted, not hashed: we need the raw value to compute HMAC)
  - event_types text[]  (or '*')
  - enabled
  - created_at

+ events                    (append-only, what happened)
  - id (pk), business_id
  - type      (invoice.created, invoice.paid, invoice.payment_failed)
  - resource_id (invoice id)
  - payload   jsonb (snapshot at that moment)
  - created_at
  index: (business_id, created_at desc, id)  -> GET /events for reconciliation

+ webhook_deliveries        (how we tried to tell each endpoint)
  - id (pk)
  - event_id (fk), endpoint_id (fk)
  - status   enum(pending, delivered, failed)  (failed = retries exhausted)
  - attempt_count
  - next_attempt_at
  - last_response_code, last_error
  - delivered_at
  index: (next_attempt_at) where status = 'pending' -> worker poll

At 100x
Fewer joins: line_items is already merged into invoices as jsonb. It’s written once, never edited, always read with the invoice and never queried alone, so the join buys nothing. Trade-off: no DB-level quantity > 0 check, so validate in the app and keep a CHECK (jsonb_typeof(line_items) = 'array').
Other merges: don’t merge payment_attempts, events or deliveries. They’re high-volume, append-heavy and have different lifecycles. businesses and api_keys could merge, but keys need independent revoke and rotation, so keep both.
Read performance: indexes are composite and start with business_id (every query is tenant-scoped). Use keyset pagination (created_at, id), not OFFSET. Add read replicas for list endpoints.
Partitioning: events, webhook_deliveries, payment_attempts and idempotency_keys by month, so old partitions can be dropped or archived cheaply. Idempotency keys can also move to Redis with TTL.
Sharding key if needed: business_id. Every table already carries it, which is why I keep it denormalized on invoices and attempts.


#decisions:
Idempotency: two approaches, and why I chose one
Approach A: Key columns on payment_attempts (no separate table)
idempotency_key and request_hash live on the payment_attempts row, with unique(business_id, idempotency_key).
Replay means rebuilding the response from that row.

Advantages

Simple: no extra table, no TTL purge job.
One less insert/update in each transaction.
Key and attempt are always consistent, since they’re the same row.

Trade-offs

Only works for /pay. Invoice creation stays unprotected against duplicate POSTs.
Requests that create no row (e.g. 409 on an already-paid invoice) can’t be replayed.
The response is rebuilt, not stored, so it can drift from the original.
Keys never expire.
Approach B: Separate idempotency_keys table (chosen)
Pk is (business_id, key). Stores request_hash, status, response_code, response_body, expires_at.
Inserted at the start of the transaction, completed at the end.

Advantages

One mechanism covers every POST (customers, invoices, pay).
Replays the exact stored response, including errors.
The INSERT ... ON CONFLICT resolves same-key races up front.
Keys expire after 24h and are purged without touching business data.
The answer to “same key, different body” is simple: compare hashes, return 422.

Trade-offs

One more table, and a purge job.
One extra INSERT and UPDATE per request, inside transactions we already run.
The stored response duplicates some data.