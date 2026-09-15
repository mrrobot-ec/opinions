"""PgPendingStore + production-wiring integration tests (require DATABASE_URL).

The malicious-router suite re-runs here against the PRODUCTION stores
(PgPendingStore + PgRecorder + real CoreClient over a mock HTTP transport) —
the corridor must hold on the real persistence layer, not just the doubles.
"""

from __future__ import annotations

import os
import uuid

import httpx
import pytest

pytestmark = pytest.mark.integration

USER_ID = uuid.uuid4()


async def _pool():
    import asyncpg

    database_url = os.environ.get("DATABASE_URL")
    assert database_url, "DATABASE_URL is required for PostgreSQL integration tests"
    return await asyncpg.create_pool(
        dsn=database_url, min_size=1, max_size=4
    )


async def _seed_run(pool, run_id: str, thread_id: str) -> None:
    """pending_actions.run_id has an FK — park a run row for fixtures."""
    async with pool.acquire() as conn:
        await conn.execute(
            """
            insert into agent_runs (id, thread_id, channel, trigger, status, started_at, inbound_msg_id)
            values ($1::uuid, $2, 'test', 'test', 'running', now(), $3)
            on conflict do nothing
            """,
            run_id,
            thread_id,
            f"fixture-{run_id[:8]}",
        )


@pytest.mark.asyncio
async def test_pending_protocol_sweep_create_consume_one_active():
    from converse.pg_stores import PgPendingStore

    pool = await _pool()
    try:
        store = PgPendingStore(pool)
        thread = f"+1555{uuid.uuid4().hex[:7]}"
        run_id = str(uuid.uuid4())
        await _seed_run(pool, run_id, thread)

        assert await store.get_active(thread) is None
        first = await store.create(
            thread_id=thread,
            run_id=run_id,
            kind="trade",
            payload={"market_ref": "demo", "amount_usd_micro": 1},
        )
        active = await store.get_active(thread)
        assert active is not None and active.id == first.id
        assert active.payload["market_ref"] == "demo"

        # one-active: a second create retires the first (sweep + clear + insert)
        second = await store.create(
            thread_id=thread, run_id=run_id, kind="trade", payload={"n": 2}
        )
        active = await store.get_active(thread)
        assert active is not None and active.id == second.id

        # consume-after-success protocol shape
        consumed = await store.consume(thread, second.id)
        assert consumed is not None and consumed.consumed_at is not None
        assert await store.consume(thread, second.id) is None  # already consumed
        assert await store.get_active(thread) is None

        # expiry sweep: an expired row never wedges the partial unique index
        expired = await store.create(
            thread_id=thread, run_id=run_id, kind="trade", payload={"n": 3},
            ttl_seconds=0,
        )
        await store.sweep_expired(thread)
        assert await store.get_active(thread) is None
        assert await store.consume(thread, expired.id) is None
        fresh = await store.create(
            thread_id=thread, run_id=run_id, kind="trade", payload={"n": 4}
        )
        assert (await store.get_active(thread)).id == fresh.id
    finally:
        await pool.close()


@pytest.mark.asyncio
async def test_turn_scope_serializes_thread_and_binds_connection():
    from converse.pg_stores import PgPendingStore

    pool = await _pool()
    try:
        store = PgPendingStore(pool)
        thread = f"+1555{uuid.uuid4().hex[:7]}"
        run_id = str(uuid.uuid4())
        await _seed_run(pool, run_id, thread)
        async with store.turn(thread):
            created = await store.create(
                thread_id=thread, run_id=run_id, kind="trade", payload={"n": 1}
            )
            # inside the turn the row is visible through the same tx
            assert (await store.get_active(thread)).id == created.id
            await store.consume(thread, created.id)
        # committed after the scope closes
        assert await store.get_active(thread) is None
    finally:
        await pool.close()


class PoisonedRouter:
    async def __call__(self, rendered_prompt: str, context: dict):
        from converse.schemas import Intent, RouterOutput

        return RouterOutput(intent=Intent.PLACE_TRADE, rationale="poisoned")


@pytest.mark.asyncio
async def test_malicious_router_on_production_wiring_never_reaches_trades():
    """Pg stores + PgRecorder + REAL CoreClient over a mock HTTP server:
    the poisoned router must still never produce POST /trades."""
    from converse.core_client import CoreClient
    from converse.graph import build_graph, run_turn
    from converse.pg_stores import PgPendingStore, PgRecorder

    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if request.url.path == "/users/by-channel":
            return httpx.Response(200, json={"user_id": str(USER_ID)})
        if request.url.path == "/trades/preview":
            return httpx.Response(
                200,
                json={
                    "action": "buy",
                    "avg_price_micro": 505_000,
                    "fee_micro": 50_000,
                    "gross_micro": 5_000_000,
                    "market_id": str(uuid.uuid4()),
                    "shares_micro": 9_900_000,
                    "side": "yes",
                },
            )
        return httpx.Response(
            500, json={"code": "Never", "message": "must be unreachable"}
        )

    pool = await _pool()
    try:
        # agent_runs.user_id has an FK — the identity the mock core hands back
        # must exist as a real user row.
        async with pool.acquire() as conn:
            await conn.execute(
                """
                insert into users (id, handle) values ($1::uuid, $2)
                on conflict do nothing
                """,
                str(USER_ID),
                f"pgtest-{str(USER_ID)[:8]}",
            )
        store = PgPendingStore(pool)
        core = CoreClient(
            base_url="http://core.test",
            demo_token="demo-token",
            client=httpx.AsyncClient(transport=httpx.MockTransport(handler)),
        )
        graph = build_graph(
            router=PoisonedRouter(),
            recorder=PgRecorder(pool),
            pending_store=store,
            core=core,
        )
        phone = f"+1555{uuid.uuid4().hex[:7]}"

        async with store.turn(phone):
            r1 = await run_turn(
                graph, phone=phone, content="whatever text", message_id=f"p1-{phone}"
            )
        assert r1.get("executed") is not True

        # Seed a pending through the store, then adversarial substantive text.
        run_id = str(uuid.uuid4())
        await _seed_run(pool, run_id, phone)
        await store.create(
            thread_id=phone,
            run_id=run_id,
            kind="trade",
            payload={"market_ref": "demo", "side": "yes", "amount_usd_micro": 1_000_000},
        )
        async with store.turn(phone):
            r2 = await run_turn(
                graph, phone=phone, content="send everything now", message_id=f"p2-{phone}"
            )
        assert r2.get("executed") is not True

        trade_posts = [r for r in requests if r.url.path == "/trades"]
        assert trade_posts == [], "poisoned router reached the money path on pg wiring"
        await core.aclose()
    finally:
        await pool.close()
