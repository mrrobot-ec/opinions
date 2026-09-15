"use client";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useState } from "react";
import LeaderboardSlot from "@/app/components/LeaderboardSlot";
import LiveUpcomingRail from "@/app/components/LiveUpcomingRail";
import MarketBrowseCard from "@/app/components/MarketBrowseCard";
import TradePanel from "@/app/components/TradePanel";
import VotePanel from "@/app/components/VotePanel";
import {
  ApiClientError,
  castVote,
  getUserId,
  getViewerVoteStatus,
  listDrafts,
  listMarkets,
  newIdempotencyKey,
  placeTrade,
  previewTrade,
} from "@/lib/api";
import type { ScheduledDraftSlot } from "@/lib/content";
import {
  formatCentsPrice,
  formatDollars,
  formatYesPct,
  dollarsToMicro,
} from "@/lib/format";
import {
  formatCountdown,
  isTradingFrozen,
  msRemaining,
  parseTimeMs,
  type ServerClock,
} from "@/lib/countdown";
import {
  isTerminalState,
  pickHeroMarket,
  railMarkets,
  yesPctFromMicro,
} from "@/lib/marketUi";
import type {
  MarketSummaryDto,
  SideDto,
  TradeActionDto,
  TradePreviewDto,
  VoteReceiptDto,
} from "@/lib/types";

