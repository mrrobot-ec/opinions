-- Phase 6 W1: canonical request fingerprints for idempotency-replay
-- precedence (D25, codex r3 NEW-3). A key HIT compares against this row —
-- match replays the original receipt with NO pause/config check, mismatch
-- is a typed 409 IdempotencyConflict. Rows are written in the same
-- transaction as the write they describe. Pre-0009 keys have no row and
-- replay as before (documented).
create table request_fingerprints (
    idempotency_key text primary key,
    fingerprint text not null,
    created_at timestamptz not null default now()
);
