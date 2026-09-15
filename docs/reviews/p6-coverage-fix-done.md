# P6 coverage fix — adapters 100% lines (DONE)

Task `task_3ec7c8346f4a` / dispatch `ctx_6079d22a1de7`.

## Result

`RUST_TEST_THREADS=1 cargo llvm-cov -p adapters --fail-under-lines 100` **exits 0**.

| | before | after |
|---|---|---|
| adapters TOTAL lines | 8969 | 8985 |
| adapters missed lines | 3 | **0** |
| adapters line cover | 99.97% | **100.00%** |
| `http/error.rs` missed | 2 | 0 |
| `http/ws.rs` missed | 1 | 0 |
| adapters functions | 1043 / 0 missed | 1043 / 0 missed |

`just coverage` (all four gates) exits 0:

- domain 1897 lines / 0 missed / 100.00%
- application 21746 lines / 0 missed / 100.00%
- adapters 8985 lines / 0 missed / 100.00%
- simswarm 2990 lines / 0 missed / 100.00%

## Root cause

Not a generic-monomorphization artifact in the usual sense — the mechanism is the
same class of problem, but the trigger is the **cfg(test) vs non-cfg(test) build of
the `adapters` lib**.

`llvm-cov report` was given nine objects: the `--lib` unittest binary plus eight
integration-test binaries. The lib is compiled twice with two different crate
disambiguators, so every function exists as two records with different mangled
names:

- `_RNv...Cs iLW4qyqqoqp_8adapters...` — cfg(test) build (the `--lib` binary)
- `_RNv...Cs 1NC8U7lYJZP_8adapters...` — plain build (linked by the 8 integration binaries)

Those two records share a source location, so `llvm-cov report` puts them in one
`InstantiationGroup` and folds them with `LineCoverageInfo::merge`, which is
`Covered = max(a.Covered, b.Covered)` — **a max, not a set union**. So when the
cfg(test) copy covers lines {A..H}\{X} and the plain copy covers {A..H}\{X,Y,Z},
the group reports `max(7, 5) = 7` of 8 → 1 missed, even though the union is 8/8.

That is why `llvm-cov show` / `--lcov` (true segment merge) reported zero
uncovered lines while the summary the threshold reads reported three.

Per-function evidence (`llvm-cov report -show-functions`, before the fix):

```
error.rs  ...ErrorResponse as From<AppError>>::from  (cfg(test) copy)  66 lines,  2 missed
error.rs  ...ErrorResponse as From<AppError>>::from  (plain copy)      66 lines, 45 missed
          → group merge max(64, 21) = 64 of 66 → 2 missed

ws.rs     market_event_is_visible (cfg(test) copy)                      8 lines,  1 missed
ws.rs     market_event_is_visible (plain copy)                          8 lines,  3 missed
          → group merge max(7, 5) = 7 of 8 → 1 missed
```

Isolating the `--lib` object pinned the exact lines:

- `error.rs:114` `AppError::AdminForbidden(_)` arm — count 0
- `error.rs:115` `AppError::ConfigInvalid { .. }` arm — count 0
- `ws.rs:219` `market_event_is_visible` else-branch (`is_subscribed(..)`) — count 0

All three were reachable and covered only from the integration binaries. The fix
is therefore option (3) from the task brief: cover them in the binary that lacked
the instantiation — i.e. add the missing unit-test assertions. No de-generification
was needed, and the existing `#[cfg(test)]` blanket impl in `ws.rs` was already
correctly gated.

## Edits

Two source files, both frozen and both pre-authorized for this task. Test-only
changes; no production code touched.

1. **`crates/adapters/src/http/error.rs`** — in
   `every_application_error_has_a_stable_http_mapping`:
   - added explicit status assertions
     `AdminForbidden("ops.pause") → 403 FORBIDDEN` and
     `ConfigInvalid { key, reason } → 422 UNPROCESSABLE_ENTITY`;
   - added both variants to the `errors` round-trip array (in match-arm order,
     after `ReceivableOpen`) so `into_response()` is exercised for them too.

2. **`crates/adapters/src/http/ws.rs`** — in
   `subscriptions_filter_and_lag_means_disconnect`:
   - added two assertions that a market-scoped `WireEvent` is visible when its
     `aggregate_id` is subscribed and hidden when it is not, exercising the
     else-branch of `market_event_is_visible`.

3. **`scripts/frozen_manifest.txt`**, **`scripts/frozen_manifest.sha256`** —
   re-blessed with `python3 scripts/check_frozen_manifest.py --write`. Path
   inventory unchanged (no files added or removed); only the two digests moved.

### TDD

Both additions were written red first with deliberately wrong expectations and
observed failing, then corrected:

```
http::error::tests::every_application_error_has_a_stable_http_mapping ... FAILED
  assertion `left == right` failed   left: 403  right: 418
http::ws::tests::subscriptions_filter_and_lag_means_disconnect ... FAILED
  assertion failed: !market_event_is_visible(&subscriptions, &wire)
```

Then green with the correct expectations. The assertions bind to real behavior —
they are not coverage padding.

## Verification

| gate | result |
|---|---|
| `RUST_TEST_THREADS=1 cargo llvm-cov -p adapters --fail-under-lines 100` | exit 0 |
| `just coverage` (4 gates) | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0, clean |
| `just test` | exit 0, 19/19 suites ok |
| `python3 scripts/check_frozen_manifest.py` | passes after `--write` |
| `just deps-check` | passes (dependency rule, frozen manifest, HTTP-client guard, sqlx/axum leakage) |

Nothing was lowered, excluded, deleted, or weakened: the threshold stays at
`--fail-under-lines 100`, no `#[coverage(off)]` / ignore-filename additions, no
tests removed.

## Pre-existing issue found, NOT fixed (out of scope)

`cargo fmt --all --check` exits 1 on a **pre-existing** violation unrelated to this
task and outside my authorized edit set:

```
Diff in crates/application/src/contract/video.rs:847
-        let ids: std::collections::BTreeSet<_> =
-            reclaimed.iter().map(|job| job.id).collect();
+        let ids: std::collections::BTreeSet<_> = reclaimed.iter().map(|job| job.id).collect();
```

This will fail the `fmt` step of `just ci`. Both files I edited are fmt-clean. I
left `video.rs` alone in case another worker is mid-edit on it — one `cargo fmt`
run on that file clears it.
