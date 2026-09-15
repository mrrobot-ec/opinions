"use client";

import { useEffect, useState } from "react";
import { ApiClientError, getTopTraders, getTopVoters } from "@/lib/api";
import { formatPnl } from "@/lib/format";
import { formatAvgScoreBp } from "@/lib/repDisplay";
import type { TraderLeaderboardRow, VoterLeaderboardRow } from "@/lib/types";

const EMPTY_REASON = "Arrives with reputation";

function TierBadge({ tier }: { tier: number }) {
  return (
    <span className="tier-badge" title={`Reputation tier ${tier}`}>
      T{tier}
    </span>
  );
}

function VotersColumn({
  rows,
  empty,
  loading,
}: {
  rows: VoterLeaderboardRow[] | null;
  empty: string;
  loading: boolean;
}) {
  return (
    <div className="lb-col">
      <div className="lb-col-head">
        <h3>Top voters</h3>
        <p>Avg weekly score · accuracy, not volume</p>
      </div>
      {loading && (
        <div className="stack" style={{ gap: 8 }}>
          <div className="skeleton" style={{ height: 36 }} />
          <div className="skeleton" style={{ height: 36 }} />
        </div>
      )}
      {!loading && (!rows || rows.length === 0) && (
        <div className="lb-empty">
          <div className="lb-empty-rank">#</div>
          <div>
            <strong>{empty}</strong>
            <p>Rank · handle · avg score when the economy API is live.</p>
          </div>
        </div>
      )}
      {!loading && rows && rows.length > 0 && (
        <ol className="lb-list">
          {rows.map((r, i) => (
            <li key={`${r.handle}-${i}`}>
              <span className="lb-rank num">#{i + 1}</span>
              <span className="lb-handle">
                {r.handle || "anon"}
                <TierBadge tier={r.tier} />
              </span>
              <span className="lb-stat num">
                {formatAvgScoreBp(r.avg_score_bp)}
                <span className="lb-stat-sub">{r.markets_scored} mkts</span>
              </span>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

function TradersColumn({
  rows,
  empty,
  loading,
}: {
  rows: TraderLeaderboardRow[] | null;
  empty: string;
  loading: boolean;
}) {
  return (
    <div className="lb-col">
      <div className="lb-col-head">
        <h3>Top traders</h3>
        <p>Weekly settled PnL — not mark-to-market</p>
      </div>
      {loading && (
        <div className="stack" style={{ gap: 8 }}>
          <div className="skeleton" style={{ height: 36 }} />
          <div className="skeleton" style={{ height: 36 }} />
        </div>
      )}
      {!loading && (!rows || rows.length === 0) && (
        <div className="lb-empty">
          <div className="lb-empty-rank">#</div>
          <div>
            <strong>{empty}</strong>
            <p>Settled PnL ranks fill when realizations land.</p>
          </div>
        </div>
      )}
      {!loading && rows && rows.length > 0 && (
        <ol className="lb-list">
          {rows.map((r, i) => (
            <li key={`${r.handle}-${i}`}>
              <span className="lb-rank num">#{i + 1}</span>
              <span className="lb-handle">{r.handle || "anon"}</span>
              <span
                className={`lb-stat num ${
                  r.realized_pnl_micro >= 0 ? "pnl-pos" : "pnl-neg"
                }`}
              >
                {formatPnl(r.realized_pnl_micro)}
                <span className="lb-stat-sub">settled PnL</span>
              </span>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

/**
 * Weekly leaderboards — one REST fetch on mount (no intervals / no polling).
 * 404 or empty → designed empty state.
 */
export default function LeaderboardSlot() {
  const [voters, setVoters] = useState<VoterLeaderboardRow[] | null>(null);
  const [traders, setTraders] = useState<TraderLeaderboardRow[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [emptyReason, setEmptyReason] = useState(EMPTY_REASON);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      setLoading(true);
      try {
        const [v, t] = await Promise.all([getTopVoters(7, 10), getTopTraders(7, 10)]);
        if (cancelled) return;
        if (v === null && t === null) {
          setEmptyReason(EMPTY_REASON);
          setVoters([]);
          setTraders([]);
        } else {
          setVoters(v ?? []);
          setTraders(t ?? []);
          if ((v?.length ?? 0) === 0 && (t?.length ?? 0) === 0) {
            setEmptyReason("No scores this week yet");
          }
        }
      } catch (e) {
        if (cancelled) return;
        setVoters([]);
        setTraders([]);
        setEmptyReason(
          e instanceof ApiClientError
            ? `Leaderboards unavailable (${e.status})`
            : EMPTY_REASON,
        );
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <section className="lb-section" aria-labelledby="lb-heading">
      <div className="lb-header">
        <h2 id="lb-heading" className="section-title">
          Weekly leaderboard
        </h2>
        <p className="section-sub">
          Two economies: prediction accuracy and settled trading skill.
        </p>
      </div>
      <div className="lb-grid">
        <VotersColumn rows={voters} empty={emptyReason} loading={loading} />
        <TradersColumn rows={traders} empty={emptyReason} loading={loading} />
      </div>
    </section>
  );
}
