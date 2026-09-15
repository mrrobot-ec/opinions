"""FastAPI webhook tests — PLAN Task 8 Step 5."""

import asyncio
import os
import uuid

import asyncpg
import httpx
import pytest
from fastapi.testclient import TestClient
from langgraph.checkpoint.postgres.aio import AsyncPostgresSaver

import converse.app as app_module
from converse.app import create_app
from converse.graph import PendingStore
from converse.recorder import MemoryRecorder


def make_app():
    """Unit wiring pinned explicitly (DATABASE_URL may be set for integration runs)."""
    return create_app(recorder=MemoryRecorder(), pending_store=PendingStore())


def configured_signing_headers() -> dict[str, str]:
    secret = os.environ.get("SENDBLUE_WEBHOOK_SECRET")
    return {"sb-signing-secret": secret} if secret else {}


def test_sendblue_webhook_ok():
    app = make_app()
    client = TestClient(app)
    r = client.post(
        "/webhooks/sendblue",
        json={
            "number": "+15551234567",
            "content": "price on the coffee market?",
            "message_id": "m1",
        },
        headers=configured_signing_headers(),
    )
    assert r.status_code == 200
    body = r.json()
    assert "reply" in body and body["reply"]
    assert "run_id" in body and body["run_id"]


def test_sendblue_webhook_rejects_unknown_fields():
    app = make_app()
    client = TestClient(app)
    r = client.post(
        "/webhooks/sendblue",
        json={
            "number": "+15551234567",
            "content": "hi",
            "message_id": "m2",
            "extra": "nope",
        },
    )
    assert r.status_code == 422


def test_sendblue_dedupe_returns_original():
    app = make_app()
    client = TestClient(app)
    payload = {
        "number": "+15551234567",
        "content": "price please",
        "message_id": "dedupe-1",
    }
    r1 = client.post(
        "/webhooks/sendblue",
        json=payload,
        headers=configured_signing_headers(),
    )
    r2 = client.post(
        "/webhooks/sendblue",
        json=payload,
        headers=configured_signing_headers(),
    )
    assert r1.status_code == 200 and r2.status_code == 200
    assert r1.json()["run_id"] == r2.json()["run_id"]
    assert r1.json()["reply"] == r2.json()["reply"]


def test_sendblue_webhook_checks_the_configured_signing_secret(monkeypatch):
    monkeypatch.setenv("SENDBLUE_WEBHOOK_SECRET", "test-signing-secret")
    app = make_app()
    client = TestClient(app)
    payload = {
        "number": "+15551234567",
        "content": "price please",
        "message_id": "signed-1",
    }

    assert client.post("/webhooks/sendblue", json=payload).status_code == 401
    assert (
        client.post(
            "/webhooks/sendblue",
            json=payload,
            headers={"sb-signing-secret": "wrong"},
        ).status_code
        == 401
    )
    assert (
        client.post(
            "/webhooks/sendblue",
            json=payload,
            headers={"sb-signing-secret": "test-signing-secret"},
        ).status_code
        == 200
    )


def test_production_requires_a_sendblue_signing_secret(monkeypatch):
    async def resources_must_not_start(**kwargs):
        del kwargs
        raise AssertionError("database resources started before auth validation")

    monkeypatch.setenv("DATABASE_URL", "postgres://test/test")
    monkeypatch.delenv("SENDBLUE_WEBHOOK_SECRET", raising=False)
    monkeypatch.setattr(asyncpg, "create_pool", resources_must_not_start)

    app = app_module.create_app(core=object())
    with pytest.raises(RuntimeError, match="SENDBLUE_WEBHOOK_SECRET"):
        with TestClient(app):
            pass


def test_production_default_core_requires_an_explicit_demo_token(monkeypatch):
    async def resources_must_not_start(**kwargs):
        del kwargs
        raise AssertionError("database resources started before core auth validation")

    monkeypatch.setenv("DATABASE_URL", "postgres://test/test")
    monkeypatch.setenv("SENDBLUE_WEBHOOK_SECRET", "test-signing-secret")
    monkeypatch.delenv("DEMO_TOKEN", raising=False)
    monkeypatch.setattr(asyncpg, "create_pool", resources_must_not_start)

    app = app_module.create_app()
    with pytest.raises(RuntimeError, match="DEMO_TOKEN"):
        with TestClient(app):
            pass


