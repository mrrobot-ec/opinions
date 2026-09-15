# Set it up locally

How to get the whole thing running on your own machine. Follow it top to bottom; each step
says what it is for.

## What you need

| Tool | Why |
|---|---|
| **Rust**, via `rustup` | Builds the core. The exact version — 1.97.1 — is pinned in `rust-toolchain.toml`, so `rustup` installs it for you. |
| **PostgreSQL 16** | The database. That is the version the project's `docker-compose.yml` uses. |
| **just** | A task runner. Turns long commands into `just test`. |
| **Docker** | Optional but easiest — the project ships a compose file. |
| **Node.js + pnpm** | Only if you want the website. |
| **Python 3.12+** | Only if you want the iMessage service. |
| **mkdocs + mkdocs-material** | Only if you want to build this guide. |

On a Mac with Homebrew:

```bash
brew install rustup just node pnpm
rustup-init
```

## 1. Start the database

The simplest route is the project's own compose file, which puts PostgreSQL on port
**15434** — the port every default in this repository expects:

```bash
just infra-up
```

That also starts a Redis and a NATS container. **Nothing uses them.** No crate in the
workspace depends on either; everything runs on PostgreSQL. You can ignore them, or delete
those two blocks from `docker-compose.yml` and nothing will break.

If you would rather run PostgreSQL yourself:

```bash
initdb ~/.opinions-pg
pg_ctl -D ~/.opinions-pg -l ~/.opinions-pg/server.log -o "-p 15434" start
createuser -h 127.0.0.1 -p 15434 -s opinions
createdb  -h 127.0.0.1 -p 15434 -U opinions opinions
```

The connection string the project defaults to — in `main`, in the `justfile`, everywhere —
is:

```
postgres://opinions:opinions@localhost:15434/opinions
```

so if you match that, you do not need to set `DATABASE_URL` at all. If you use something
else, export it.

## 2. Build the schema

You usually do not have to. **The server runs any pending migrations itself at startup.**

If you want to do it explicitly:

```bash
just migrate
```

or, without the `sqlx` tool installed:

```bash
for f in migrations/*.sql; do
  psql -h 127.0.0.1 -p 15434 -U opinions -d opinions -v ON_ERROR_STOP=1 -f "$f"
done
```

!!! tip "Always glob the folder"
    Use the wildcard rather than typing the files you remember. Every environment that ever
    broke on this project broke because someone replayed a partial list, or edited a file
    that had already been applied. The files are the truth; the running database is a cache
    of them.

## 3. Run the tests

```bash
just test
```

This compiles everything and runs the whole Rust suite — the last recorded full run was
1,027 tests across 23 binaries. The first build takes a few minutes; later ones are quick.

Note the recipe sets `RUST_TEST_THREADS=1`. The database-backed tests are not safe to run
concurrently against one database, so the recipe serialises them for you. This is the main
reason to use `just test` rather than calling `cargo test` directly.

## 4. Start the API

Two environment variables matter before you start.

**`DEMO_TOKEN`** stands in for real user authentication, which is out of scope for the
phases built so far. It defaults to `demo-token` if unset.

**`ADMIN_TOKENS_JSON`** is required — the server refuses to start without it. It is a JSON
**array** of admin credentials, each with an id, a list of roles, and the SHA-256 hex digest
of the actual token. The raw token is never stored, only its digest, and comparison is
constant-time.

To create one:

```bash
printf 'local-admin-token' | shasum -a 256
# bc32a9a66697ce5242e5517f4a801fa2a3dccf9960459f666fec8a166289401e
```

Then:

```bash
export DEMO_TOKEN="local-dev-token"
export ADMIN_TOKENS_JSON='[{"id":"local-admin","roles":["superadmin"],"sha256":"bc32a9a66697ce5242e5517f4a801fa2a3dccf9960459f666fec8a166289401e"}]'
just run-core
```

Send `local-admin-token` as the admin credential; the server hashes what you send and
compares digests.

The valid roles are `curator`, `ops`, `finance` and `superadmin`. The document is validated
strictly: an empty array, an empty id, a token with no roles, a repeated role, a duplicate
id, a duplicate digest, or a digest that is not exactly 64 hex characters are each a startup
error with a specific message.

It listens on `127.0.0.1:8080` by default. Check it:

```bash
curl localhost:8080/healthz
curl localhost:8080/markets
```

