"use client";

import Link from "next/link";
import { useCallback, useEffect, useState } from "react";
import { useParams } from "next/navigation";
import { ApiClientError, userProfile } from "@/lib/api";
import { formatDollars, formatPnl, formatBps } from "@/lib/format";
import type { UserProfileDto } from "@/lib/types";

export default function ProfilePage() {
  const params = useParams<{ id: string }>();
  const id = decodeURIComponent(params.id);
  const [profile, setProfile] = useState<UserProfileDto | null>(null);
  const [missing, setMissing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    setMissing(false);
    try {
      const p = await userProfile(id);
      if (p === null) {
        setMissing(true);
        setProfile(null);
        return;
      }
      setProfile(p);
    } catch (e) {
      setError(
        e instanceof ApiClientError
          ? e.message
          : e instanceof Error
            ? e.message
            : "Failed to load profile",
      );
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  if (error) {
    return (
      <div className="state-box error" role="alert">
        <h2>Profile</h2>
        <p>{error}</p>
        <button type="button" className="btn" onClick={() => void load()}>
          Retry
        </button>
      </div>
    );
  }

  if (missing) {
    return (
      <div className="state-box">
        <h2>Profile</h2>
        <p className="panel-hint">
          Profile isn&apos;t available yet — core route may still be landing.
        </p>
        <p>
          <Link href="/">← Markets</Link>
        </p>
      </div>
    );
  }

  if (!profile) {
    return (
      <div className="stack">
        <div className="skeleton" style={{ height: 120 }} />
        <div className="skeleton" style={{ height: 200 }} />
      </div>
    );
  }

  const handle = profile.handle ?? profile.user_id.slice(0, 8);
  /** rep_micro is 0..1_000_000 — show as 0–100 score-ish for display. */
  const repDisplay = (profile.rep_micro / 10_000).toFixed(1);
  const pnl = profile.realized_pnl_micro ?? 0;
  const avg = profile.avg_score_bp;
  const scored = profile.markets_scored ?? 0;

  return (
    <div className="profile-page stack">
      <p className="crumb">
        <Link href="/">← Markets</Link>
      </p>
      <header className="panel">
        <h1 className="page-title">@{handle}</h1>
        <div className="row" style={{ gap: "1rem", marginTop: "0.75rem" }}>
          <div>
            <div className="lbl-muted">Reputation</div>
            <div className="num bigish">{repDisplay}</div>
          </div>
          <div>
            <div className="lbl-muted">Tier</div>
            <div className="num bigish">T{profile.tier}</div>
          </div>
          <div>
            <div className="lbl-muted">Realized PnL</div>
            <div
              className={`num bigish ${pnl >= 0 ? "yes-tone" : "no-tone"}`}
            >
              {formatPnl(pnl)}
            </div>
          </div>
        </div>
        <div className="row" style={{ marginTop: "1rem", gap: "1.5rem" }}>
          <div>
            <div className="lbl-muted">Avg score</div>
            <div className="num">
              {avg != null ? formatBps(avg, 0) : "—"}
            </div>
          </div>
          <div>
            <div className="lbl-muted">Markets scored</div>
            <div className="num">{scored}</div>
          </div>
        </div>
      </header>

      <section className="panel">
        <h3>Recent trades</h3>
        {!profile.recent_trades?.length ? (
          <p className="panel-hint">No public trades yet.</p>
        ) : (
          <ul className="tape">
            {profile.recent_trades.map((t, i) => (
              <li key={`${t.created_at}-${i}`}>
                <span className="handle">
                  {t.market_slug ? (
                    <Link href={`/m/${encodeURIComponent(t.market_slug)}`}>
                      {t.market_slug}
                    </Link>
                  ) : (
                    t.market_id.slice(0, 8)
                  )}
                </span>
                <span className={t.side === "yes" ? "side-yes" : "side-no"}>
                  {t.action} {t.side.toUpperCase()}
                </span>
                <span className="num">{formatDollars(t.collateral_micro)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="panel">
        <h3>Recent votes</h3>
        <p className="panel-hint">
          Side and score appear only after a market resolves. Cast times are
          public.
        </p>
        {!profile.recent_votes?.length ? (
          <p className="panel-hint">No votes yet.</p>
        ) : (
          <ul className="tape">
            {profile.recent_votes.map((v, i) => (
              <li key={`${v.cast_at}-${i}`}>
                <span className="handle">
                  <Link href={`/m/${encodeURIComponent(v.market_id)}`}>
                    {v.market_question ?? v.market_id.slice(0, 8)}
                  </Link>
                </span>
                <span className="num">
                  {new Date(v.cast_at).toLocaleString()}
                </span>
                {v.side != null ? (
                  <span className={v.side === "yes" ? "side-yes" : "side-no"}>
                    {v.side.toUpperCase()}
                    {v.score_bp != null ? ` · ${formatBps(v.score_bp, 0)}` : ""}
                  </span>
                ) : (
                  <span className="panel-hint" style={{ margin: 0 }}>
                    voted · side hidden until resolve
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