def test_production_isolates_turn_locks_from_recorder_connections(monkeypatch):
    """A saturated turn pool must not deadlock its own recorder calls."""

    class FakePool:
        def __init__(self) -> None:
            self.closed = False

        async def close(self) -> None:
            self.closed = True

    class FakeSaver:
        async def setup(self) -> None:
            pass

    class FakeSaverContext:
        async def __aenter__(self):
            return FakeSaver()

        async def __aexit__(self, exc_type, exc, traceback) -> None:
            pass

    pools: list[FakePool] = []
    captured: dict[str, object] = {}

    async def create_pool(**kwargs):
        del kwargs
        pool = FakePool()
        pools.append(pool)
        return pool

    def capture_graph(**kwargs):
        captured.update(kwargs)
        return object()

    monkeypatch.setenv("DATABASE_URL", "postgres://test/test")
    monkeypatch.setenv("SENDBLUE_WEBHOOK_SECRET", "test-signing-secret")
    monkeypatch.setattr(asyncpg, "create_pool", create_pool)
    monkeypatch.setattr(
        AsyncPostgresSaver,
        "from_conn_string",
        lambda dsn: FakeSaverContext(),
    )
    monkeypatch.setattr(app_module, "build_graph", capture_graph)

    app = app_module.create_app(core=object())
    with TestClient(app):
        pass

    assert len(pools) == 2
    pending_store = captured["pending_store"]
    recorder = captured["recorder"]
    assert pending_store._pool is pools[0]
    assert recorder._pool is pools[1]
    assert all(pool.closed for pool in pools)


@pytest.mark.asyncio
@pytest.mark.integration
async def test_concurrent_production_redelivery_rechecks_dedupe_inside_turn(monkeypatch):
    """Two pre-lock misses must still converge on the original completed run."""
    database_url = os.environ.get("DATABASE_URL")
    assert database_url, "DATABASE_URL is required for the Postgres redelivery contract"
    monkeypatch.setenv("SENDBLUE_WEBHOOK_SECRET", "test-signing-secret")

    import converse.pg_stores as pg_stores

    class FakeCore:
        async def user_by_channel(self, channel: str, address: str):
            del channel, address
            return None

    real_lookup = pg_stores.dedupe_lookup
    first_lookups_ready = asyncio.Event()
    outside_lookup_calls = 0

    async def force_both_outer_lookups_to_miss(pool, channel, inbound_msg_id):
        nonlocal outside_lookup_calls
        store = app.state.pending_store
        if store._conn.get() is None:
            outside_lookup_calls += 1
            if outside_lookup_calls == 2:
                first_lookups_ready.set()
            await first_lookups_ready.wait()
            return None
        return await real_lookup(pool, channel, inbound_msg_id)

    monkeypatch.setattr(
        pg_stores,
        "dedupe_lookup",
        force_both_outer_lookups_to_miss,
    )
    app = create_app(core=FakeCore())
    message_id = f"concurrent-redelivery-{uuid.uuid4()}"
    payload = {
        "number": f"+1555{uuid.uuid4().int % 10_000_000:07d}",
        "content": "price please",
        "message_id": message_id,
    }

    async with app.router.lifespan_context(app):
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(
            transport=transport,
            base_url="http://converse.test",
        ) as client:
            responses = await asyncio.gather(
                client.post(
                    "/webhooks/sendblue",
                    json=payload,
                    headers={"sb-signing-secret": "test-signing-secret"},
                ),
                client.post(
                    "/webhooks/sendblue",
                    json=payload,
                    headers={"sb-signing-secret": "test-signing-secret"},
                ),
                return_exceptions=True,
            )

    assert all(isinstance(response, httpx.Response) for response in responses), repr(
        responses
    )
    assert all(response.status_code == 200 for response in responses)
    assert responses[0].json() == responses[1].json()
    assert outside_lookup_calls == 0
