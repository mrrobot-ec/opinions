"""Recorder tests — PLAN Task 8 Step 4 (+ integration pending protocol)."""

from __future__ import annotations

import asyncio
import os
import uuid

import pytest

from converse.graph import build_graph, run_turn
from converse.recorder import MemoryRecorder
from converse.schemas import Intent, RouterOutput


class FakeRouter:
    async def __call__(self, rendered_prompt: str, context: dict) -> RouterOutput:
        return RouterOutput(intent=Intent.PRICE, rationale="fake")


@pytest.mark.asyncio
async def test_memory_recorder_produces_steps():
    rec = MemoryRecorder()
    graph = build_graph(router=FakeRouter(), recorder=rec)
    result = await run_turn(
        graph,
        phone="+15551234567",
        content="price on coffee?",
        message_id="m-rec-1",
    )
    run_id = result["run_id"]
    run = rec.runs[run_id]
    assert run.status == "ok"
    assert len(run.steps) >= 3
    assert all(s.state_before is not None for s in run.steps)
    assert all(s.state_after is not None for s in run.steps)
    seqs = [s.seq for s in run.steps]
    assert seqs == sorted(seqs)


@pytest.mark.integration
@pytest.mark.asyncio
async def test_pg_recorder_and_pending_protocol():
    database_url = os.environ.get("DATABASE_URL")
    assert database_url, "DATABASE_URL is required for PostgreSQL integration tests"

    import asyncpg

    from converse.recorder import PgRecorder

    pool = await asyncpg.create_pool(database_url, min_size=1, max_size=4)
    try:
        rec = PgRecorder(pool)
        run_id = str(uuid.uuid4())
        msg_id = f"int-{uuid.uuid4()}"
        await rec.start_run(
            run_id=run_id,
            thread_id="+15559990000",
            channel="sendblue_imessage",
            trigger="webhook",
            user_id=None,
            inbound_msg_id=msg_id,
        )
        await rec.record_step(
            run_id=run_id,
            seq=1,
            node="load_session",
            kind="tool",
            state_before={"a": 1},
            state_after={"a": 2},
        )
        await rec.end_run(run_id=run_id, status="ok", final_intent="price")

        async with pool.acquire() as conn:
            status = await conn.fetchval(
                "select status from agent_runs where id = $1::uuid", run_id
            )
            steps = await conn.fetchval(
                "select count(*) from agent_steps where run_id = $1::uuid", run_id
            )
        assert status == "ok"
        assert steps == 1

        # --- pending protocol under advisory lock ---
        thread = f"thread-{uuid.uuid4()}"
        run_for_pending = str(uuid.uuid4())
        async with pool.acquire() as conn:
            await conn.execute(
                """
                insert into agent_runs (
                  id, thread_id, channel, trigger, status, started_at, inbound_msg_id
                ) values ($1::uuid, $2, 'test', 'test', 'ok', now(), $3)
                """,
                run_for_pending,
                thread,
                f"p-{uuid.uuid4()}",
            )

        # Create an already-expired pending row
        async with pool.acquire() as conn:
            await conn.execute(
                """
                insert into pending_actions (
                  id, thread_id, run_id, kind, payload, expires_at, consumed_at
                ) values (
                  $1::uuid, $2, $3::uuid, 'trade', '{}'::jsonb,
                  now() - interval '1 minute', null
                )
                """,
                str(uuid.uuid4()),
                thread,
                run_for_pending,
            )

        async def expiry_then_insert() -> None:
            async with pool.acquire() as conn:
                async with conn.transaction():
                    await conn.execute(
                        "select pg_advisory_xact_lock(hashtext($1))", thread
                    )
                    # 1. expiry sweep
                    await conn.execute(
                        """
                        update pending_actions set consumed_at = now()
                         where thread_id = $1 and consumed_at is null
                           and expires_at <= now()
                        """,
                        thread,
                    )
                    # 3. create pending
                    await conn.execute(
                        """
                        insert into pending_actions (
                          id, thread_id, run_id, kind, payload, expires_at
                        ) values (
                          $1::uuid, $2, $3::uuid, 'trade', '{}'::jsonb,
                          now() + interval '2 minutes'
                        )
                        """,
                        str(uuid.uuid4()),
                        thread,
                        run_for_pending,
                    )

        await expiry_then_insert()
        async with pool.acquire() as conn:
            active = await conn.fetchval(
                """
                select count(*) from pending_actions
                 where thread_id = $1 and consumed_at is null
                """,
                thread,
            )
        assert active == 1

        # Two concurrent confirms → exactly one consume
        action_id = None
        async with pool.acquire() as conn:
            action_id = await conn.fetchval(
                """
                select id::text from pending_actions
                 where thread_id = $1 and consumed_at is null
                """,
                thread,
            )

        async def try_consume() -> str | None:
            async with pool.acquire() as conn:
                async with conn.transaction():
                    await conn.execute(
                        "select pg_advisory_xact_lock(hashtext($1))", thread
                    )
                    row = await conn.fetchrow(
                        """
                        update pending_actions set consumed_at = now()
                         where id = $1::uuid and consumed_at is null
                           and expires_at > now()
                         returning id::text
                        """,
                        action_id,
                    )
                    return row["id"] if row else None

        results = await asyncio.gather(try_consume(), try_consume())
        wins = [r for r in results if r is not None]
        assert len(wins) == 1

        # Two concurrent previews → exactly one active (second must sweep/replace under lock)
        thread2 = f"thread-{uuid.uuid4()}"
        run2 = str(uuid.uuid4())
        async with pool.acquire() as conn:
            await conn.execute(
                """
                insert into agent_runs (
                  id, thread_id, channel, trigger, status, started_at, inbound_msg_id
                ) values ($1::uuid, $2, 'test', 'test', 'ok', now(), $3)
                """,
                run2,
                thread2,
                f"p2-{uuid.uuid4()}",
            )

        async def try_preview(n: int) -> None:
            async with pool.acquire() as conn:
                async with conn.transaction():
                    await conn.execute(
                        "select pg_advisory_xact_lock(hashtext($1))", thread2
                    )
                    await conn.execute(
                        """
                        update pending_actions set consumed_at = now()
                         where thread_id = $1 and consumed_at is null
                        """,
                        thread2,
                    )
                    await conn.execute(
                        """
                        insert into pending_actions (
                          id, thread_id, run_id, kind, payload, expires_at
                        ) values (
                          $1::uuid, $2, $3::uuid, 'vote', $4::jsonb,
                          now() + interval '2 minutes'
                        )
                        """,
                        str(uuid.uuid4()),
                        thread2,
                        run2,
                        f'{{"n": {n}}}',
                    )

        await asyncio.gather(try_preview(1), try_preview(2))
        async with pool.acquire() as conn:
            active2 = await conn.fetchval(
                """
                select count(*) from pending_actions
                 where thread_id = $1 and consumed_at is null
                """,
                thread2,
            )
        assert active2 == 1
    finally:
        await pool.close()
