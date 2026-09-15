"""LangGraph conversation graph — lexical pending gate + agent-free execute corridor.

Phase 1 wiring: the pending store is async (Postgres-backed in production),
`execute_pending` follows consume-AFTER-success (grok-p1r1 B1), and when a
`core` client is injected the preview/execute nodes call the real core API —
`idempotency_key = pending_action_id` keeps retries replay-safe.
"""

from __future__ import annotations

import contextlib
import re
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any, AsyncIterator, Awaitable, Callable, Protocol, TypedDict

from langgraph.checkpoint.memory import MemorySaver
from langgraph.graph import END, START, StateGraph

from converse.core_client import CoreError
from converse.recorder import MemoryRecorder, Recorder
from converse.schemas import AgentNode, Intent, RouterOutput, TradeParams, VoteParams


CONFIRM_ALLOWLIST = frozenset(
    {"confirm", "yes", "y", "yep", "do it", "lock it", "yes do it"}
)
CANCEL_ALLOWLIST = frozenset({"cancel", "no", "stop", "nevermind"})
POLITENESS = frozenset({"please", "pls", "thanks", "thank", "thx"})

# The deterministic demo extractor's grammar (plan Task 1.5): the demo proves
# the seam, not NLU. Applied to lowercased text.
DEMO_TRADE_RE = re.compile(r"buy \$(\d+) of (yes|no) on ([a-z0-9-]+)")
DEMO_VOTE_RE = re.compile(r"vote (yes|no) (\d{1,3}) on ([a-z0-9-]+)")

ONBOARDING_REPLY = (
    "Welcome to Opinions! This number doesn't have an account yet — "
    "ask the operator to onboard you."
)

# Unicode ranges covering common emoji / symbols used in iMessage tapbacks.
_EMOJI_RE = re.compile(
    "["
    "\U0001f300-\U0001faff"
    "\U00002700-\U000027bf"
    "\U0001f000-\U0001f02f"
    "\U0001f0a0-\U0001f0ff"
    "\U00002600-\U000026ff"
    "\U0000fe00-\U0000fe0f"
    "\U0000200d"
    "]+",
    flags=re.UNICODE,
)
_PUNCT_RE = re.compile(r"[^\w\s]+", flags=re.UNICODE)
_WS_RE = re.compile(r"\s+")


def normalize_text(text: str) -> str:
    """Spec §7.5: lowercase → strip punct/emoji → collapse ws → drop politeness tokens."""
    t = (text or "").lower()
    t = _EMOJI_RE.sub(" ", t)
    t = _PUNCT_RE.sub(" ", t)
    t = _WS_RE.sub(" ", t).strip()
    if not t:
        return ""
    parts = t.split(" ")
    # drop leading/trailing politeness tokens
    while parts and parts[0] in POLITENESS:
        parts.pop(0)
    while parts and parts[-1] in POLITENESS:
        parts.pop()
    return " ".join(parts)


class ExtractedRoute(RouterOutput):
    """Router output extended with deterministic parameter extraction.

    Any AgentNode may return this richer shape; params flow to the preview
    nodes through state. A plain RouterOutput still works.
    """

    trade_params: TradeParams | None = None
    vote_params: VoteParams | None = None


class DemoExtractorRouter:
    """The plan's deterministic extractor, injected through the AgentNode seam."""

    async def __call__(self, rendered_prompt: str, context: dict) -> ExtractedRoute:
        text = (rendered_prompt or "").lower()
        trade = DEMO_TRADE_RE.search(text)
        if trade:
            return ExtractedRoute(
                intent=Intent.PLACE_TRADE,
                rationale="demo extractor: trade grammar",
                trade_params=TradeParams(
                    market_ref=trade.group(3),
                    side=trade.group(2),
                    amount_usd_micro=int(trade.group(1)) * 1_000_000,
                    action="buy",
                    rationale="demo extractor: trade grammar",
                ),
            )
        vote = DEMO_VOTE_RE.search(text)
        if vote and int(vote.group(2)) <= 100:
            return ExtractedRoute(
                intent=Intent.VOTE,
                rationale="demo extractor: vote grammar",
                vote_params=VoteParams(
                    market_ref=vote.group(3),
                    side=vote.group(1),
                    crowd_guess_pct=int(vote.group(2)),
                    rationale="demo extractor: vote grammar",
                ),
            )
        if "price" in text:
            return ExtractedRoute(intent=Intent.PRICE, rationale="price keyword")
        if "portfolio" in text or "position" in text:
            return ExtractedRoute(intent=Intent.PORTFOLIO, rationale="portfolio keyword")
        return ExtractedRoute(intent=Intent.CHITCHAT, rationale="no demo grammar matched")


