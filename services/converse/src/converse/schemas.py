# schemas.py — every LLM structured output carries a required rationale (spec §10.3)
# R2 (grok M4 / codex M3): there is NO confirm/cancel intent — confirmation authority
# lives in the deterministic pre-router pending gate, so the router cannot express it.
from enum import StrEnum
from typing import Protocol

from pydantic import BaseModel, Field


class Intent(StrEnum):
    LIST_MARKETS = "list_markets"
    PRICE = "price"
    PORTFOLIO = "portfolio"
    ASK_ABOUT_MARKET = "ask_about_market"
    VOTE = "vote"
    PLACE_TRADE = "place_trade"
    CHITCHAT = "chitchat"
    UNKNOWN = "unknown"


class RouterOutput(BaseModel):
    intent: Intent
    rationale: str = Field(min_length=1)


class TradeParams(BaseModel):
    market_ref: str
    side: str = Field(pattern="^(yes|no)$")
    amount_usd_micro: int = Field(gt=0)
    action: str = Field(pattern="^(buy|sell)$")
    rationale: str = Field(min_length=1)


class VoteParams(BaseModel):
    market_ref: str
    side: str = Field(pattern="^(yes|no)$")
    crowd_guess_pct: int = Field(ge=0, le=100)
    rationale: str = Field(min_length=1)


class ComposerOutput(BaseModel):
    text: str
    numbers_used: list[int]  # every numeric figure in `text`, for the output guard
    rationale: str = Field(min_length=1)


class AgentNode(Protocol):
    """Shared async LLM seam — provider-agnostic (R1/codex ckpt 7)."""

    async def __call__(self, rendered_prompt: str, context: dict) -> BaseModel: ...
