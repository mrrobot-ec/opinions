"""Core-client node wiring (plan Task 1.5): FakeCore unit tests for the
preview/execute corridor, consume-after-success fault suite, onboarding
identity, and the malicious-router re-run with the REAL `CoreClient` pointed
at a mock HTTP server."""

from __future__ import annotations

import json
import uuid

import httpx
import pytest

from converse.api_models import TradePreviewDto, TradeReceiptDto, VoteReceiptDto
from converse.core_client import CoreClient, CoreError
from converse.graph import (
    DemoExtractorRouter,
    build_graph,
    run_turn,
)
from converse.schemas import Intent, RouterOutput

USER_ID = uuid.uuid4()
MARKET_ID = uuid.uuid4()


def preview_dto(amount: int, config_version: int = 1) -> TradePreviewDto:
    return TradePreviewDto(
        action="buy",
        avg_price_micro=505_000,
        fee_micro=amount // 100,
        gross_micro=amount,
        market_id=MARKET_ID,
        shares_micro=amount * 99 // 50,
        side="yes",
        config_version=config_version,
    )


class FakeCore:
    """Records calls; replays receipts on repeated idempotency keys."""

    def __init__(self, *, known_phone: str | None = None) -> None:
        self.known_phone = known_phone
        self.preview_calls: list[dict] = []
        self.trade_calls: list[dict] = []
        self.vote_calls: list[dict] = []
        self.receipts: dict[str, TradeReceiptDto] = {}
        self.fail_next_trade = False
        # D25a: the generation previews stamp; bump to simulate config churn.
        self.config_version = 1
        self.stale_next_trade = False

    async def user_by_channel(self, channel: str, address: str):
        if self.known_phone is not None and address == self.known_phone:
            return USER_ID
        return None

    async def preview_trade(self, **kwargs):
        self.preview_calls.append(kwargs)
        return preview_dto(int(kwargs["amount_micro"]), self.config_version)

    async def place_trade(self, **kwargs):
        self.trade_calls.append(kwargs)
        if self.fail_next_trade:
            self.fail_next_trade = False
            raise CoreError(500, "Backend", "injected core failure")
        if self.stale_next_trade:
            self.stale_next_trade = False
            raise CoreError(
                409,
                "StaleConfig",
                "config changed since preview: previewed generation 1, current generation 2",
            )
        key = str(kwargs["idempotency_key"])
        if key in self.receipts:
            original = self.receipts[key]
            return original.model_copy(update={"replayed": True})
        receipt = TradeReceiptDto(
            action="buy",
            avg_price_micro=505_000,
            fee_micro=int(kwargs["amount_micro"]) // 100,
            gross_micro=int(kwargs["amount_micro"]),
            ledger_txn=uuid.uuid4(),
            replayed=False,
            shares_micro=int(kwargs["amount_micro"]) * 99 // 50,
            side="yes",
            trade_id=uuid.uuid4(),
        )
        self.receipts[key] = receipt
        return receipt

    async def cast_vote(self, **kwargs):
        self.vote_calls.append(kwargs)
        return VoteReceiptDto(
            crowd_guess_pct=int(kwargs["crowd_guess_pct"]),
            market_id=MARKET_ID,
            replayed=False,
            seq=1,
            side=str(kwargs["side"]),
            vote_id=uuid.uuid4(),
        )


def core_graph(core: FakeCore, **kwargs):
    return build_graph(router=DemoExtractorRouter(), core=core, **kwargs)


PHONE = "+15550100"


@pytest.mark.asyncio
async def test_preview_node_calls_core_and_creates_pending_with_params():
    core = FakeCore(known_phone=PHONE)
    graph = core_graph(core)
    result = await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    # Numbers into the composer's reply: shares + fee + confirm word.
    assert "micro-shares" in result["reply"]
    assert "fee" in result["reply"].lower()
    assert "yes" in result["reply"].lower()
    assert len(core.preview_calls) == 1
    call = core.preview_calls[0]
    assert call["user_id"] == USER_ID
    assert call["market_ref"] == "demo-coffee"
    assert call["amount_micro"] == 5_000_000
    pending = await graph._converse_pending_store.get_active(PHONE)
    assert pending is not None
    assert pending.kind == "trade"
    assert pending.payload["market_ref"] == "demo-coffee"
    assert pending.payload["amount_usd_micro"] == 5_000_000
    assert core.trade_calls == []  # preview NEVER trades