class ConverseState(TypedDict, total=False):
    phone: str
    content: str
    message_id: str
    channel: str
    turn_counter: int
    intent: str | None
    reply: str
    run_id: str
    gate_outcome: str
    executed: bool
    execute_called: bool
    pending_cleared: bool
    pending_kept: bool
    normalized: str
    user_id: str | None
    kyc_region: str | None
    onboarding: bool
    pending_id: str | None
    trade_params: dict[str, Any] | None
    vote_params: dict[str, Any] | None


@dataclass
class PendingAction:
    id: str
    thread_id: str
    run_id: str
    kind: str
    payload: dict[str, Any]
    expires_at: datetime
    consumed_at: datetime | None = None


class PendingStoreProtocol(Protocol):
    """Async pending_actions port — memory (tests) or Postgres (production)."""

    async def get_active(self, thread_id: str) -> PendingAction | None: ...

    async def sweep_expired(self, thread_id: str) -> None: ...

    async def create(
        self,
        *,
        thread_id: str,
        run_id: str,
        kind: str,
        payload: dict[str, Any],
        ttl_seconds: int = 120,
    ) -> PendingAction: ...

    async def consume(self, thread_id: str, action_id: str) -> PendingAction | None: ...

    async def clear(self, thread_id: str) -> None: ...

    def turn(self, thread_id: str) -> Any: ...


@dataclass
class PendingStore:
    """In-memory pending_actions (unit tests; PgPendingStore in production)."""

    _by_thread: dict[str, PendingAction] = field(default_factory=dict)

    def _active_sync(self, thread_id: str, now: datetime | None = None) -> PendingAction | None:
        now = now or datetime.now(timezone.utc)
        p = self._by_thread.get(thread_id)
        if p is None or p.consumed_at is not None:
            return None
        if p.expires_at <= now:
            return None
        return p

    async def get_active(self, thread_id: str) -> PendingAction | None:
        return self._active_sync(thread_id)

    async def sweep_expired(self, thread_id: str) -> None:
        now = datetime.now(timezone.utc)
        p = self._by_thread.get(thread_id)
        if p is not None and p.consumed_at is None and p.expires_at <= now:
            p.consumed_at = now

    async def create(
        self,
        *,
        thread_id: str,
        run_id: str,
        kind: str,
        payload: dict[str, Any],
        ttl_seconds: int = 120,
    ) -> PendingAction:
        now = datetime.now(timezone.utc)
        await self.sweep_expired(thread_id)
        active = self._active_sync(thread_id, now)
        if active is not None:
            # one-active: production enforces via partial unique index
            active.consumed_at = now
        action = PendingAction(
            id=str(uuid.uuid4()),
            thread_id=thread_id,
            run_id=run_id,
            kind=kind,
            payload=payload,
            expires_at=now + timedelta(seconds=ttl_seconds),
        )
        self._by_thread[thread_id] = action
        return action

    async def consume(self, thread_id: str, action_id: str) -> PendingAction | None:
        now = datetime.now(timezone.utc)
        p = self._by_thread.get(thread_id)
        if p is None or p.id != action_id or p.consumed_at is not None:
            return None
        if p.expires_at <= now:
            return None
        p.consumed_at = now
        return p

    async def clear(self, thread_id: str) -> None:
        p = self._by_thread.get(thread_id)
        if p is not None and p.consumed_at is None:
            p.consumed_at = datetime.now(timezone.utc)

    @contextlib.asynccontextmanager
    async def turn(self, thread_id: str) -> AsyncIterator[None]:
        """No-op turn scope (Postgres holds the per-thread advisory lock here)."""
        del thread_id
        yield


ExecuteFn = Callable[[ConverseState, PendingAction], Awaitable[str]]

CORE_RETRY_REPLY = (
    "I couldn't reach the trading core just now — your pending action is "
    "still active. Reply yes to retry or cancel to drop it."
)


def _fmt_usd(micro: int) -> str:
    return f"${micro / 1_000_000:.2f}"


