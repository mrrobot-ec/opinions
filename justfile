# Default DB URL: host port 15434 maps to container 5432 (compose).
export DATABASE_URL := env_var_or_default("DATABASE_URL", "postgres://opinions:opinions@localhost:15434/opinions")

ci: fmt clippy test coverage deny audit gate-test deps-check

frozen-manifest-check:
    python3 scripts/check_frozen_manifest.py

gate-test:
    python3 scripts/mutation_gate.py scripts/fixtures/pass.json 90
    ! python3 scripts/mutation_gate.py scripts/fixtures/fail.json 90
    ! python3 scripts/mutation_gate.py scripts/fixtures/timeout.json 90
    ! python3 scripts/mutation_gate.py scripts/fixtures/empty.json 90
    python3 -m unittest scripts/test_coverage_gate.py
    python3 -m unittest scripts/test_e2e_live_loop.py
    python3 -m unittest scripts/test_pg_contract_suites.py
    python3 -m unittest scripts/test_e2e_shell_integrity.py
    python3 -m unittest scripts/test_e2e_swarm_smoke.py
    python3 scripts/test_test_quality.py

fmt:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    RUST_TEST_THREADS=1 cargo test --workspace

# 100% line coverage, zero exclusions, on every money-bearing crate.
# The gate asserts the lcov UNION (scripts/coverage_gate.py) because llvm-cov's
# summary folds cfg(test) and non-test instantiation records with
# max-of-covered instead of set-union and therefore UNDER-reports. The summary
# is still printed for humans; the union is what fails the build.
coverage:
    cargo llvm-cov clean --workspace
    cargo llvm-cov -p domain --lcov --output-path var/cov-domain.lcov
    python3 scripts/coverage_gate.py var/cov-domain.lcov domain
    cargo llvm-cov clean --workspace
    cargo llvm-cov -p application --lcov --output-path var/cov-application.lcov
    python3 scripts/coverage_gate.py var/cov-application.lcov application
    cargo llvm-cov clean --workspace
    RUST_TEST_THREADS=1 cargo llvm-cov -p adapters --lcov --output-path var/cov-adapters.lcov
    python3 scripts/coverage_gate.py var/cov-adapters.lcov adapters
    cargo llvm-cov clean --workspace
    cargo llvm-cov -p simswarm --lib --lcov --output-path var/cov-simswarm.lcov
    python3 scripts/coverage_gate.py var/cov-simswarm.lcov simswarm

branch-coverage:
    # nightly-only; scheduled CI job, reviewed not gating (R1/B2)
    cargo +nightly llvm-cov -p domain --branch

mutants:
    # R2 (codex M5): do NOT mask failures. cargo-mutants exit codes: 0 = all caught,
    # 2 = some mutants missed (expected, gate decides), anything else = invalid run.
    cargo mutants -p domain -o mutants.out; ec=$?; \
    if [ "$ec" -ne 0 ] && [ "$ec" -ne 2 ]; then echo "cargo-mutants failed (exit $ec)"; exit "$ec"; fi; \
    python3 scripts/mutation_gate.py mutants.out/outcomes.json 90

deny:
    cargo deny check

audit:
    cargo audit

infra-up:
    docker compose up -d

# Serve the core API (migrations run on startup).
run-core:
    cargo run -p main

# THE E2E DEMO: fresh db → seed → text-to-trade through converse → psql invariants.
e2e:
    bash scripts/e2e_demo.sh

migrate:
    sqlx migrate run --source migrations --database-url $DATABASE_URL

# Offline OpenAPI + Python model generation (Task 1.4).
openapi:
    bash scripts/gen_openapi.sh

api-models:
    bash scripts/gen_api_models.sh

# Architecture gate: crate dependency direction + no sqlx/axum in application.
deps-check:
    python3 scripts/check_dependency_rule.py
    python3 scripts/check_frozen_manifest.py
    # All production HTTP construction is confined to the audited transport.
    hits=$(rg -n 'reqwest::(Client|ClientBuilder|get)|Client::builder\(' crates --glob '*.rs' --glob '!**/llm/transport.rs' --glob '!crates/simswarm/src/transport.rs' --glob '!crates/adapters/src/rails/transport.rs' || true); \
    if [ -n "$hits" ]; then \
      echo "HTTP client construction outside llm/transport.rs:"; echo "$hits"; exit 1; \
    fi
    # Type-leakage guard (Phase 1 review): fail if application sources import sqlx/axum.
    # Graceful skip while crates/application does not exist yet.
    if [ -d crates/application/src ]; then \
      hits=$(rg -l 'sqlx|axum' crates/application/src || true); \
      if [ -n "$hits" ]; then \
        echo "sqlx/axum leakage into application:"; echo "$hits"; exit 1; \
      fi; \
      echo "application type-leakage check OK"; \
    else \
      echo "application crate not present yet — skipping sqlx/axum leakage grep"; \
    fi

# Phase 8 learning site (docs-site/, mkdocs.yml): plain-language guide for
# readers with no software background. `just docs` serves it at :8000.
docs:
    mkdocs serve

docs-build:
    mkdocs build --strict
