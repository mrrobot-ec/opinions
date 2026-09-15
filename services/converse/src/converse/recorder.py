"""Agent run recorder — MemoryRecorder (tests) + PgRecorder (Postgres audit tables)."""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Protocol


def _utcnow() -> datetime:
    return datetime.now(timezone.utc)


@dataclass
class StepRecord:
    id: str
    run_id: str
    seq: int
    node: str
    kind: str
    state_before: dict | None
    state_after: dict | None
    error: str | None = None


@dataclass
class RunRecord:
    id: str
    thread_id: str
    channel: str
    trigger: str
    user_id: str | None
    inbound_msg_id: str | None
    final_intent: str | None
    status: str
    started_at: datetime
    ended_at: datetime | None = None
    steps: list[StepRecord] = field(default_factory=list)


class Recorder(Protocol):
    async def start_run(
        self,
        *,
        run_id: str,
        thread_id: str,
        channel: str,
        trigger: str,
        user_id: str | None,
        inbound_msg_id: str | None,
    ) -> None: ...

    async def record_step(
        self,
        *,
        run_id: str,
        seq: int,
        node: str,
        kind: str,
        state_before: dict | None,
        state_after: dict | None,
        error: str | None = None,
    ) -> None: ...

    async def end_run(
        self,
        *,
        run_id: str,
        status: str,
        final_intent: str | None,
    ) -> None: ...


class MemoryRecorder:
    """In-memory recorder for unit tests."""

    def __init__(self) -> None:
        self.runs: dict[str, RunRecord] = {}

    async def start_run(
        self,
        *,
        run_id: str,
        thread_id: str,
        channel: str,
        trigger: str,
        user_id: str | None,
        inbound_msg_id: str | None,
    ) -> None:
        self.runs[run_id] = RunRecord(
            id=run_id,
            thread_id=thread_id,
            channel=channel,
            trigger=trigger,
            user_id=user_id,
            inbound_msg_id=inbound_msg_id,
            final_intent=None,
            status="running",
            started_at=_utcnow(),
        )

    async def record_step(
        self,
        *,
        run_id: str,
        seq: int,
        node: str,
        kind: str,
        state_before: dict | None,
        state_after: dict | None,
        error: str | None = None,
    ) -> None:
        run = self.runs[run_id]
        run.steps.append(
            StepRecord(
                id=str(uuid.uuid4()),
                run_id=run_id,
                seq=seq,
                node=node,
                kind=kind,
                state_before=state_before,
                state_after=state_after,
                error=error,
            )
        )

    async def end_run(
        self,
        *,
        run_id: str,
        status: str,
        final_intent: str | None,
    ) -> None:
        run = self.runs[run_id]
        run.status = status
        run.final_intent = final_intent
        run.ended_at = _utcnow()


class PgRecorder:
    """Postgres recorder matching migrations/0001_init.sql agent_runs/agent_steps."""

    def __init__(self, pool: Any) -> None:
        self._pool = pool

    async def start_run(
        self,
        *,
        run_id: str,
        thread_id: str,
        channel: str,
        trigger: str,
        user_id: str | None,
        inbound_msg_id: str | None,
    ) -> None:
        async with self._pool.acquire() as conn:
            await conn.execute(
                """
                insert into agent_runs (
                  id, thread_id, channel, trigger, user_id, inbound_msg_id,
                  status, started_at
                ) values ($1::uuid, $2, $3, $4, $5::uuid, $6, 'running', now())
                on conflict (channel, inbound_msg_id) do nothing
                """,
                run_id,
                thread_id,
                channel,
                trigger,
                user_id,
                inbound_msg_id,
            )

    async def record_step(
        self,
        *,
        run_id: str,
        seq: int,
        node: str,
        kind: str,
        state_before: dict | None,
        state_after: dict | None,
        error: str | None = None,
    ) -> None:
        import json

        step_id = str(uuid.uuid4())
        async with self._pool.acquire() as conn:
            await conn.execute(
                """
                insert into agent_steps (
                  id, run_id, seq, node, kind, state_before, state_after, error
                ) values (
                  $1::uuid, $2::uuid, $3, $4, $5, $6::jsonb, $7::jsonb, $8
                )
                """,
                step_id,
                run_id,
                seq,
                node,
                kind,
                json.dumps(state_before) if state_before is not None else None,
                json.dumps(state_after) if state_after is not None else None,
                error,
            )

    async def end_run(
        self,
        *,
        run_id: str,
        status: str,
        final_intent: str | None,
    ) -> None:
        async with self._pool.acquire() as conn:
            await conn.execute(
                """
                update agent_runs
                   set status = $2, final_intent = $3, ended_at = now()
                 where id = $1::uuid
                """,
                run_id,
                status,
                final_intent,
            )
