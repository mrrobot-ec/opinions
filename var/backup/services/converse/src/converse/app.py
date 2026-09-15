"""FastAPI entry — Sendblue webhook with (channel, message_id) dedupe.

Two wirings share one shape:
- Unit/dev (no `DATABASE_URL`): in-memory recorder/pending store/dedupe, the
  Phase-0 behavior, built eagerly so plain `TestClient(app)` works.
- Production (`DATABASE_URL` set): asyncpg pool, `PgRecorder`,
  `PgPendingStore` (+ per-thread advisory-lock turn scope),
  `AsyncPostgresSaver` checkpointer, dedupe against
  `agent_runs (channel, inbound_msg_id)`, and the real `CoreClient` pointed
  at `CORE_API_URL` — all assembled in the lifespan.
"""

from __future__ import annotations

import contextlib
import os
import uuid
from typing import Any

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, ConfigDict

from converse.core_client import CoreClient
from converse.graph import (
    DemoExtractorRouter,
    PendingStore,
    build_graph,
    run_turn,
)
from converse.recorder import MemoryRecorder
from converse.schemas import Intent, RouterOutput


class SendblueWebhook(BaseModel):
    model_config = ConfigDict(extra="forbid")

    number: str
    content: str
    message_id: str


class WebhookReply(BaseModel):
    reply: str
    run_id: str


class FixedRouter:
    """Default stub router for the unit wiring (no real LLM)."""

    async def __call__(self, rendered_prompt: str, context: dict) -> RouterOutput:
        text = (rendered_prompt or "").lower()
        if "price" in text:
            return RouterOutput(intent=Intent.PRICE, rationale="price keyword")
        if "vote" in text:
            return RouterOutput(intent=Intent.VOTE, rationale="vote keyword")
        if "trade" in text or "buy" in text or "sell" in text:
            return RouterOutput(intent=Intent.PLACE_TRADE, rationale="trade keyword")
        return RouterOutput(intent=Intent.CHITCHAT, rationale="default")


def create_app(
    *,
    router: Any | None = None,
    recorder: Any | None = None,
    pending_store: Any | None = None,
    core: Any | None = None,
    checkpointer: Any | None = None,
) -> FastAPI:
    database_url = os.environ.get("DATABASE_URL")
    production = database_url is not None and recorder is None and pending_store is None

    @contextlib.asynccontextmanager
    async def lifespan(app: FastAPI):
        if production:
            import asyncpg  # noqa: PLC0415 — production-only dependency path
            from langgraph.checkpoint.postgres.aio import AsyncPostgresSaver

            from converse.pg_stores import PgPendingStore, PgRecorder

            pool = await asyncpg.create_pool(dsn=database_url, min_size=1, max_size=8)
            psycopg_dsn = database_url.replace("postgres://", "postgresql://", 1)
            saver_cm = AsyncPostgresSaver.from_conn_string(psycopg_dsn)
            saver = await saver_cm.__aenter__()
            await saver.setup()
            store = PgPendingStore(pool)
            core_client = core or CoreClient()
            app.state.graph = build_graph(
                router=router or DemoExtractorRouter(),
                recorder=PgRecorder(pool),
                pending_store=store,
                checkpointer=saver,
                core=core_client,
            )
            app.state.pending_store = store
            app.state.pg_pool = pool
            try:
                yield
            finally:
                if core is None:
                    await core_client.aclose()
                await saver_cm.__aexit__(None, None, None)
                await pool.close()
        else:
            yield

    app = FastAPI(title="converse", version="0.1.0", lifespan=lifespan)
    app.state.graph = None
    app.state.pg_pool = None
    # channel + message_id -> (reply, run_id); production dedupes via agent_runs
    dedupe: dict[tuple[str, str], tuple[str, str]] = {}
    app.state.dedupe = dedupe

    if not production:
        rec = recorder or MemoryRecorder()
        store = pending_store if pending_store is not None else PendingStore()
        app.state.graph = build_graph(
            router=router or FixedRouter(),
            recorder=rec,
            pending_store=store,
            checkpointer=checkpointer,
            core=core,
        )
        app.state.recorder = rec
        app.state.pending_store = store

    @app.get("/healthz")
    async def healthz() -> dict[str, str]:
        return {"status": "ok"}

    @app.post("/webhooks/sendblue", response_model=WebhookReply)
    async def sendblue_webhook(body: SendblueWebhook) -> WebhookReply:
        graph = app.state.graph
        if graph is None:  # pragma: no cover — lifespan not started
            raise HTTPException(status_code=503, detail="graph not ready")
        channel = "sendblue_imessage"
        key = (channel, body.message_id)

        if app.state.pg_pool is not None:
            from converse.pg_stores import dedupe_lookup

            seen = await dedupe_lookup(app.state.pg_pool, channel, body.message_id)
            if seen is not None:
                reply, run_id = seen
                return WebhookReply(reply=reply, run_id=run_id)
        elif key in dedupe:
            reply, run_id = dedupe[key]
            return WebhookReply(reply=reply, run_id=run_id)

        # The per-thread turn scope: a no-op in memory; in production it holds
        # pg_advisory_xact_lock(hashtext(phone)) for the whole turn.
        async with app.state.pending_store.turn(body.number):
            result = await run_turn(
                graph,
                phone=body.number,
                content=body.content,
                message_id=body.message_id,
                channel=channel,
            )
        reply = result.get("reply") or "OK."
        run_id = result.get("run_id") or str(uuid.uuid4())
        dedupe[key] = (reply, run_id)
        return WebhookReply(reply=reply, run_id=run_id)

    return app


app = create_app()
