"""Schema contract tests — PLAN Task 8 Step 2."""

import pytest
from pydantic import ValidationError

from converse.schemas import (
    ComposerOutput,
    Intent,
    RouterOutput,
    TradeParams,
    VoteParams,
)


def test_intent_vote_round_trips():
    assert Intent("vote") is Intent.VOTE
    assert Intent.VOTE.value == "vote"


def test_intent_has_no_confirm_or_cancel():
    names = {i.name for i in Intent}
    assert "CONFIRM" not in names
    assert "CANCEL" not in names


def test_trade_params_rejects_zero_amount():
    with pytest.raises(ValidationError):
        TradeParams(
            market_ref="m",
            side="yes",
            amount_usd_micro=0,
            action="buy",
            rationale="x",
        )


def test_trade_params_rejects_missing_rationale():
    with pytest.raises(ValidationError):
        TradeParams(
            market_ref="m",
            side="yes",
            amount_usd_micro=1,
            action="buy",
            rationale="",
        )


def test_trade_params_rejects_invalid_side():
    with pytest.raises(ValidationError):
        TradeParams(
            market_ref="m",
            side="maybe",
            amount_usd_micro=1,
            action="buy",
            rationale="x",
        )


def test_router_output_requires_rationale():
    with pytest.raises(ValidationError):
        RouterOutput(intent=Intent.PRICE, rationale="")


def test_vote_and_composer_ok():
    v = VoteParams(market_ref="m", side="no", crowd_guess_pct=50, rationale="guess")
    assert v.crowd_guess_pct == 50
    c = ComposerOutput(text="hi", numbers_used=[], rationale="ok")
    assert c.text == "hi"