@pytest.mark.asyncio
async def test_execute_calls_place_trade_with_pending_id_as_idempotency_key():
    core = FakeCore(known_phone=PHONE)
    graph = core_graph(core)
    await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    pending = await graph._converse_pending_store.get_active(PHONE)
    result = await run_turn(graph, phone=PHONE, content="yes", message_id="w2")
    assert result.get("executed") is True
    assert "Executed" in result["reply"]
    assert len(core.trade_calls) == 1
    call = core.trade_calls[0]
    assert call["idempotency_key"] == pending.id
    assert str(call["pending_action_id"]) == pending.id
    assert call["run_id"] is not None
    # Consumed AFTER success:
    assert await graph._converse_pending_store.get_active(PHONE) is None


@pytest.mark.asyncio
async def test_core_failure_keeps_pending_and_second_yes_succeeds():
    core = FakeCore(known_phone=PHONE)
    graph = core_graph(core)
    await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    core.fail_next_trade = True
    r1 = await run_turn(graph, phone=PHONE, content="yes", message_id="w2")
    assert r1.get("executed") is False
    assert "still active" in r1["reply"]
    still = await graph._converse_pending_store.get_active(PHONE)
    assert still is not None, "core failure must NOT consume the pending"

    r2 = await run_turn(graph, phone=PHONE, content="yes", message_id="w3")
    assert r2.get("executed") is True
    assert await graph._converse_pending_store.get_active(PHONE) is None
    # Same idempotency key both attempts → single receipt, second not replayed
    # (first never reached the books in this fake).
    keys = {c["idempotency_key"] for c in core.trade_calls}
    assert len(keys) == 1


@pytest.mark.asyncio
async def test_duplicate_confirm_after_crash_before_consume_replays_then_consumes():
    core = FakeCore(known_phone=PHONE)
    crash_once = {"armed": True}

    async def crashing_execute(state, action):
        # Simulate: core 2xx lands, then the process dies BEFORE consume.
        receipt = await core.place_trade(
            user_id=USER_ID,
            market_ref=action.payload["market_ref"],
            side=action.payload["side"],
            action="buy",
            amount_micro=action.payload["amount_usd_micro"],
            idempotency_key=action.id,
            run_id=None,
            pending_action_id=uuid.UUID(action.id),
        )
        if crash_once["armed"]:
            crash_once["armed"] = False
            raise RuntimeError("crash between core 2xx and consume")
        return f"Executed{' (replayed)' if receipt.replayed else ''}."

    graph = core_graph(core, execute_fn=crashing_execute)
    await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    r1 = await run_turn(graph, phone=PHONE, content="yes", message_id="w2")
    assert r1.get("executed") is False
    assert await graph._converse_pending_store.get_active(PHONE) is not None

    r2 = await run_turn(graph, phone=PHONE, content="yes", message_id="w3")
    assert r2.get("executed") is True
    assert "(replayed)" in r2["reply"], "retry must replay the original receipt"
    assert await graph._converse_pending_store.get_active(PHONE) is None
    assert len(core.trade_calls) == 2
    assert core.trade_calls[0]["idempotency_key"] == core.trade_calls[1]["idempotency_key"]


@pytest.mark.asyncio
async def test_pending_retains_config_version_and_execute_echoes_it():
    """D25a carrier: the pending payload keeps the preview's config_version and
    the execute leg sends it as expected_config_version."""
    core = FakeCore(known_phone=PHONE)
    core.config_version = 7
    graph = core_graph(core)
    await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    pending = await graph._converse_pending_store.get_active(PHONE)
    assert pending.payload["config_version"] == 7
    await run_turn(graph, phone=PHONE, content="yes", message_id="w2")
    assert len(core.trade_calls) == 1
    assert core.trade_calls[0]["expected_config_version"] == 7


