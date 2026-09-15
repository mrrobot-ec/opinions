"use client";

import Link from "next/link";
import { useCallback, useEffect, useState } from "react";
import CreditBalance from "@/app/components/CreditBalance";
import { ApiClientError, getUserId, userPositions, userProfile } from "@/lib/api";
import { formatDollars, formatPnl } from "@/lib/format";
import { getBalances, type MoneyBalances } from "@/lib/money";
import { formatTierFeeLine } from "@/lib/repDisplay";
import type { PositionDto } from "@/lib/types";

export default function PortfolioPage() {
  const [positions, setPositions] = useState<PositionDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [userId, setUserId] = useState<string | null>(null);
  const [tierLine, setTierLine] = useState<string | null>(null);
  const [balances, setBalances] = useState<MoneyBalances | null>(null);

  const load = useCallback(async () => {
    const uid = getUserId();
    setUserId(uid);
    if (!uid) {
      setPositions([]);
      setError(null);
      setTierLine(null);
      return;
    }
    setError(null);
    try {
      const [rows, profile, money] = await Promise.all([
        userPositions(uid),
        userProfile(uid).catch(() => null),
        getBalances(uid).catch(() => null),
      ]);
      setPositions(rows);
      setBalances(money);
      if (profile && typeof profile.tier === "number") {
        setTierLine(formatTierFeeLine(profile.tier));
      } else {
        const withTier = rows.find((p) => typeof p.tier === "number");
        setTierLine(
          formatTierFeeLine(
            withTier && typeof withTier.tier === "number" ? withTier.tier : 0,
          ),
        );
      }
    } catch (e) {
      setPositions([]);
      setError(
        e instanceof ApiClientError
          ? `${e.message} (${e.status})`
          : e instanceof Error
            ? e.message
            : "Failed to load positions",
      );
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const totalPnl =
    positions?.reduce((s, p) => s + p.realized_pnl_micro, 0) ?? 0;
  const totalCost =
    positions?.reduce((s, p) => s + p.cost_micro, 0) ?? 0;

  return (
    <>
      <h1 className="page-title">Portfolio</h1>
      <p className="page-sub">
        Positions with cost basis and realized PnL.{" "}
        <span className="pill warn" style={{ verticalAlign: "middle" }}>
          DEV login
        </span>
        {tierLine && (
          <span className="profile-chip" style={{ marginLeft: 8 }}>
            {tierLine}
          </span>
        )}
      </p>

      {!userId && (
        <div className="state-box">
          <h2>Dev login required</h2>
          <p>
            Open the Dev login control in the nav, paste your user UUID and demo
            bearer token, then return here.
          </p>
        </div>
      )}

      {userId && positions === null && (
        <div className="stack">
          <div className="skeleton" style={{ height: 24, width: "50%" }} />
          <div className="skeleton" style={{ height: 120, width: "100%" }} />
        </div>
      )}

      {error && (
        <div className="state-box error" role="alert">
          <h2>Couldn’t load positions</h2>
          <p>{error}</p>
          <p style={{ marginTop: 12 }}>
            <button type="button" className="btn" onClick={() => void load()}>
              Retry
            </button>
          </p>
        </div>
      )}

      {userId && positions && !error && (
        <>
          <div className="header-prices" style={{ marginBottom: "1rem" }}>
            <CreditBalance balances={balances} />
            <div className="price-tile">
              <div className="label">Cost basis</div>
              <div className="value num" style={{ fontSize: "1.35rem" }}>
                {formatDollars(totalCost)}
              </div>
            </div>
            <div className="price-tile">
              <div className="label">Realized PnL</div>
              <div
                className={`value num ${totalPnl >= 0 ? "yes-tone" : "no-tone"}`}
                style={{
                  fontSize: "1.35rem",
                  color: totalPnl >= 0 ? "var(--yes)" : "var(--no)",
                }}
              >
                {formatPnl(totalPnl)}
              </div>
            </div>
          </div>

          {positions.length === 0 ? (
            <div className="state-box">
              <h2>No positions</h2>
              <p>
                After you trade on a market, positions show here.{" "}
                <Link href="/">Browse markets</Link>
              </p>
            </div>
          ) : (
            <div className="panel" style={{ overflowX: "auto" }}>
              <table className="table">
                <thead>
                  <tr>
                    <th>Market</th>
                    <th>Side</th>
                    <th>Shares</th>
                    <th>Cost</th>
                    <th>Realized</th>
                  </tr>
                </thead>
                <tbody>
                  {positions.map((p) => (
                    <tr key={`${p.market_id}-${p.outcome_id}`}>
                      <td>
                        <Link href={`/m/${p.market_id}`}>
                          <span className="num" style={{ fontSize: "0.78rem" }}>
                            {p.market_id.slice(0, 8)}…
                          </span>
                        </Link>
                      </td>
                      <td>
                        <span className={p.side === "yes" ? "pill yes" : "pill no"}>
                          {p.side.toUpperCase()}
                        </span>
                      </td>
                      <td className="num">{formatDollars(p.shares_micro)}</td>
                      <td className="num">{formatDollars(p.cost_micro)}</td>
                      <td
                        className={`num ${
                          p.realized_pnl_micro >= 0 ? "pnl-pos" : "pnl-neg"
                        }`}
                      >
                        {formatPnl(p.realized_pnl_micro)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </>
  );
}
