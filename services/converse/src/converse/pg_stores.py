"""Production Postgres stores for converse (migrations 0001+0003).

- `PgPendingStore`: pending_actions with the locked protocol — expiry sweep
  first, SELECT for the gate, consume-AFTER-core-success, one active row per
  thread (partial unique index). Its `turn()` scope takes the per-thread
  `pg_advisory_xact_lock` (spec §7.5) so a whole webhook turn is serialized;
  every store call inside the scope rides that same transaction.
- `PgRecorder` (re-exported from `recorder`) is the app default when
  `DATABASE_URL` is set.
- `dedupe_lookup`: webhook redelivery dedupe against
  `agent_runs (channel, inbound_msg_id)`.
"""

from __future__ import annotations

import contextlib
import contextvars
import json
from typing import Any, AsyncIterator

import asyncpg

from converse.graph import PendingAction
from converse.recorder import PgRecorder

__all__ = ["PgPendingStore", "PgRecorder", "create_pool", "dedupe_lookup"]


async def create_pool(dsn: str) -> asyncpg.Pool:
    return await asyncpg.create_pool(dsn=dsn, min_size=1, max_size=8)


def _row_to_action(row: asyncpg.Record) -> PendingAction:
    return PendingAction(
        id=str(row["id"]),
        thread_id=row["thread_id"],
        run_id=str(row["run_id"]),
        kind=row["kind"],
        payload=json.loads(row["payload"]),
        expires_at=row["expires_at"],
        consumed_at=row["consumed_at"],
    )


_ROW_COLS = "id, thread_id, run_id, kind, payload::text as payload, expires_at, consumed_at"


class PgPendingStore:
    """pending_actions over asyncpg, connection-scoped per turn."""

    def __init__(self, pool: asyncpg.Pool) -> None:
        self._pool = pool
        self._conn: contextvars.ContextVar[asyncpg.Connection | None] = (
            contextvars.ContextVar("pg_pending_conn", default=None)
        )

    @contextlib.asynccontextmanager
    async def turn(self, thread_id: str) -> AsyncIterator[None]:
        """One webhook turn: a transaction holding the per-thread advisory lock.

        `pg_advisory_xact_lock(hashtext(thread_id))` releases at commit, so two
        replicas (or a redelivery race) cannot interleave gate/execute for one
        phone — select-without-consume can never double-fire concurrently.
        """
        async with self._pool.acquire() as conn:
            async with conn.transaction():
                await conn.execute(
                    "select pg_advisory_xact_lock(hashtext($1))", thread_id
                )
                token = self._conn.set(conn)
                try:
                    yield
                finally:
                    self._conn.reset(token)

    async def _fetchrow(self, sql: str, *args: Any) -> asyncpg.Record | None:
        conn = self._conn.get()
        if conn is not None:
            return await conn.fetchrow(sql, *args)
        async with self._pool.acquire() as fresh:
            return await fresh.fetchrow(sql, *args)

    async def _execute(self, sql: str, *args: Any) -> None:
        conn = self._conn.get()
        if conn is not None:
            await conn.execute(sql, *args)
            return
        async with self._pool.acquire() as fresh:
            await fresh.execute(sql, *args)

    async def sweep_expired(self, thread_id: str) -> None:
        # Step 1 of the locked protocol: an expired unconsumed row must never
        # wedge the one-active partial unique index.
        await self._execute(
            """
            update pending_actions set consumed_at = clock_timestamp()
             where thread_id = $1 and consumed_at is null and expires_at <= clock_timestamp()
            """,
            thread_id,
        )

    async def get_active(self, thread_id: str) -> PendingAction | None:
        row = await self._fetchrow(
            f"""
            select {_ROW_COLS} from pending_actions
             where thread_id = $1 and consumed_at is null and expires_at > clock_timestamp()
             limit 1
            """,
            thread_id,
        )
        return _row_to_action(row) if row else None

    async def create(
        self,
        *,
        thread_id: str,
        run_id: str,
        kind: str,
        payload: dict[str, Any],
        ttl_seconds: int = 120,
    ) -> PendingAction:
        await self.sweep_expired(thread_id)
        # one-active: replace any live pending (the gate cleared substantive
        # texts already; this mirrors the memory-store semantics).
        await self.clear(thread_id)
        row = await self._fetchrow(
            f"""
            insert into pending_actions (thread_id, run_id, kind, payload, expires_at)
            values ($1, $2::uuid, $3, $4::jsonb, clock_timestamp() + make_interval(secs => $5))
            returning {_ROW_COLS}
            """,
            thread_id,
            run_id,
            kind,
            json.dumps(payload),
            float(ttl_seconds),
        )
        if row is None:  # pragma: no cover — insert..returning always yields
            raise RuntimeError("pending insert returned no row")
        return _row_to_action(row)

    async def consume(self, thread_id: str, action_id: str) -> PendingAction | None:
        # Step 2 of the locked protocol; zero rows = lost the race or expired.
        row = await self._fetchrow(
            f"""
            update pending_actions set consumed_at = clock_timestamp()
             where id = $2::uuid and thread_id = $1
               and consumed_at is null and expires_at > clock_timestamp()
            returning {_ROW_COLS}
            """,
            thread_id,
            action_id,
        )
        return _row_to_action(row) if row else None

    async def clear(self, thread_id: str) -> None:
        await self._execute(
            """
            update pending_actions set consumed_at = clock_timestamp()
             where thread_id = $1 and consumed_at is null
            """,
            thread_id,
        )


async def dedupe_lookup(
    pool: asyncpg.Pool, channel: str, message_id: str
) -> tuple[str, str] | None:
    """Webhook redelivery dedupe: (reply, run_id) of the already-processed run."""
    async with pool.acquire() as conn:
        run = await conn.fetchrow(
            "select id from agent_runs where channel = $1 and inbound_msg_id = $2",
            channel,
            message_id,
        )
        if run is None:
            return None
        run_id = str(run["id"])
        step = await conn.fetchrow(
            """
            select state_after->>'reply' as reply
              from agent_steps
             where run_id = $1::uuid and state_after ? 'reply'
             order by seq desc limit 1
            """,
            run_id,
        )
        reply = (step and step["reply"]) or "Already processed."
        return reply, run_id