Seed some demo data:

```bash
cargo run -p main -- --seed-demo
```

And to run the continuous invariant sweep alongside the server:

```bash
cargo run -p main -- --continuous
```

## 5. The website (optional)

```bash
cd web
pnpm install
CORE_PROXY_TARGET=http://127.0.0.1:8080 pnpm dev
```

Then open `http://localhost:3000`. Without `CORE_PROXY_TARGET` the site builds and runs but
has nothing to talk to — that variable is what installs the `/core-api/...` proxy route.

## 6. The iMessage service (optional)

```bash
cd services/converse
pip install -e '.[dev]'
pytest
```

It needs credentials for a messaging provider and a language-model key to run for real; its
tests run without either, using in-memory stand-ins. Tests that need a database are marked
`integration` and skip when `DATABASE_URL` is unset.

## 7. This guide (optional)

```bash
just docs        # serve at http://127.0.0.1:8000
just docs-build  # build to var/site, with --strict
```

## Environment variables worth knowing

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | Where PostgreSQL is. Defaults to the port-15434 string above. |
| `BIND_ADDR` | Where to listen. Defaults to `127.0.0.1:8080`; anything non-loopback prints a warning, because these phases have no real user authentication. |
| `DEMO_TOKEN` | Stands in for user login. |
| `ADMIN_TOKENS_JSON` | Admin credentials and roles — **required**, strictly validated. |
| `SCHEDULER_TICK_MS` | How often the lifecycle scheduler runs. Default 1,000; zero is a startup error. |
| `RAIL_GENESIS_HASH` and the other `RAIL_*` values | Blockchain identity. If the genesis hash is set, the *whole* identity must be present and is validated against the live network at startup. If it is unset, the process stays off-rail entirely. |
| `OPINIONS_ENV` + `CHAOS_ENABLED` | The two-factor arm for fault injection. Any `CHAOS_*` variable without **both** of these is a startup error. |
| `OPINIONS_ENV` + `STAGING_FAUCET` | The two-factor arm for sandbox test helpers. |
| `TRUSTED_PROXY_CIDRS` | Which network ranges may set a client's IP via a forwarded-for header. Empty means trust none. |
| `DEVICE_HASH_SECRET` | The key for device fingerprints. |
| `REP_*`, `FEE_*`, `POSITION_CAP_*`, `MIN_FEE_BPS` | The economy policy. See the warning below. |
| `DRAFT_PUBLISHER_ENABLED`, `VIDEO_WORKER_ENABLED`, `MODERATION_RUNNER_ENABLED` | Background content loops. All default to off. |

The pattern to notice: the server **refuses to start** when a security-relevant variable is
missing or malformed. It never falls back to a permissive default. A half-set chaos arm, an
incomplete blockchain identity, a malformed admin document — each stops the process rather
than degrading quietly.

!!! warning "A bare local server does not apply the published economy policy"
    The tier fee discounts, the position caps and the minimum fee floor are read from
    environment variables at startup, and their built-in defaults are *no discount* and *no
    cap*. Migration `0008` seeds the intended policy into the configuration table, and the
    control plane validates changes against it — but a server started with no economy
    variables set will charge every tier the base fee and impose no position limit. If you
    are exploring the tier system locally, set `FEE_DISCOUNT_BP_BY_TIER`,
    `MIN_FEE_BPS` and `POSITION_CAP_MICRO_BY_TIER` explicitly.

## Common problems

**"connection refused"** — PostgreSQL is not running, or it is on the default port 5432
instead of 15434. Start it with the `-o "-p 15434"` flag shown above, or set `DATABASE_URL`.

**"ADMIN_TOKENS_JSON is required"** — exactly what it says. See step 4; note that it is an
array, not an object, and that the value is a digest, not the token.

**Foreign-key errors that match no source file** — your local database is older than the
migration files. Drop it, recreate it, and re-run all the migrations.

**Tests time out on database connections** — you are running them in parallel. `just test`
already serialises them; use it rather than calling `cargo` directly.

**A test suite "passes" in 0.00 seconds** — it skipped. Some PostgreSQL contract suites need
their own scratch database; a suite that finds nothing to connect to used to report success.
This bit the project once, which is why those suites now create their own database.

## Where to go next

- [Tests and quality gates](tests-and-gates.md)
- [The swarm](the-swarm.md)
- [How this was built](how-this-was-built.md)