def build_graph(
    *,
    router: AgentNode,
    recorder: Recorder | None = None,
    pending_store: PendingStoreProtocol | None = None,
    execute_fn: ExecuteFn | None = None,
    checkpointer: Any | None = None,
    core: Any | None = None,
):
    """Build the conversation graph with the locked R2 topology.

    With `core` injected, preview/execute call the real core API; without it
    the Phase-0 stubs keep unit tests hermetic.
    """
    rec = recorder or MemoryRecorder()
    store: PendingStoreProtocol = pending_store or PendingStore()
    cp = checkpointer if checkpointer is not None else MemorySaver()
    execute_call_log: list[str] = []

    async def default_execute(state: ConverseState, action: PendingAction) -> str:
        kind = action.kind
        return f"Executed {kind}: {action.payload}"

    async def core_execute(state: ConverseState, action: PendingAction) -> str:
        """The ONLY money-moving call path; idempotency_key = pending id."""
        user_id = uuid.UUID(str(state["user_id"]))
        run_id = uuid.UUID(str(state["run_id"]))
        payload = action.payload
        if action.kind == "vote":
            receipt = await core.cast_vote(
                user_id=user_id,
                market_ref=str(payload["market_ref"]),
                side=str(payload["side"]),
                crowd_guess_pct=int(payload["crowd_guess_pct"]),
                idempotency_key=action.id,
                run_id=run_id,
                region=state.get("kyc_region"),
            )
            replayed = " (replayed)" if receipt.replayed else ""
            return (
                f"Vote cast: {str(payload['side']).upper()} on "
                f"{payload['market_ref']} — you are voter #{receipt.seq}{replayed}."
            )
        expected_config_version = payload.get("config_version")
        if expected_config_version is None:
            return "Can't place that trade: expected_config_version is required."
        receipt = await core.place_trade(
            user_id=user_id,
            market_ref=str(payload["market_ref"]),
            side=str(payload["side"]),
            action=str(payload.get("action") or "buy"),
            amount_micro=int(payload["amount_usd_micro"]),
            idempotency_key=action.id,
            run_id=run_id,
            pending_action_id=uuid.UUID(action.id),
            expected_config_version=int(expected_config_version),
            region=state.get("kyc_region"),
        )
        replayed = " (replayed)" if receipt.replayed else ""
        return (
            f"Executed: bought {receipt.shares_micro} micro-shares of "
            f"{str(payload['side']).upper()} on {payload['market_ref']} for "
            f"{_fmt_usd(receipt.gross_micro)} (fee {_fmt_usd(receipt.fee_micro)})"
            f"{replayed}."
        )

    if execute_fn is not None:
        do_execute: ExecuteFn = execute_fn
    elif core is not None:
        do_execute = core_execute
    else:
        do_execute = default_execute

    async def load_session(state: ConverseState) -> dict[str, Any]:
        before = dict(state)
        turn = int(state.get("turn_counter") or 0) + 1
        # Every inbound turn is its own agent run: the previous run_id lives on
        # in checkpointed thread state, so reusing it would violate the
        # agent_runs primary key on the second turn (found by the pg wiring).
        run_id = str(uuid.uuid4())
        phone = state["phone"]
        user_id = state.get("user_id")
        onboarding = False
        if core is not None and not user_id:
            # Identity: phone → user via the core; no auto-created accounts.
            found = await core.user_by_channel("imessage", phone)
            user_id = str(found) if found is not None else None
            onboarding = user_id is None
        await rec.start_run(
            run_id=run_id,
            thread_id=phone,
            channel=state.get("channel") or "sendblue_imessage",
            trigger="webhook",
            user_id=user_id,
            inbound_msg_id=state.get("message_id"),
        )
        after = {
            "turn_counter": turn,
            "run_id": run_id,
            "user_id": user_id,
            "onboarding": onboarding,
            "executed": False,
            "execute_called": False,
            "pending_cleared": False,
            "pending_kept": False,
            "gate_outcome": "",
            "intent": None,
            "reply": "",
            "pending_id": None,
            "trade_params": None,
            "vote_params": None,
        }
        await rec.record_step(
            run_id=run_id,
            seq=1,
            node="load_session",
            kind="tool",
            state_before=before,
            state_after={**before, **after},
        )
        return after

    async def pending_gate(state: ConverseState) -> dict[str, Any]:
        before = dict(state)
        phone = state["phone"]
        content = state.get("content") or ""
        normalized = normalize_text(content)
        out: dict[str, Any] = {"normalized": normalized}

        if state.get("onboarding"):
            out["gate_outcome"] = "onboarding"
            out["reply"] = ONBOARDING_REPLY
        else:
            # Expiry sweep ALWAYS runs first (expired rows must never wedge
            # the one-active index).
            await store.sweep_expired(phone)
            active = await store.get_active(phone)
            if active is None:
                out["gate_outcome"] = "route"
            elif normalized in CONFIRM_ALLOWLIST:
                # Consume-after-success (grok-p1r1 B1): SELECT only — the
                # consume happens in execute_pending after the core 2xx.
                out["gate_outcome"] = "execute"
                out["pending_id"] = active.id
            elif normalized in CANCEL_ALLOWLIST:
                await store.clear(phone)
                out["gate_outcome"] = "clear"
                out["pending_cleared"] = True
                out["reply"] = "Cancelled."
            elif normalized == "":
                out["gate_outcome"] = "reprompt"
                out["pending_kept"] = True
                out["reply"] = "Reply yes or cancel to confirm your pending action."
            else:
                await store.clear(phone)
                out["gate_outcome"] = "route"
                out["pending_cleared"] = True

        await rec.record_step(
            run_id=state["run_id"],
            seq=2,
            node="pending_gate",
            kind="gate",
            state_before=before,
            state_after={**before, **out},
        )
        return out

    async def execute_pending(state: ConverseState) -> dict[str, Any]:
        before = dict(state)
        phone = state["phone"]
        pending_id = state.get("pending_id")
        active = await store.get_active(phone)
        if active is None or (pending_id and active.id != pending_id):
            out: dict[str, Any] = {
                "reply": "Nothing pending.",
                "executed": False,
                "execute_called": False,
            }
        else:
            execute_call_log.append(active.id)
            try:
                reply = await do_execute(state, active)
            except CoreError as exc:
                if (
                    exc.status_code == 409
                    and exc.code == "StaleConfig"
                    and active.kind == "trade"
                    and core is not None
                ):
                    # D25a converse contract: a stale yes NEVER executes the
                    # old pending. Expire it, take a fresh preview at the new
                    # generation, and require a NEW lexical yes (grok r2 N8:
                    # never re-loop the same version).
                    await store.clear(phone)
                    parked = await preview_and_park(
                        phone=phone,
                        run_id=state["run_id"],
                        user_id=uuid.UUID(str(state["user_id"])),
                        params=dict(active.payload),
                    )
                    if isinstance(parked, str):
                        out = {
                            "reply": parked,
                            "executed": False,
                            "execute_called": True,
                            "pending_cleared": True,
                        }
                    else:
                        _new_action, confirm = parked
                        out = {
                            "reply": (
                                "Prices or limits changed since your preview, "
                                "so I did not place that trade. New "
                                f"{confirm[0].lower()}{confirm[1:]}"
                            ),
                            "executed": False,
                            "execute_called": True,
                            "pending_cleared": True,
                        }
                else:
                    # Core failure/timeout: do NOT consume; the user
                    # re-confirms and idempotency_key = pending id makes the
                    # retry replay-safe.
                    out = {
                        "reply": CORE_RETRY_REPLY,
                        "executed": False,
                        "execute_called": True,
                        "pending_kept": True,
                    }
            except Exception:  # noqa: BLE001 — any failure keeps the pending
                # Core failure/timeout: do NOT consume; the user re-confirms
                # and idempotency_key = pending id makes the retry replay-safe.
                out = {
                    "reply": CORE_RETRY_REPLY,
                    "executed": False,
                    "execute_called": True,
                    "pending_kept": True,
                }
            else:
                await store.consume(phone, active.id)
                out = {
                    "reply": reply,
                    "executed": True,
                    "execute_called": True,
                }
        await rec.record_step(
            run_id=state["run_id"],
            seq=3,
            node="execute_pending",
            kind="tool",
            state_before=before,
            state_after={**before, **out},
        )
        return out

    async def clear_pending(state: ConverseState) -> dict[str, Any]:
        # clear already applied in gate; just ensure reply
        return {"reply": state.get("reply") or "Cancelled.", "pending_cleared": True}

    async def reprompt(state: ConverseState) -> dict[str, Any]:
        return {
            "reply": state.get("reply")
            or "Reply yes or cancel to confirm your pending action.",
            "pending_kept": True,
        }

    async def router_node(state: ConverseState) -> dict[str, Any]:
        before = dict(state)
        result = await router(state.get("content") or "", dict(state))
        if not isinstance(result, RouterOutput):
            # tolerate dict-like fakes
            intent = Intent(str(getattr(result, "intent", Intent.UNKNOWN)))
            rationale = str(getattr(result, "rationale", "n/a"))
            result = RouterOutput(intent=intent, rationale=rationale)
        out: dict[str, Any] = {"intent": result.intent.value}
        trade_params = getattr(result, "trade_params", None)
        if trade_params is not None:
            out["trade_params"] = trade_params.model_dump()
        vote_params = getattr(result, "vote_params", None)
        if vote_params is not None:
            out["vote_params"] = vote_params.model_dump()
        await rec.record_step(
            run_id=state["run_id"],
            seq=3,
            node="router",
            kind="llm",
            state_before=before,
            state_after={**before, **out},
        )
        return out

    async def respond_stub(state: ConverseState) -> dict[str, Any]:
        intent = state.get("intent") or "unknown"
        return {"reply": f"Stub response for {intent}."}

    async def preview_and_park(
        *, phone: str, run_id: str, user_id: uuid.UUID, params: dict[str, Any]
    ) -> tuple[PendingAction, str] | str:
        """Fresh preview → NEW pending carrying the preview's config_version.

        Returns the parked action + confirm reply, or an error reply string.
        Used by the trade_preview node AND the 409-StaleConfig re-preview
        (D25a): the pending payload retains `config_version` so execute can
        send `expected_config_version`.
        """
        try:
            preview = await core.preview_trade(
                user_id=user_id,
                market_ref=str(params["market_ref"]),
                side=str(params["side"]),
                action=str(params.get("action") or "buy"),
                amount_micro=int(params["amount_usd_micro"]),
            )
        except CoreError as exc:
            return f"Can't preview that trade: {exc.message}"
        action = await store.create(
            thread_id=phone,
            run_id=run_id,
            kind="trade",
            payload={
                "market_ref": params["market_ref"],
                "side": params["side"],
                "action": params.get("action") or "buy",
                "amount_usd_micro": int(params["amount_usd_micro"]),
                "config_version": preview.config_version,
                "preview": {
                    "shares_micro": preview.shares_micro,
                    "fee_micro": preview.fee_micro,
                    "avg_price_micro": preview.avg_price_micro,
                },
            },
        )
        reply = (
            f"Preview: buy {_fmt_usd(int(params['amount_usd_micro']))} of "
            f"{str(params['side']).upper()} on {params['market_ref']} → "
            f"~{preview.shares_micro} micro-shares at "
            f"{_fmt_usd(preview.avg_price_micro)}/share, fee "
            f"{_fmt_usd(preview.fee_micro)}. Reply yes to confirm "
            f"(pending {action.id[:8]})."
        )
        return action, reply

    async def trade_preview(state: ConverseState) -> dict[str, Any]:
        phone = state["phone"]
        run_id = state["run_id"]
        if core is None:
            action = await store.create(
                thread_id=phone,
                run_id=run_id,
                kind="trade",
                payload={
                    "side": "yes",
                    "amount_usd_micro": 10_000_000,
                    "market_ref": "demo",
                },
            )
            return {
                "reply": (
                    f"Preview trade: buy YES $10 on demo. Reply yes to confirm "
                    f"(pending {action.id[:8]})."
                ),
                "intent": Intent.PLACE_TRADE.value,
            }
        params = state.get("trade_params")
        if not params:
            return {
                "reply": (
                    "Tell me the trade like: buy $5 of yes on demo-coffee "
                    "(that's all this demo understands)."
                ),
            }
        parked = await preview_and_park(
            phone=phone,
            run_id=run_id,
            user_id=uuid.UUID(str(state["user_id"])),
            params=dict(params),
        )
        if isinstance(parked, str):
            return {"reply": parked}
        _action, reply = parked
        return {"reply": reply}

    async def vote_preview(state: ConverseState) -> dict[str, Any]:
        phone = state["phone"]
        run_id = state["run_id"]
        if core is None:
            action = await store.create(
                thread_id=phone,
                run_id=run_id,
                kind="vote",
                payload={"side": "yes", "crowd_guess_pct": 65, "market_ref": "demo"},
            )
            return {
                "reply": (
                    f"Preview vote: YES at 65%. Reply yes to confirm "
                    f"(pending {action.id[:8]})."
                ),
                "intent": Intent.VOTE.value,
            }
        params = state.get("vote_params")
        if not params:
            return {
                "reply": (
                    "Tell me the vote like: vote yes 60 on demo-coffee "
                    "(that's all this demo understands)."
                ),
            }
        action = await store.create(
            thread_id=phone,
            run_id=run_id,
            kind="vote",
            payload={
                "market_ref": params["market_ref"],
                "side": params["side"],
                "crowd_guess_pct": int(params["crowd_guess_pct"]),
            },
        )
        return {
            "reply": (
                f"Preview vote: {str(params['side']).upper()} on "
                f"{params['market_ref']} with crowd guess "
                f"{params['crowd_guess_pct']}%. Reply yes to confirm "
                f"(pending {action.id[:8]})."
            ),
        }

    async def compose_stub(state: ConverseState) -> dict[str, Any]:
        before = dict(state)
        reply = state.get("reply") or "OK."
        out = {"reply": reply}
        await rec.record_step(
            run_id=state["run_id"],
            seq=99,
            node="compose_stub",
            kind="llm",
            state_before=before,
            state_after={**before, **out},
        )
        await rec.end_run(
            run_id=state["run_id"],
            status="ok",
            final_intent=state.get("intent"),
        )
        return out

    def route_after_gate(state: ConverseState) -> str:
        outcome = state.get("gate_outcome") or "route"
        if outcome == "execute":
            return "execute_pending"
        if outcome == "clear":
            return "clear_pending"
        if outcome == "reprompt":
            return "reprompt"
        if outcome in {"nothing_pending", "onboarding"}:
            return "compose_stub"
        return "router"

    def route_after_router(state: ConverseState) -> str:
        intent = state.get("intent")
        if intent == Intent.PLACE_TRADE.value:
            return "trade_preview"
        if intent == Intent.VOTE.value:
            return "vote_preview"
        return "respond_stub"

    g = StateGraph(ConverseState)
    g.add_node("load_session", load_session)
    g.add_node("pending_gate", pending_gate)
    g.add_node("execute_pending", execute_pending)
    g.add_node("clear_pending", clear_pending)
    g.add_node("reprompt", reprompt)
    g.add_node("router", router_node)
    g.add_node("respond_stub", respond_stub)
    g.add_node("trade_preview", trade_preview)
    g.add_node("vote_preview", vote_preview)
    g.add_node("compose_stub", compose_stub)

    g.add_edge(START, "load_session")
    g.add_edge("load_session", "pending_gate")
    g.add_conditional_edges(
        "pending_gate",
        route_after_gate,
        {
            "execute_pending": "execute_pending",
            "clear_pending": "clear_pending",
            "reprompt": "reprompt",
            "compose_stub": "compose_stub",
            "router": "router",
        },
    )
    g.add_edge("execute_pending", "compose_stub")
    g.add_edge("clear_pending", "compose_stub")
    g.add_edge("reprompt", "compose_stub")
    g.add_conditional_edges(
        "router",
        route_after_router,
        {
            "trade_preview": "trade_preview",
            "vote_preview": "vote_preview",
            "respond_stub": "respond_stub",
        },
    )
    g.add_edge("trade_preview", "compose_stub")
    g.add_edge("vote_preview", "compose_stub")
    g.add_edge("respond_stub", "compose_stub")
    g.add_edge("compose_stub", END)

    compiled = g.compile(checkpointer=cp)
    # Attach diagnostics for tests
    compiled._converse_pending_store = store  # type: ignore[attr-defined]
    compiled._converse_execute_log = execute_call_log  # type: ignore[attr-defined]
    compiled._converse_recorder = rec  # type: ignore[attr-defined]
    return compiled


async def run_turn(
    graph: Any,
    *,
    phone: str,
    content: str,
    message_id: str,
    channel: str = "sendblue_imessage",
    kyc_region: str | None = None,
    config: dict | None = None,
) -> ConverseState:
    """Invoke one inbound message turn; thread_id = phone."""
    cfg = config or {"configurable": {"thread_id": phone}}
    turn_input: ConverseState = {
        "phone": phone,
        "content": content,
        "message_id": message_id,
        "channel": channel,
    }
    if kyc_region is not None:
        turn_input["kyc_region"] = kyc_region
    result = await graph.ainvoke(
        turn_input,
        config=cfg,
    )
    return result
