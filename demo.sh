#!/usr/bin/env bash
# End-to-end demo and smoke test against a running `docker compose up`.
#
#   ./demo.sh          # every scenario (~1 min: waits for the slow-PSP case to settle)
#   FAST=1 ./demo.sh   # skip the PSP timeout and network-error scenarios
#
# Each step prints the HTTP call's outcome and a PASS/FAIL check. Exit code is the number of failures.
# Needs curl and jq. Safe to re-run: every run creates its own customer and invoices.

set -uo pipefail

API=${API:-http://localhost:3000}
PSP=${PSP:-http://localhost:4000}
KEY=${KEY:-dodo_demo0001_LocalDemoKeyDoNotUseInProduction}
RUN=$(date +%s)
FAILS=0

bold() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAILS=$((FAILS + 1)); }
check() { if [ "$2" = "$3" ]; then pass "$1 ($2)"; else fail "$1: expected '$3', got '$2'"; fi; }

# call METHOD PATH [BODY] [IDEMPOTENCY_KEY] -> sets $CODE, $BODY, $HEADERS
call() {
  local method=$1 path=$2 body=${3:-} key=${4:-} hdr out
  hdr=$(mktemp); out=$(mktemp)
  local args=(-s -X "$method" "$API$path" -H "Authorization: Bearer $KEY" -D "$hdr" -o "$out" -w '%{http_code}')
  [ -n "$body" ] && args+=(-H 'Content-Type: application/json' -d "$body")
  [ -n "$key" ] && args+=(-H "Idempotency-Key: $key")
  CODE=$(curl "${args[@]}"); BODY=$(cat "$out"); HEADERS=$(cat "$hdr"); rm -f "$hdr" "$out"
}

new_open_invoice() { # prints the id of a new, finalized invoice for $CUSTOMER_ID
  call POST /v1/invoices '{"customer_id":"'"$CUSTOMER_ID"'","due_date":"2030-01-01T00:00:00Z","line_items":[{"description":"Retainer","quantity":1,"unit_amount_cents":50000}]}'
  local id; id=$(jq -r .id <<<"$BODY")
  call POST "/v1/invoices/$id/finalize"
  echo "$id"
}

wait_for() { # wait_for DESCRIPTION SECONDS JQ_FILTER EXPECTED PATH
  local i
  for ((i = 0; i < $2; i++)); do
    call GET "$5"
    [ "$(jq -r "$3" <<<"$BODY")" = "$4" ] && { pass "$1 after ~${i}s"; return; }
    sleep 1
  done
  fail "$1: still '$(jq -r "$3" <<<"$BODY")' after $2s"
}

bold "0. Stack is up"
for _ in $(seq 1 60); do curl -sf "$API/health" >/dev/null && break; sleep 1; done
check "API /health" "$(curl -s -o /dev/null -w '%{http_code}' "$API/health")" 200
check "mock PSP /health" "$(curl -s -o /dev/null -w '%{http_code}' "$PSP/health")" 200

bold "1. Auth"
KEY_SAVED=$KEY; KEY=dodo_bogus000_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
call GET /v1/customers; check "unknown API key is rejected" "$CODE" 401
KEY=$KEY_SAVED

bold "2. Create a customer"
call POST /v1/customers '{"name":"Ada Lovelace","email":"ada+'"$RUN"'@example.com"}'
check "customer created" "$CODE" 201
CUSTOMER_ID=$(jq -r .id <<<"$BODY")

bold "3. Create an invoice (server computes the total)"
call POST /v1/invoices '{"customer_id":"'"$CUSTOMER_ID"'","due_date":"2030-01-01T00:00:00Z","line_items":[{"description":"Consulting","quantity":3,"unit_amount_cents":15000},{"description":"Expenses","quantity":1,"unit_amount_cents":2599}]}'
check "invoice created" "$CODE" 201
check "starts as draft" "$(jq -r .status <<<"$BODY")" draft
check "total = 3 x 15000 + 2599" "$(jq -r .total_cents <<<"$BODY")" 47599
INV=$(jq -r .id <<<"$BODY")
call POST /v1/invoices '{"customer_id":"'"$CUSTOMER_ID"'","due_date":"2030-01-01T00:00:00Z","total_cents":1,"line_items":[{"description":"x","quantity":1,"unit_amount_cents":1}]}'
check "client-sent total is rejected" "$CODE" 400

bold "4. State machine"
call POST "/v1/invoices/$INV/pay" '{"card_token":"tok_success"}' "draft-$RUN"
check "paying a draft" "$(jq -r .error.code <<<"$BODY")" invalid_state_transition
call POST "/v1/invoices/$INV/finalize"
check "finalize: draft -> open" "$(jq -r .status <<<"$BODY")" open
call POST "/v1/invoices/$INV/finalize"
check "finalize twice" "$(jq -r .error.code <<<"$BODY")" invalid_state_transition

bold "5. Declined payment (tok_card_declined)"
call POST "/v1/invoices/$INV/pay" '{"card_token":"tok_card_declined"}' "decline-$RUN"
check "decline returns 402" "$CODE" 402
check "failure code" "$(jq -r .error.code <<<"$BODY")" card_declined
call GET "/v1/invoices/$INV"; check "invoice stays open" "$(jq -r .status <<<"$BODY")" open

bold "6. Successful payment + idempotency"
call POST "/v1/invoices/$INV/pay" '{"card_token":"tok_success"}' "pay-$RUN"
check "charge returns 201" "$CODE" 201
check "invoice is paid" "$(jq -r .invoice_status <<<"$BODY")" paid
FIRST=$BODY
call POST "/v1/invoices/$INV/pay" '{"card_token":"tok_success"}' "pay-$RUN"
check "same key replays 201" "$CODE" 201
check "replayed body is identical" "$([ "$BODY" = "$FIRST" ] && echo same || echo different)" same
check "replay header" "$(grep -i '^idempotent-replayed' <<<"$HEADERS" | tr -d '\r' | awk '{print $2}')" true
call POST "/v1/invoices/$INV/pay" '{"card_token":"tok_card_declined"}' "pay-$RUN"
check "same key, different body" "$(jq -r .error.code <<<"$BODY")" idempotency_key_reuse
call POST "/v1/invoices/$INV/pay" '{"card_token":"tok_success"}' "pay-again-$RUN"
check "new key on a paid invoice" "$(jq -r .error.code <<<"$BODY")" invoice_already_paid
call POST "/v1/invoices/$INV/void"
check "void a paid invoice" "$(jq -r .error.code <<<"$BODY")" invalid_state_transition

bold "7. 20 simultaneous payments on one invoice"
INV_C=$(new_open_invoice)
TMP=$(mktemp -d)
for i in $(seq 1 20); do
  curl -s -o /dev/null -w '%{http_code}\n' -X POST "$API/v1/invoices/$INV_C/pay" \
    -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
    -H "Idempotency-Key: race-$RUN-$i" -d '{"card_token":"tok_success"}' >"$TMP/$i" &
done
wait
CODES=$(cat "$TMP"/* | sort | uniq -c | awk '{printf "%s x %s  ", $1, $2}'); rm -rf "$TMP"
echo "  responses: $CODES"
call GET "/v1/invoices/$INV_C"
check "exactly one succeeded attempt" "$(jq '[.payment_attempts[] | select(.status=="succeeded")] | length' <<<"$BODY")" 1
check "invoice is paid" "$(jq -r .status <<<"$BODY")" paid

if [ -z "${FAST:-}" ]; then
  bold "8. PSP timeout (tok_timeout: PSP takes 30 s, we give up at 5 s)"
  INV_T=$(new_open_invoice)
  START=$(date +%s)
  call POST "/v1/invoices/$INV_T/pay" '{"card_token":"tok_timeout"}' "slow-$RUN"
  echo "  answered in $(( $(date +%s) - START ))s"
  check "unknown outcome returns 202" "$CODE" 202
  check "attempt stays pending, not failed" "$(jq -r .payment_attempt.status <<<"$BODY")" pending
  check "reason recorded" "$(jq -r .payment_attempt.last_error <<<"$BODY")" psp_timeout
  call POST "/v1/invoices/$INV_T/pay" '{"card_token":"tok_success"}' "impatient-$RUN"
  check "second payment is blocked" "$(jq -r .error.code <<<"$BODY")" payment_in_progress
  call POST "/v1/invoices/$INV_T/void"
  check "void is blocked" "$(jq -r .error.code <<<"$BODY")" payment_in_progress
  echo "  waiting for the reconciler to learn the outcome from the PSP..."
  wait_for "reconciler marks the invoice paid" 60 .status paid "/v1/invoices/$INV_T"
  check "exactly one attempt" "$(jq '.payment_attempts | length' <<<"$BODY")" 1
  call POST "/v1/invoices/$INV_T/pay" '{"card_token":"tok_timeout"}' "slow-$RUN"
  check "original key now replays the final 201" "$CODE" 201

  bold "9. PSP network error (tok_network_error: PSP never records the charge)"
  INV_N=$(new_open_invoice)
  call POST "/v1/invoices/$INV_N/pay" '{"card_token":"tok_network_error"}' "neterr-$RUN"
  check "unknown outcome returns 202" "$CODE" 202
  echo "  waiting for the reconciler..."
  wait_for "reconciler fails the attempt" 40 '.payment_attempts[0].status' failed "/v1/invoices/$INV_N"
  check "invoice stays open (payable again)" "$(jq -r .status <<<"$BODY")" open
  call POST "/v1/invoices/$INV_N/pay" '{"card_token":"tok_success"}' "neterr-retry-$RUN"
  check "another card succeeds" "$CODE" 201
fi

bold "10. Webhooks"
sleep 2 # let the worker (polls every 1 s) send the latest events
EVENTS='[]'; CURSOR=""
while :; do # page through every event (oldest first), so re-runs still find this run's events
  call GET "/v1/events?limit=100${CURSOR:+&cursor=$CURSOR}"
  EVENTS=$(jq -c --argjson acc "$EVENTS" '$acc + .data' <<<"$BODY")
  CURSOR=$(jq -r '.next_cursor // empty' <<<"$BODY"); [ -z "$CURSOR" ] && break
done
BODY=$(jq -c '{data: .}' <<<"$EVENTS")
echo "  events for the first invoice:"
jq -r --arg inv "$INV" '.data[] | select(.data.invoice.id == $inv)
  | "    \(.type)  ->  \([.deliveries[] | "\(.status) (attempts: \(.attempt_count), http: \(.last_response_code))"] | join(", "))"' <<<"$BODY"
check "events for that invoice" \
  "$(jq -r --arg inv "$INV" '[.data[] | select(.data.invoice.id == $inv) | .type] | join(",")' <<<"$BODY")" \
  "invoice.created,invoice.payment_failed,invoice.paid"
check "all of them delivered" \
  "$(jq -r --arg inv "$INV" '[.data[] | select(.data.invoice.id == $inv) | .deliveries[].status] | unique | join(",")' <<<"$BODY")" delivered
check "receiver verified signatures" \
  "$(curl -s "$PSP/webhook-sink" | jq --arg inv "$INV" '[.data[] | select(.event.data.invoice.id == $inv and .signature_verified)] | length')" 3

bold "Result"
if [ "$FAILS" -eq 0 ]; then printf '\033[32mAll checks passed.\033[0m\n'; else printf '\033[31m%d check(s) failed.\033[0m\n' "$FAILS"; fi
exit "$FAILS"