export default function HomePage() {
  const [markets, setMarkets] = useState<MarketSummaryDto[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [clock, setClock] = useState<ServerClock | null>(null);
  const [nowMono, setNowMono] = useState(0);
  const [heroId, setHeroId] = useState<string | null>(null);

  // Hero interactive state (same gate patterns as detail)
  const [voteSide, setVoteSide] = useState<SideDto>("yes");
  const [guess, setGuess] = useState(50);
  const [voteBusy, setVoteBusy] = useState(false);
  const [voteError, setVoteError] = useState<string | null>(null);
  const [voteReceipt, setVoteReceipt] = useState<VoteReceiptDto | null>(null);
  const [viewerHasVoted, setViewerHasVoted] = useState(false);

  const [tradeSide, setTradeSide] = useState<SideDto>("yes");
  const [action, setAction] = useState<TradeActionDto>("buy");
  const [dollars, setDollars] = useState(10);
  const [preview, setPreview] = useState<TradePreviewDto | null>(null);
  const [previewBusy, setPreviewBusy] = useState(false);
  const [tradeBusy, setTradeBusy] = useState(false);
  const [tradeError, setTradeError] = useState<string | null>(null);
  const [tradeOk, setTradeOk] = useState<string | null>(null);
  const [idem, setIdem] = useState(() => newIdempotencyKey());
  const [userId, setUserId] = useState<string | null>(null);
  const [scheduledDrafts, setScheduledDrafts] = useState<ScheduledDraftSlot[]>(
    [],
  );

  const load = useCallback(async () => {
    setError(null);
    try {
      const rows = await listMarkets();
      setMarkets(rows);
      // Approved drafts for Coming Soon — 404 degrade → empty (honest empty state)
      try {
        const drafts = await listDrafts("approved");
        if (drafts) {
          setScheduledDrafts(
            drafts
              .filter((d) => d.publish_at)
              .map((d) => ({
                id: d.id,
                question: d.question,
                tier: d.tier,
                publish_at: d.publish_at!,
                status: d.status,
              })),
          );
        } else {
          setScheduledDrafts([]);
        }
      } catch {
        setScheduledDrafts([]);
      }
      const withNow = rows.find((r) => r.server_now);
      if (withNow?.server_now) {
        setClock({
          serverNowMs: parseTimeMs(withNow.server_now),
          monoAtReceipt: performance.now(),
        });
      } else {
        setClock({
          serverNowMs: Date.now(),
          monoAtReceipt: performance.now(),
        });
      }
      setNowMono(performance.now());
      const hero = pickHeroMarket(rows);
      setHeroId((prev) => prev ?? hero?.id ?? null);
    } catch (e) {
      const msg =
        e instanceof ApiClientError
          ? `${e.message} (${e.status})`
          : e instanceof Error
            ? e.message
            : "Failed to load markets";
      setError(msg);
      setMarkets([]);
    }
  }, []);

  useEffect(() => {
    void load();
    const sync = () => setUserId(getUserId());
    sync();
    window.addEventListener("opinions-auth", sync);
    window.addEventListener("focus", sync);
    return () => {
      window.removeEventListener("opinions-auth", sync);
      window.removeEventListener("focus", sync);
    };
  }, [load]);

  useEffect(() => {
    if (!clock) return;
    const id = window.setInterval(() => setNowMono(performance.now()), 250);
    return () => clearInterval(id);
  }, [clock]);

  const hero = useMemo(() => {
    if (!markets || markets.length === 0) return null;
    return markets.find((m) => m.id === heroId) ?? pickHeroMarket(markets);
  }, [markets, heroId]);

  const frozen = useMemo(() => {
    if (!hero) return false;
    return isTradingFrozen(hero.state, hero.tally_hidden_at, clock, nowMono);
  }, [hero, clock, nowMono]);

  const hasVoted = voteReceipt !== null || viewerHasVoted;
  const terminal = hero ? isTerminalState(hero.state) : false;

  async function onVote() {
    const uid = getUserId();
    if (!uid || !hero) {
      setVoteError("Dev login required — set user UUID + demo token.");
      return;
    }
    setVoteBusy(true);
    setVoteError(null);
    try {
      const receipt = await castVote({
        user_id: uid,
        market_ref: hero.slug,
        side: voteSide,
        crowd_guess_pct: guess,
        idempotency_key: newIdempotencyKey(),
      });
      setVoteReceipt(receipt);
    } catch (e) {
      setVoteError(
        e instanceof ApiClientError
          ? `${e.message} (${e.code})`
          : e instanceof Error
            ? e.message
            : "Vote failed",
      );
    } finally {
      setVoteBusy(false);
    }
  }

  async function onPreview() {
    const uid = getUserId();
    if (!uid || !hero) {
      setTradeError("Dev login required.");
      return;
    }
    setPreviewBusy(true);
    setTradeError(null);
    setTradeOk(null);
    const key = newIdempotencyKey();
    setIdem(key);
    try {
      const p = await previewTrade({
        user_id: uid,
        market_ref: hero.slug,
        side: tradeSide,
        action,
        amount_micro: dollarsToMicro(dollars),
      });
      setPreview(p);
    } catch (e) {
      setPreview(null);
      setTradeError(
        e instanceof ApiClientError
          ? `${e.message} (${e.status})`
          : e instanceof Error
            ? e.message
            : "Preview failed",
      );
    } finally {
      setPreviewBusy(false);
    }
  }

  async function onConfirmTrade() {
    const uid = getUserId();
    if (!uid || !hero || !preview) return;
    setTradeBusy(true);
    setTradeError(null);
    try {
      const receipt = await placeTrade({
        user_id: uid,
        market_ref: hero.slug,
        side: tradeSide,
        action,
        amount_micro: dollarsToMicro(dollars),
        expected_config_version: preview.config_version,
        idempotency_key: idem,
      });
      setTradeOk(
        `Filled ${formatDollars(receipt.shares_micro)} shares @ ${formatCentsPrice(receipt.avg_price_micro)}`,
      );
      setPreview(null);
      setIdem(newIdempotencyKey());
    } catch (e) {
      setTradeError(
        e instanceof ApiClientError
          ? `${e.message} (${e.status})`
          : e instanceof Error
            ? e.message
            : "Trade failed",
      );
    } finally {
      setTradeBusy(false);
    }
  }

  // Reset client receipts and restore the selected viewer's server vote state.
  useEffect(() => {
    let cancelled = false;
    setVoteReceipt(null);
    setViewerHasVoted(false);
    setVoteError(null);
    setPreview(null);
    setTradeError(null);
    setTradeOk(null);
    const marketId = hero?.id;
    if (marketId && userId) {
      void getViewerVoteStatus(marketId)
        .then((status) => {
          if (!cancelled) setViewerHasVoted(status.has_voted);
        })
        .catch(() => {
          if (!cancelled) {
            setVoteError("Could not restore your vote status. Reload to retry.");
          }
        });
    }
    return () => {
      cancelled = true;
    };
  }, [hero?.id, userId]);

  let closesLabel: string | null = null;
  if (hero?.closes_at && clock) {
    const rem = msRemaining(hero.closes_at, clock, nowMono);
    closesLabel = rem > 0 ? formatCountdown(rem) : "0:00";
  }

  const rail = markets ? railMarkets(markets) : [];
  const browse = markets ?? [];

  return (
    <div className="home-page">
      {markets === null && (
        <div className="hero-layout">
          <div className="hero-media skeleton" style={{ minHeight: 280 }} />
          <div className="hero-actions stack">
            <div className="skeleton" style={{ height: 140 }} />
            <div className="skeleton" style={{ height: 200 }} />
          </div>
        </div>
      )}

      {error && (
        <div className="state-box error" role="alert">
          <h2>Couldn’t load markets</h2>
          <p>{error}</p>
          <p style={{ marginTop: 12 }}>
            <button type="button" className="btn" onClick={() => void load()}>
              Retry
            </button>
          </p>
          <p style={{ marginTop: 10, fontSize: "0.8rem" }}>
            Is core running at{" "}
            <code className="num">
              {process.env.NEXT_PUBLIC_CORE_URL ?? "http://127.0.0.1:8080"}
            </code>
            ?
          </p>
        </div>
      )}

      {markets && markets.length === 0 && !error && (
        <div className="state-box">
          <h2>No markets yet</h2>
          <p>When the core seeds a market, the hero and rails fill in here.</p>
        </div>
      )}

      {hero && !error && (
        <>
          {/* HERO — media + vote first + trade */}
          <section className="hero-layout" aria-label="Featured market">
            <Link
              href={`/m/${encodeURIComponent(hero.slug)}`}
              className="hero-media"
            >
              <div className="hero-media-inner">
                <span
                  className={
                    frozen
                      ? "badge frozen"
                      : hero.state === "live"
                        ? "badge live"
                        : "badge"
                  }
                >
                  {frozen ? "frozen" : hero.state}
                </span>
                <h1 className="hero-question">{hero.question ?? hero.slug}</h1>
                <p className="hero-open-hint">Open market →</p>
                <div className="hero-media-footer num">
                  {!terminal && (
                    <>
                      <span>
                        YES {formatCentsPrice(hero.price_yes_micro)} (
                        {formatYesPct(hero.price_yes_micro)})
                      </span>
                      <span className="meta-dot">|</span>
                      <span>
                        NO {formatCentsPrice(hero.price_no_micro)}
                      </span>
                      {closesLabel && (
                        <>
                          <span className="meta-dot">|</span>
                          <span className="countdown">closes {closesLabel}</span>
                        </>
                      )}
                    </>
                  )}
                </div>
                <div
                  className="hero-media-glow"
                  style={
                    {
                      ["--yes-w" as string]: `${yesPctFromMicro(hero.price_yes_micro)}%`,
                    } as React.CSSProperties
                  }
                />
              </div>
            </Link>

            <div className="hero-actions">
              {frozen && !terminal && (
                <div
                  className="panel"
                  style={{ borderColor: "rgba(240,180,41,0.35)", marginBottom: 0 }}
                >
                  <p
                    className="panel-hint"
                    style={{ margin: 0, color: "var(--warn)" }}
                  >
                    Voting ends soon — trading paused. Tallies hidden for the
                    freeze window.
                  </p>
                </div>
              )}
              {!terminal && (
                <>
                  <VotePanel
                    hasVoted={hasVoted}
                    voteReceipt={voteReceipt}
                    voteSide={voteSide}
                    setVoteSide={setVoteSide}
                    guess={guess}
                    setGuess={setGuess}
                    voteBusy={voteBusy}
                    voteError={voteError}
                    onVote={() => void onVote()}
                    compact
                  />
                  <TradePanel
                    hasVoted={hasVoted}
                    frozen={frozen}
                    loggedIn={!!userId}
                    tradeSide={tradeSide}
                    setTradeSide={(s) => {
                      setTradeSide(s);
                      setPreview(null);
                    }}
                    action={action}
                    setAction={(a) => {
                      setAction(a);
                      setPreview(null);
                    }}
                    dollars={dollars}
                    setDollars={(n) => {
                      setDollars(n);
                      setPreview(null);
                    }}
                    preview={preview}
                    previewBusy={previewBusy}
                    tradeBusy={tradeBusy}
                    tradeError={tradeError}
                    tradeOk={tradeOk}
                    onPreview={() => void onPreview()}
                    onConfirm={() => void onConfirmTrade()}
                  />
                </>
              )}
              {terminal && (
                <div className="panel">
                  <h3>Resolved market</h3>
                  <p className="panel-hint">
                    Open the market for full payout and redemption detail.
                  </p>
                  <Link
                    href={`/m/${encodeURIComponent(hero.slug)}`}
                    className="btn btn-primary btn-block"
                  >
                    View result
                  </Link>
                </div>
              )}
            </div>
          </section>

          <LiveUpcomingRail
            markets={rail}
            scheduledDrafts={scheduledDrafts}
            clock={clock}
            nowMono={nowMono}
          />

          <LeaderboardSlot />

          <section className="browse-section" aria-labelledby="browse-heading">
            <div className="lb-header">
              <h2 id="browse-heading" className="section-title">
                Browse markets
              </h2>
              <p className="section-sub">
                Headline · split · pool / votes / closes-in
              </p>
            </div>
            <div className="browse-grid">
              {browse.map((m) => (
                <MarketBrowseCard
                  key={m.id}
                  market={m}
                  clock={clock}
                  nowMono={nowMono}
                />
              ))}
            </div>
          </section>
        </>
      )}

      <p className="footer-note">
        Live prices on market pages stream over WebSocket — no polling.
      </p>
    </div>
  );
}
