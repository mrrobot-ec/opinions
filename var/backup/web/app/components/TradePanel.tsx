"use client";

import { useState } from "react";
import {
  formatCentsPrice,
  formatDollars,
} from "@/lib/format";
import type { SideDto, TradeActionDto, TradePreviewDto } from "@/lib/types";

const CHIPS = [
  { label: "+$1", dollars: 1, max: false },
  { label: "+$10", dollars: 10, max: false },
  { label: "+$20", dollars: 20, max: false },
  { label: "Max", dollars: 100, max: true },
] as const;

export default function TradePanel({
  hasVoted,
  frozen,
  loggedIn,
  tradeSide,
  setTradeSide,
  action,
  setAction,
  dollars,
  setDollars,
  preview,
  previewBusy,
  tradeBusy,
  tradeError,
  tradeOk,
  onPreview,
  onConfirm,
}: {
  hasVoted: boolean;
  frozen: boolean;
  loggedIn: boolean;
  tradeSide: SideDto;
  setTradeSide: (s: SideDto) => void;
  action: TradeActionDto;
  setAction: (a: TradeActionDto) => void;
  dollars: number;
  setDollars: (n: number) => void;
  preview: TradePreviewDto | null;
  previewBusy: boolean;
  tradeBusy: boolean;
  tradeError: string | null;
  tradeOk: string | null;
  onPreview: () => void;
  onConfirm: () => void;
}) {
  const [explainerOpen, setExplainerOpen] = useState(false);
  const unlocked = hasVoted && !frozen && loggedIn;
  const lockReason = !loggedIn
    ? "Log in (dev session) to trade."
    : !hasVoted
      ? "Unlock trading by voting."
      : frozen
        ? "Trading frozen for the hidden-tally window."
        : null;

  /** +$ chips accumulate; Max sets a fixed demo cap (no balance API yet). */
  function applyChip(dollarsChip: number, isMax: boolean) {
    if (isMax) {
      setDollars(100);
      return;
    }
    setDollars(Math.min(10_000, dollars + dollarsChip));
  }

  return (
    <div className={`panel trade-panel ${unlocked ? "" : "locked-panel"}`}>
      <div className="panel-head-row">
        <h3>Trade</h3>
        <button
          type="button"
          className="text-btn"
          onClick={() => setExplainerOpen((v) => !v)}
        >
          What is trading?
        </button>
      </div>
      {explainerOpen && (
        <p className="panel-hint explainer">
          After you vote, you can buy or sell YES/NO shares. Amounts are in
          dollars of collateral. Preview shows server-quoted shares, fee, and
          average price — nothing is calculated on the client.
        </p>
      )}
      {lockReason && (
        <div className="locked-overlay">
          <p>{lockReason}</p>
        </div>
      )}
      <div className={unlocked ? "" : "locked-body"}>
        <div className="tabs" role="tablist" aria-label="Buy or sell">
          <button
            type="button"
            role="tab"
            aria-selected={action === "buy"}
            className={`tab ${action === "buy" ? "active" : ""}`}
            onClick={() => setAction("buy")}
          >
            Buy
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={action === "sell"}
            className={`tab ${action === "sell" ? "active" : ""}`}
            onClick={() => setAction("sell")}
          >
            Sell
          </button>
        </div>

        <div className="field">
          <label htmlFor="trade-amt">Amount (USD)</label>
          <input
            id="trade-amt"
            type="number"
            min={1}
            step={1}
            value={dollars}
            onChange={(e) => setDollars(Math.max(0, Number(e.target.value) || 0))}
          />
        </div>
        <div className="chips">
          {CHIPS.map((c) => (
            <button
              key={c.label}
              type="button"
              className={`btn btn-chip ${c.max && dollars === 100 ? "active" : ""}`}
              onClick={() => applyChip(c.dollars, c.max)}
            >
              {c.label}
            </button>
          ))}
        </div>

        <div className="side-toggle trade-side-btns">
          <button
            type="button"
            className={`btn btn-yes btn-block ${tradeSide === "yes" ? "active" : ""}`}
            onClick={() => setTradeSide("yes")}
          >
            {action === "buy" ? "Buy Yes" : "Sell Yes"}
          </button>
          <button
            type="button"
            className={`btn btn-no btn-block ${tradeSide === "no" ? "active" : ""}`}
            onClick={() => setTradeSide("no")}
          >
            {action === "buy" ? "Buy No" : "Sell No"}
          </button>
        </div>

        <button
          type="button"
          className="btn btn-block"
          disabled={previewBusy || !unlocked}
          onClick={onPreview}
        >
          {previewBusy ? "Previewing…" : "Preview ticket"}
        </button>
        {preview && (
          <>
            <dl className="preview-grid">
              <dt>Shares</dt>
              <dd className="num">{formatDollars(preview.shares_micro)}</dd>
              <dt>Avg price</dt>
              <dd className="num">{formatCentsPrice(preview.avg_price_micro)}</dd>
              <dt>Fee</dt>
              <dd className="num">{formatDollars(preview.fee_micro)}</dd>
              <dt>Gross</dt>
              <dd className="num">{formatDollars(preview.gross_micro)}</dd>
              <dt>Max payout</dt>
              <dd className="num">{formatDollars(preview.shares_micro)}</dd>
            </dl>
            <button
              type="button"
              className="btn btn-primary btn-block"
              disabled={tradeBusy}
              onClick={onConfirm}
            >
              {tradeBusy ? "Confirming…" : "Confirm trade"}
            </button>
          </>
        )}
        {tradeError && (
          <p role="alert" style={{ color: "var(--danger)", fontSize: "0.88rem" }}>
            {tradeError}
          </p>
        )}
        {tradeOk && (
          <p style={{ color: "var(--yes)", fontSize: "0.88rem" }}>{tradeOk}</p>
        )}
      </div>
    </div>
  );
}