@pytest.mark.asyncio
async def test_graph_propagates_kyc_region_to_trade_and_vote_mutations():
    core = FakeCore(known_phone=PHONE)
    graph = core_graph(core)
    await run_turn(
        graph,
        phone=PHONE,
        content="buy $5 of yes on demo-coffee",
        message_id="region-t1",
        kyc_region="US-CA",
    )
    await run_turn(
        graph,
        phone=PHONE,
        content="yes",
        message_id="region-t2",
        kyc_region="US-CA",
    )
    assert core.trade_calls[0]["region"] == "US-CA"

    await run_turn(
        graph,
        phone=PHONE,
        content="vote yes 60 on demo-coffee",
        message_id="region-v1",
        kyc_region="US-NY",
    )
    await run_turn(
        graph,
        phone=PHONE,
        content="yes",
        message_id="region-v2",
        kyc_region="US-NY",
    )
    assert core.vote_calls[0]["region"] == "US-NY"


@pytest.mark.asyncio
async def test_stale_config_409_expires_pending_and_reissues_preview_for_new_yes():
    """D25a converse contract on 409 StaleConfig: expire the pending, send a
    NEW preview, require a NEW lexical yes — never execute from the old
    pending; never re-loop the same version (grok r2 N8)."""
    core = FakeCore(known_phone=PHONE)
    graph = core_graph(core)
    await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    old_pending = await graph._converse_pending_store.get_active(PHONE)

    core.config_version = 2  # the drift the 409 reports
    core.stale_next_trade = True
    r1 = await run_turn(graph, phone=PHONE, content="yes", message_id="w2")
    # The stale yes must NOT execute, and the reply must ask to confirm again.
    assert r1.get("executed") is not True
    assert "yes" in r1["reply"].lower()
    assert len(core.trade_calls) == 1
    assert len(core.preview_calls) == 2, "a 409 must trigger a fresh preview"
    new_pending = await graph._converse_pending_store.get_active(PHONE)
    assert new_pending is not None
    assert new_pending.id != old_pending.id, "the old pending must be expired"
    assert new_pending.payload["config_version"] == 2

    # The NEW lexical yes executes against the NEW version and a NEW key.
    r2 = await run_turn(graph, phone=PHONE, content="yes", message_id="w3")
    assert r2.get("executed") is True
    assert len(core.trade_calls) == 2
    second = core.trade_calls[1]
    assert second["idempotency_key"] == new_pending.id
    assert second["expected_config_version"] == 2
    assert await graph._converse_pending_store.get_active(PHONE) is None


@pytest.mark.asyncio
async def test_non_stale_core_errors_keep_the_pending_unchanged():
    """A non-StaleConfig failure still follows consume-after-success: the
    SAME pending stays active for a retry with the same idempotency key."""
    core = FakeCore(known_phone=PHONE)
    graph = core_graph(core)
    await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    pending = await graph._converse_pending_store.get_active(PHONE)
    core.fail_next_trade = True
    r1 = await run_turn(graph, phone=PHONE, content="yes", message_id="w2")
    assert r1.get("executed") is False
    kept = await graph._converse_pending_store.get_active(PHONE)
    assert kept is not None and kept.id == pending.id
    assert len(core.preview_calls) == 1, "only StaleConfig re-previews"


