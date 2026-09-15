"""Graph + lexical pending-gate tests — PLAN Task 8 Step 3."""

from __future__ import annotations

import pytest

from converse.graph import PendingStore, build_graph, normalize_text, run_turn
from converse.recorder import MemoryRecorder
from converse.schemas import Intent, RouterOutput


class FakeRouter:
    def __init__(self, intent: Intent = Intent.PRICE) -> None:
        self.intent = intent
        self.calls = 0

    async def __call__(self, rendered_prompt: str, context: dict) -> RouterOutput:
        self.calls += 1
        return RouterOutput(intent=self.intent, rationale="fake")


class AlwaysTradeRouter:
    async def __call__(self, rendered_prompt: str, context: dict) -> RouterOutput:
        return RouterOutput(intent=Intent.PLACE_TRADE, rationale="malicious")


def test_normalize_confirm_variants():
    assert normalize_text("Confirm!") == "confirm"
    assert normalize_text("yes.") == "yes"
    assert normalize_text("YES please") == "yes"
    assert normalize_text("do it") == "do it"
    assert normalize_text("👍") == ""
    assert normalize_text("") == ""
    assert normalize_text("yes but $20") == "yes but 20"


@pytest.mark.asyncio
async def test_price_intent_reply():
    router = FakeRouter(Intent.PRICE)
    rec = MemoryRecorder()
    graph = build_graph(router=router, recorder=rec)
    result = await run_turn(
        graph,
        phone="+15551234567",
        content="price on the coffee market?",
        message_id="m1",
    )
    assert result.get("intent") == "price"
    assert result.get("reply")
    assert "price" in result["reply"].lower() or "Stub" in result["reply"]


@pytest.mark.asyncio
async def test_checkpointer_turn_counter_restores():
    router = FakeRouter(Intent.CHITCHAT)
    graph = build_graph(router=router)
    phone = "+15550001111"
    r1 = await run_turn(graph, phone=phone, content="hi", message_id="a1")
    r2 = await run_turn(graph, phone=phone, content="hi again", message_id="a2")
    assert r1["turn_counter"] == 1
    assert r2["turn_counter"] == 2


async def _seed_pending(graph, phone: str, msg: str = "buy 10 yes") -> None:
    """Route a place_trade to create a pending action via trade_preview_stub."""
    # Temporarily use AlwaysTradeRouter by replacing — use store.create via first turn
    store: PendingStore = graph._converse_pending_store
    await store.create(
        thread_id=phone,
        run_id="seed-run",
        kind="trade",
        payload={"side": "yes", "amount_usd_micro": 10_000_000},
    )


@pytest.mark.asyncio
async def test_adversarial_gate_confirm_bang_executes():
    router = AlwaysTradeRouter()
    graph = build_graph(router=router)
    phone = "+15550002222"
    await _seed_pending(graph, phone)
    result = await run_turn(graph, phone=phone, content="Confirm!", message_id="c1")
    assert result.get("executed") is True
    assert result.get("execute_called") is True
    assert result.get("gate_outcome") == "execute"


@pytest.mark.asyncio
async def test_adversarial_gate_yes_dot_executes():
    router = AlwaysTradeRouter()
    graph = build_graph(router=router)
    phone = "+15550003333"
    await _seed_pending(graph, phone)
    result = await run_turn(graph, phone=phone, content="yes.", message_id="c2")
    assert result.get("executed") is True


@pytest.mark.asyncio
async def test_adversarial_gate_cancel_clears():
    router = FakeRouter(Intent.PRICE)
    graph = build_graph(router=router)
    phone = "+15550004444"
    await _seed_pending(graph, phone)
    result = await run_turn(graph, phone=phone, content="cancel", message_id="c3")
    assert result.get("pending_cleared") is True
    assert result.get("executed") is not True
    assert await graph._converse_pending_store.get_active(phone) is None


@pytest.mark.asyncio
async def test_adversarial_gate_emoji_reprompts_keeps_pending():
    router = AlwaysTradeRouter()
    graph = build_graph(router=router)
    phone = "+15550005555"
    await _seed_pending(graph, phone)
    result = await run_turn(graph, phone=phone, content="👍", message_id="c4")
    assert result.get("pending_kept") is True
    assert result.get("gate_outcome") == "reprompt"
    assert result.get("executed") is not True
    assert await graph._converse_pending_store.get_active(phone) is not None


@pytest.mark.asyncio
async def test_adversarial_gate_empty_reprompts_keeps_pending():
    router = AlwaysTradeRouter()
    graph = build_graph(router=router)
    phone = "+15550006666"
    await _seed_pending(graph, phone)
    result = await run_turn(graph, phone=phone, content="", message_id="c5")
    assert result.get("pending_kept") is True
    assert await graph._converse_pending_store.get_active(phone) is not None


@pytest.mark.asyncio
async def test_adversarial_gate_yes_but_amount_cancels_then_routes():
    router = AlwaysTradeRouter()
    graph = build_graph(router=router)
    phone = "+15550007777"
    await _seed_pending(graph, phone)
    result = await run_turn(graph, phone=phone, content="yes but $20", message_id="c6")
    assert result.get("pending_cleared") is True
    assert result.get("executed") is not True
    # re-routed to place_trade → new preview pending may exist
    assert result.get("gate_outcome") == "route"


@pytest.mark.asyncio
async def test_malicious_router_never_reaches_execute_on_non_allowlist():
    execute_calls: list[str] = []

    async def tracking_execute(state, action):
        execute_calls.append(action.id)
        return "should-not-run"

    router = AlwaysTradeRouter()
    graph = build_graph(router=router, execute_fn=tracking_execute)
    phone = "+15550008888"
    # No pending — free-form message; router wants trade but cannot execute
    result = await run_turn(
        graph, phone=phone, content="whatever non allowlist text", message_id="c7"
    )
    assert execute_calls == []
    assert result.get("execute_called") is not True

    # With pending + non-allowlist substantive text: cancel + route, still no execute
    await _seed_pending(graph, phone)
    result2 = await run_turn(
        graph, phone=phone, content="show me portfolio now", message_id="c8"
    )
    assert execute_calls == []
    assert result2.get("execute_called") is not True
