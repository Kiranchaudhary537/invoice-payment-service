-- When the reconciler should next ask the PSP about a pending attempt. Bumped after every
-- inconclusive check, so attempts the PSP keeps reporting as `processing` go to the back of the
-- queue instead of filling every batch and starving newer ones.
ALTER TABLE payment_attempts ADD COLUMN next_reconcile_at timestamptz;