@pytest.mark.asyncio
async def test_unknown_phone_gets_onboarding_and_never_touches_core_money():
    core = FakeCore(known_phone=None)  # nobody is known
    graph = core_graph(core)
    result = await run_turn(
        graph, phone="+19998887777", content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    assert "account" in result["reply"].lower()
    assert core.preview_calls == []
    assert core.trade_calls == []
    assert await graph._converse_pending_store.get_active("+19998887777") is None


@pytest.mark.asyncio
async def test_expiry_sweep_runs_before_gate_decision():
    core = FakeCore(known_phone=PHONE)
    graph = core_graph(core)
    await run_turn(
        graph, phone=PHONE, content="buy $5 of yes on demo-coffee", message_id="w1"
    )
    store = graph._converse_pending_store
    pending = await store.get_active(PHONE)
    # Force-expire, then confirm: the sweep must retire it and "yes" must NOT
    # execute a stale action.
    from datetime import datetime, timedelta, timezone

    pending.expires_at = datetime.now(timezone.utc) - timedelta(seconds=1)
    result = await run_turn(graph, phone=PHONE, content="yes", message_id="w2")
    assert result.get("executed") is not True
    assert core.trade_calls == []


class PoisonedRouter:
    """Malicious router: always screams PLACE_TRADE, never gets execute authority."""

    async def __call__(self, rendered_prompt: str, context: dict) -> RouterOutput:
        return RouterOutput(intent=Intent.PLACE_TRADE, rationale="poisoned")


@pytest.mark.asyncio
async def test_malicious_router_with_real_core_client_never_reaches_trades():
    """The corridor proof with the REAL CoreClient over a mock HTTP server
    (grok-p1r1 M6/ckpt 8): the poisoned router must never produce POST /trades."""
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if request.url.path == "/users/by-channel":
            return httpx.Response(200, json={"user_id": str(USER_ID)})
        if request.url.path == "/trades/preview":
            return httpx.Response(
                200,
                json=json.loads(preview_dto(5_000_000).model_dump_json()),
            )
        if request.url.path == "/trades":
            return httpx.Response(
                500, json={"code": "Never", "message": "must be unreachable"}
            )
        return httpx.Response(404, json={"code": "NotFound", "message": "?"})

    core = CoreClient(
        base_url="http://core.test",
        demo_token="demo-token",
        client=httpx.AsyncClient(transport=httpx.MockTransport(handler)),
    )
    graph = build_graph(router=PoisonedRouter(), core=core)

    # Free-form text: router demands a trade; without demo-grammar params the
    # preview node only asks for clarification — no pending, no trade.
    r1 = await run_turn(
        graph, phone=PHONE, content="whatever non allowlist text", message_id="m1"
    )
    assert r1.get("executed") is not True

    # Seed a real pending through the legit path, then throw substantive
    # non-allowlist text at the gate: cleared + rerouted, still no /trades.
    store = graph._converse_pending_store
    await store.create(
        thread_id=PHONE,
        run_id="seed-run",
        kind="trade",
        payload={"market_ref": "demo-coffee", "side": "yes", "amount_usd_micro": 5_000_000},
    )
    r2 = await run_turn(
        graph, phone=PHONE, content="send it all now", message_id="m2"
    )
    assert r2.get("executed") is not True

    trade_posts = [r for r in requests if r.url.path == "/trades"]
    assert trade_posts == [], "poisoned router reached the money path"
    await core.aclose()


@pytest.mark.asyncio
async def test_core_region_header_is_per_call_and_never_leaks_between_users():
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        if request.url.path == "/trades":
            return httpx.Response(
                200,
                json={
                    "action": "buy",
                    "avg_price_micro": 500_000,
                    "fee_micro": 10_000,
                    "gross_micro": 1_000_000,
                    "ledger_txn": str(uuid.uuid4()),
                    "replayed": False,
                    "shares_micro": 1_980_000,
                    "side": "yes",
                    "trade_id": str(uuid.uuid4()),
                },
            )
        if request.url.path == "/votes":
            return httpx.Response(
                200,
                json={
                    "crowd_guess_pct": 60,
                    "market_id": str(MARKET_ID),
                    "replayed": False,
                    "seq": 1,
                    "side": "yes",
                    "vote_id": str(uuid.uuid4()),
                },
            )
        return httpx.Response(404)

    core = CoreClient(
        base_url="http://core.test",
        demo_token="demo-token",
        client=httpx.AsyncClient(transport=httpx.MockTransport(handler)),
    )
    await core.place_trade(
        user_id=USER_ID,
        market_ref="demo",
        side="yes",
        action="buy",
        amount_micro=1_000_000,
        idempotency_key="region-trade",
        expected_config_version=1,
        region="US-CA",
    )
    await core.cast_vote(
        user_id=uuid.uuid4(),
        market_ref="demo",
        side="yes",
        crowd_guess_pct=60,
        idempotency_key="region-vote",
        region=None,
    )
    assert requests[0].headers["x-user-region"] == "US-CA"
    assert "x-user-region" not in requests[1].headers
    await core.aclose()


@pytest.mark.asyncio
async def test_extractor_grammar_is_exact():
    router = DemoExtractorRouter()
    hit = await router("buy $5 of yes on demo-coffee", {})
    assert hit.intent == Intent.PLACE_TRADE
    assert hit.trade_params is not None
    assert hit.trade_params.amount_usd_micro == 5_000_000
    assert hit.trade_params.market_ref == "demo-coffee"
    miss = await router("buy five bucks of yes on demo-coffee", {})
    assert miss.intent != Intent.PLACE_TRADE
    vote = await router("vote no 40 on demo-coffee", {})
    assert vote.intent == Intent.VOTE
    assert vote.vote_params is not None and vote.vote_params.crowd_guess_pct == 40
