"""Typed httpx client for the core API — the only path converse moves money.

Every payload and response rides the generated `api_models` (the OpenAPI
freshness gate keeps them from drifting). Injected into the graph via
`build_graph(core=...)`; unit tests use a `FakeCore`, the e2e exercises this
one against the real server.
"""

from __future__ import annotations

import os
import uuid
from typing import Any

import httpx

from converse.api_models import (
    CastVoteRequest,
    MarketSummaryDto,
    PlaceTradeRequest,
    PositionDto,
    PreviewTradeRequest,
    SideDto,
    TradeActionDto,
    TradePreviewDto,
    TradeReceiptDto,
    UserIdDto,
    VoteReceiptDto,
)


class CoreError(RuntimeError):
    """Non-2xx from the core; carries the ApiError envelope when present."""

    def __init__(self, status_code: int, code: str, message: str) -> None:
        super().__init__(f"core {status_code} {code}: {message}")
        self.status_code = status_code
        self.code = code
        self.message = message


class CoreClient:
    """Thin async wrapper; one method per route the corridor needs."""

    def __init__(
        self,
        *,
        base_url: str | None = None,
        demo_token: str | None = None,
        client: httpx.AsyncClient | None = None,
    ) -> None:
        self.base_url = (
            base_url or os.environ.get("CORE_API_URL") or "http://127.0.0.1:8080"
        ).rstrip("/")
        self._token = demo_token or os.environ.get("DEMO_TOKEN") or "demo-token"
        self._client = client or httpx.AsyncClient(timeout=10.0)
    def _headers(self, region: str | None = None) -> dict[str, str]:
        headers = {"x-demo-token": self._token}
        if region:
            headers["x-user-region"] = region
        return headers

    async def aclose(self) -> None:
        await self._client.aclose()

    async def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: dict[str, Any] | None = None,
        params: dict[str, str] | None = None,
        region: str | None = None,
    ) -> httpx.Response:
        try:
            resp = await self._client.request(
                method,
                f"{self.base_url}{path}",
                json=json_body,
                params=params,
                headers=self._headers(region),
            )
        except httpx.HTTPError as exc:  # network/timeout → corridor keeps pending
            raise CoreError(0, "Unreachable", str(exc)) from exc
        if resp.status_code >= 400:
            code, message = "Unknown", resp.text[:200]
            try:
                envelope = resp.json()
                code = str(envelope.get("code", code))
                message = str(envelope.get("message", message))
            except ValueError:
                pass
            raise CoreError(resp.status_code, code, message)
        return resp

    async def user_by_channel(self, channel: str, address: str) -> uuid.UUID | None:
        """Phone → user id; None when the number has no account (onboarding)."""
        try:
            resp = await self._request(
                "GET",
                "/users/by-channel",
                params={"channel": channel, "address": address},
            )
        except CoreError as exc:
            if exc.status_code == 404:
                return None
            raise
        return UserIdDto.model_validate(resp.json()).user_id

    async def market_by_ref(self, ref: str) -> MarketSummaryDto:
        resp = await self._request("GET", f"/markets/{ref}")
        return MarketSummaryDto.model_validate(resp.json())

    async def positions(self, user_id: uuid.UUID) -> list[PositionDto]:
        resp = await self._request("GET", f"/users/{user_id}/positions")
        return [PositionDto.model_validate(item) for item in resp.json()]

    async def preview_trade(
        self,
        *,
        user_id: uuid.UUID,
        market_ref: str,
        side: str,
        action: str,
        amount_micro: int,
    ) -> TradePreviewDto:
        body = PreviewTradeRequest(
            user_id=user_id,
            market_ref=market_ref,
            side=SideDto(side),
            action=TradeActionDto(action),
            amount_micro=amount_micro,
        )
        resp = await self._request(
            "POST", "/trades/preview", json_body=body.model_dump(mode="json")
        )
        return TradePreviewDto.model_validate(resp.json())

    async def place_trade(
        self,
        *,
        user_id: uuid.UUID,
        market_ref: str,
        side: str,
        action: str,
        amount_micro: int,
        idempotency_key: str,
        run_id: uuid.UUID | None = None,
        pending_action_id: uuid.UUID | None = None,
        expected_config_version: int,
        region: str | None = None,
    ) -> TradeReceiptDto:
        body = PlaceTradeRequest(
            user_id=user_id,
            market_ref=market_ref,
            side=SideDto(side),
            action=TradeActionDto(action),
            amount_micro=amount_micro,
            idempotency_key=idempotency_key,
            run_id=run_id,
            pending_action_id=pending_action_id,
            expected_config_version=expected_config_version,
        )
        resp = await self._request(
            "POST",
            "/trades",
            json_body=body.model_dump(mode="json"),
            region=region,
        )
        return TradeReceiptDto.model_validate(resp.json())

    async def cast_vote(
        self,
        *,
        user_id: uuid.UUID,
        market_ref: str,
        side: str,
        crowd_guess_pct: int,
        idempotency_key: str,
        run_id: uuid.UUID | None = None,
        region: str | None = None,
    ) -> VoteReceiptDto:
        body = CastVoteRequest(
            user_id=user_id,
            market_ref=market_ref,
            side=SideDto(side),
            crowd_guess_pct=crowd_guess_pct,
            idempotency_key=idempotency_key,
            run_id=run_id,
        )
        resp = await self._request(
            "POST",
            "/votes",
            json_body=body.model_dump(mode="json"),
            region=region,
        )
        return VoteReceiptDto.model_validate(resp.json())
