"use client";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useParams } from "next/navigation";
import CommentSection from "@/app/components/CommentSection";
import HoldersPanel from "@/app/components/HoldersPanel";
import MarketMedia from "@/app/components/MarketMedia";
import ShareCardButton from "@/app/components/ShareCardButton";
import TradePanel from "@/app/components/TradePanel";
import VotePanel from "@/app/components/VotePanel";
import {
  applyAssetFrame,
  rehydrateAssetsFromSnapshot,
  type MarketAssets,
} from "@/lib/content";
import {
  ApiClientError,
  castVote,
  getChart,
  getMarket,
  getTape,
  getUserId,
  newIdempotencyKey,
  placeTrade,
  previewTrade,
  userPositions,
} from "@/lib/api";
import {
  dollarsToMicro,
  formatBps,
  formatCentsPrice,
  formatDollars,
  formatPnl,
  formatYesPct,
} from "@/lib/format";
import {
  formatCountdown,
  isTradingFrozen,
  msRemaining,
  parseTimeMs,
  type ServerClock,
} from "@/lib/countdown";
import { isTerminalState, isUnderReview, yesPctFromMicro } from "@/lib/marketUi";
import { defaultWsUrl, MarketSocket } from "@/lib/ws";
import type {
  MarketSummaryDto,
  PositionDto,
  PricePoint,
  ServerFrame,
  SideDto,
  TapeRow,
  TradeActionDto,
  TradePreviewDto,
  VoteReceiptDto,
} from "@/lib/types";

type WsStatus = "connecting" | "open" | "closed" | "error";

export default function MarketDetailPage() {
  const params = useParams<{ slug: string }>();
  const slug = decodeURIComponent(params.slug);

  const [market, setMarket] = useState<MarketSummaryDto | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [wsStatus, setWsStatus] = useState<WsStatus>("closed");
  const [clock, setClock] = useState<ServerClock | null>(null);
  const [nowMono, setNowMono] = useState(0);
  const [priceFlash, setPriceFlash] = useState<"up" | "down" | null>(null);

  const [voteSide, setVoteSide] = useState<SideDto>("yes");
  const [guess, setGuess] = useState(50);
  const [voteBusy, setVoteBusy] = useState(false);
  const [voteError, setVoteError] = useState<string | null>(null);
  const [voteReceipt, setVoteReceipt] = useState<VoteReceiptDto | null>(null);

  const [tradeSide, setTradeSide] = useState<SideDto>("yes");
  const [action, setAction] = useState<TradeActionDto>("buy");
  const [dollars, setDollars] = useState(10);
  const [preview, setPreview] = useState<TradePreviewDto | null>(null);
  const [previewBusy, setPreviewBusy] = useState(false);
  const [tradeBusy, setTradeBusy] = useState(false);
  const [tradeError, setTradeError] = useState<string | null>(null);
  const [tradeOk, setTradeOk] = useState<string | null>(null);
  const idemRef = useRef(newIdempotencyKey());

  const [tape, setTape] = useState<TapeRow[] | null>(null);
  const [tapeAvailable, setTapeAvailable] = useState(true);
  const [chart, setChart] = useState<PricePoint[] | null>(null);
  const [chartAvailable, setChartAvailable] = useState(true);
  const [myPositions, setMyPositions] = useState<PositionDto[]>([]);
  const [tally, setTally] = useState<{ yes: number; no: number } | null>(null);
  const [finalBps, setFinalBps] = useState<number | null>(null);
  const [redYes, setRedYes] = useState<number | null>(null);
  const [redNo, setRedNo] = useState<number | null>(null);
  const [userId, setUserId] = useState<string | null>(null);
  /** Bumps on trade frames → HoldersPanel coalesced re-fetch (no polling). */
  const [tradeTick, setTradeTick] = useState(0);
  const [assets, setAssets] = useState<MarketAssets>({
    poster_asset_url: null,
    video_asset_url: null,
  });

  const load = useCallback(async () => {
    setLoadError(null);
    try {
      const m = await getMarket(slug);
      setMarket(m);
      setAssets(
        rehydrateAssetsFromSnapshot({
          poster_asset_url: m.poster_asset_url ?? null,
          video_asset_url: m.video_asset_url ?? null,
        }),
      );
      if (m.server_now) {
        setClock({
          serverNowMs: parseTimeMs(m.server_now),
          monoAtReceipt: performance.now(),
        });
      }
      if (m.final_vote_bps != null) setFinalBps(m.final_vote_bps);
      if (m.redemption_yes_micro != null) setRedYes(m.redemption_yes_micro);
      if (m.redemption_no_micro != null) setRedNo(m.redemption_no_micro);

      const [c, t] = await Promise.all([getChart(m.id), getTape(m.id)]);
      if (c === null) setChartAvailable(false);
      else setChart(c);
      if (t === null) setTapeAvailable(false);
      else setTape(t);

      const uid = getUserId();
      setUserId(uid);
      if (uid) {
        try {
          const pos = await userPositions(uid);
          setMyPositions(pos.filter((p) => p.market_id === m.id));
        } catch {
          /* optional */
        }
      }
    } catch (e) {
      const msg =
        e instanceof ApiClientError
          ? `${e.message} (${e.status})`
          : e instanceof Error
            ? e.message
            : "Failed to load market";
      setLoadError(msg);
    }
  }, [slug]);

  useEffect(() => {
    void load();
    const sync = () => setUserId(getUserId());
    window.addEventListener("opinions-auth", sync);
    window.addEventListener("focus", sync);
    return () => {
      window.removeEventListener("opinions-auth", sync);
      window.removeEventListener("focus", sync);
    };
  }, [load]);

  useEffect(() => {
    if (!market?.id) return;
    const sock = new MarketSocket(
      defaultWsUrl(),
      (frame: ServerFrame) => {
        if (frame.type === "snapshot") {
          setClock({
            serverNowMs: parseTimeMs(frame.server_now),
            monoAtReceipt: performance.now(),
          });
          setMarket((prev) =>
            prev
              ? {
                  ...prev,
                  state: frame.state,
                  price_yes_micro: frame.price_yes_micro,
                  price_no_micro: frame.price_no_micro,
                  closes_at: frame.closes_at,
                  tally_hidden_at: frame.tally_hidden_at,
                  under_review:
                    frame.under_review ?? frame.state === "resolving",
                  poster_asset_url: frame.poster_asset_url,
                  video_asset_url: frame.video_asset_url,
                }
              : prev,
          );
          setAssets((prev) =>
            rehydrateAssetsFromSnapshot(
              {
                poster_asset_url: frame.poster_asset_url,
                video_asset_url: frame.video_asset_url,
              },
              prev,
            ),
          );
          setTally(frame.tally);
          if (frame.final_vote_bps != null) setFinalBps(frame.final_vote_bps);
          if (frame.redemption_yes_micro != null)
            setRedYes(frame.redemption_yes_micro);
          if (frame.redemption_no_micro != null)
            setRedNo(frame.redemption_no_micro);
          return;
        }
        if (frame.type === "asset") {
          setAssets((prev) =>
            applyAssetFrame(prev, { kind: frame.kind, url: frame.url }),
          );
          setMarket((prev) =>
            prev
              ? {
                  ...prev,
                  ...(frame.kind === "poster"
                    ? { poster_asset_url: frame.url }
                    : { video_asset_url: frame.url }),
                }
              : prev,
          );
          return;
        }
        if (frame.type === "price") {
          setMarket((prev) => {
            if (!prev) return prev;
            if (frame.price_yes_micro > prev.price_yes_micro) setPriceFlash("up");
            else if (frame.price_yes_micro < prev.price_yes_micro)
              setPriceFlash("down");
            return {
              ...prev,
              price_yes_micro: frame.price_yes_micro,
              price_no_micro: frame.price_no_micro,
            };
          });
          return;
        }
        if (frame.type === "trade") {
          const row: TapeRow = {
            handle: frame.handle,
            side: frame.side,
            action: frame.action,
            collateral_micro: frame.collateral_micro,
            created_at: frame.created_at,
            seq: frame.trade_seq,
          };
          setTape((prev) => {
            if (prev === null) return [row];
            if (prev.some((r) => r.seq === row.seq)) return prev;
            return [row, ...prev].slice(0, 100);
          });
          setTradeTick((n) => n + 1);
          return;
        }
        if (frame.type === "lifecycle") {
          setMarket((prev) =>
            prev
              ? {
                  ...prev,
                  state: frame.state,
                  under_review:
                    frame.state === "resolving"
                      ? true
                      : isTerminalState(frame.state)
                        ? false
                        : prev.under_review,
                }
              : prev,
          );
          if (frame.final_vote_bps != null) setFinalBps(frame.final_vote_bps);
          if (frame.redemption_yes_micro != null)
            setRedYes(frame.redemption_yes_micro);
          if (frame.redemption_no_micro != null)
            setRedNo(frame.redemption_no_micro);
          if (isTerminalState(frame.state)) {
            const uid = getUserId();
            if (uid && market) {
              void userPositions(uid).then((pos) =>
                setMyPositions(pos.filter((p) => p.market_id === market.id)),
              );
            }
          }
          return;
        }
        if (frame.type === "tally") {
          setTally(frame.tally);
        }
      },
      setWsStatus,
    );
    sock.subscribe(market.id);
    return () => sock.unsubscribe();
  }, [market?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!priceFlash) return;
    const t = window.setTimeout(() => setPriceFlash(null), 600);
    return () => clearTimeout(t);
  }, [priceFlash]);

  useEffect(() => {
    if (!clock) return;
    const id = window.setInterval(() => setNowMono(performance.now()), 250);
    return () => clearInterval(id);
  }, [clock]);

  const frozen = useMemo(() => {
    if (!market) return false;
    return isTradingFrozen(
      market.state,
      market.tally_hidden_at,
      clock,
      nowMono,
    );
  }, [market, clock, nowMono]);

  const closesLabel = useMemo(() => {
    if (!market?.closes_at || !clock) return null;
    const rem = msRemaining(market.closes_at, clock, nowMono);
    return rem > 0 ? formatCountdown(rem) : "0:00";
  }, [market, clock, nowMono]);

  const hasVoted = voteReceipt !== null;

  async function onVote() {
    const uid = getUserId();
    if (!uid || !market) {
      setVoteError("Dev login required — set user UUID + demo token.");
      return;
    }
    setVoteBusy(true);
    setVoteError(null);
    try {
      const receipt = await castVote({
        user_id: uid,
        market_ref: market.slug,
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
    if (!uid || !market) {
      setTradeError("Dev login required.");
      return;
    }
    setPreviewBusy(true);
    setTradeError(null);
    setTradeOk(null);
    idemRef.current = newIdempotencyKey();
    try {
      const p = await previewTrade({
        user_id: uid,
        market_ref: market.slug,
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
    if (!uid || !market || !preview) return;
    setTradeBusy(true);
    setTradeError(null);
    try {
      const receipt = await placeTrade({
        user_id: uid,
        market_ref: market.slug,
        side: tradeSide,
        action,
        amount_micro: dollarsToMicro(dollars),
        expected_config_version: preview.config_version,
        idempotency_key: idemRef.current,
      });
      setTradeOk(
        `Filled ${formatDollars(receipt.shares_micro)} shares @ ${formatCentsPrice(receipt.avg_price_micro)}`,
      );
      setPreview(null);
      idemRef.current = newIdempotencyKey();
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

  if (loadError) {
    return (
      <div className="state-box error" role="alert">
        <h2>Market not found</h2>
        <p>{loadError}</p>
        <p style={{ marginTop: 12 }}>
          <button type="button" className="btn" onClick={() => void load()}>
            Retry
          </button>
        </p>
      </div>
    );
  }

  if (!market) {
    return (
      <div className="hero-layout">
        <div className="hero-media skeleton" style={{ minHeight: 260 }} />
        <div className="hero-actions stack">
          <div className="skeleton" style={{ height: 140 }} />
          <div className="skeleton" style={{ height: 200 }} />
        </div>
      </div>
    );
  }

  const title = market.question ?? market.slug;
  const terminal = isTerminalState(market.state);
  const underReview = isUnderReview(market);
  const flashCls =
    priceFlash === "up"
      ? "price-flash price-up"
      : priceFlash === "down"
        ? "price-flash price-down"
        : "";
  const myPayout = myPositions.reduce((s, p) => s + p.realized_pnl_micro, 0);

  return (
    <div className="detail-page">
      <p className="crumb">
        <Link href="/">← Markets</Link>
      </p>

      {/* HERO: media + vote → trade (mobile stacks same order) */}
      <section className="hero-layout" aria-label="Market hero">
        <div className={`hero-media static ${flashCls}`}>
          <div className="hero-media-inner">
            <MarketMedia
              assets={assets}
              question={title}
              tier={market.content_tier}
              compact
            />
            <div className="hero-media-tags">
              <span
                className={
                  underReview
                    ? "badge under-review"
                    : frozen
                      ? "badge frozen"
                      : market.state === "live"
                        ? "badge live"
                        : market.state === "resolved" || market.state === "paid"
                          ? "badge resolved"
                          : market.state === "voided"
                            ? "badge voided"
                            : "badge"
                }
              >
                {underReview
                  ? "under review"
                  : frozen
                    ? "frozen"
                    : market.state}
              </span>
              <span className={`ws-status ${wsStatus}`}>
                {wsStatus === "open" && <span className="live-dot" />}
                WS {wsStatus}
              </span>
            </div>
            <h1 className="hero-question">{title}</h1>

            {!terminal && (
              <div className="header-prices hero-prices">
                <div className={`price-tile yes ${flashCls}`}>
                  <div className="label">YES</div>
                  <div className="value num">
                    {formatCentsPrice(market.price_yes_micro)}
                  </div>
                  <div className="num" style={{ fontSize: "0.8rem", opacity: 0.75 }}>
                    {formatYesPct(market.price_yes_micro)}
                  </div>
                </div>
                <div className={`price-tile no ${flashCls}`}>
                  <div className="label">NO</div>
                  <div className="value num">
                    {formatCentsPrice(market.price_no_micro)}
                  </div>
                  <div className="num" style={{ fontSize: "0.8rem", opacity: 0.75 }}>
                    {formatYesPct(market.price_no_micro)}
                  </div>
                </div>
              </div>
            )}

            <div className="hero-media-footer num">
              {closesLabel && !terminal && (
                <span className="countdown">
                  closes <strong>{closesLabel}</strong>
                </span>
              )}
              {tally && !frozen && !terminal && (
                <>
                  <span className="meta-dot">|</span>
                  <span>
                    tally YES {tally.yes} · NO {tally.no}
                  </span>
                </>
              )}
            </div>
            <div
              className="hero-media-glow"
              style={
                {
                  ["--yes-w" as string]: `${yesPctFromMicro(market.price_yes_micro)}%`,
                } as React.CSSProperties
              }
            />
          </div>
        </div>

        <div className="hero-actions">
          {frozen && !terminal && !underReview && (
            <div
              className="panel"
              style={{ borderColor: "rgba(240,180,41,0.35)", marginBottom: 0 }}
            >
              <p className="panel-hint" style={{ margin: 0, color: "var(--warn)" }}>
                Voting ends soon — trading paused. Tallies are hidden for the
                freeze window.
              </p>
            </div>
          )}

          {underReview && !terminal && (
            <div className="panel terminal-screen under-review-panel">
              <span className="badge under-review">under review</span>
              <p className="big void-tone" style={{ fontSize: "1.35rem" }}>
                Fairness review
              </p>
              <p className="panel-hint" style={{ maxWidth: "22rem", margin: "0 auto" }}>
                Large pot — automated fairness review before payout, usually a
                few minutes. Heuristic checks, not a guilt finding.
              </p>
            </div>
          )}

          {terminal && (
            <div className="panel terminal-screen">
              {market.state === "voided" ? (
                <>
                  <span className="badge voided">voided</span>
                  <p className="big void-tone">Voided</p>
                  <p className="panel-hint">
                    {market.void_reason ??
                      "Market voided — participation threshold not met. Neutral redemption."}
                  </p>
                </>
              ) : (
                <>
                  <span className="badge resolved">{market.state}</span>
                  <p
                    className={`big ${
                      (finalBps ?? 5000) >= 5000 ? "yes-tone" : "no-tone"
                    }`}
                  >
                    {finalBps != null ? formatBps(finalBps) : "—"}
                  </p>
                  <p className="panel-hint">Final vote share (YES)</p>
                </>
              )}
              <div className="redemption-row">
                <div className="tile">
                  <div className="lbl">YES redeems</div>
                  <div className="val num yes-tone">
                    {redYes != null ? formatCentsPrice(redYes) : "—"}
                  </div>
                </div>
                <div className="tile">
                  <div className="lbl">NO redeems</div>
                  <div className="val num no-tone">
                    {redNo != null ? formatCentsPrice(redNo) : "—"}
                  </div>
                </div>
              </div>
              {userId && (
                <div style={{ marginTop: "1.25rem" }}>
                  <div
                    className="lbl"
                    style={{
                      color: "var(--text-muted)",
                      fontSize: "0.72rem",
                      letterSpacing: "0.06em",
                      textTransform: "uppercase",
                      fontWeight: 700,
                    }}
                  >
                    Your payout
                  </div>
                  <div
                    className={`big num ${myPayout >= 0 ? "yes-tone" : "no-tone"}`}
                    style={{ fontSize: "1.6rem" }}
                  >
                    {formatPnl(myPayout)}
                  </div>
                  {myPositions.length === 0 && (
                    <p className="panel-hint">No positions on this market.</p>
                  )}
                  <div style={{ marginTop: "1rem" }}>
                    <ShareCardButton marketId={market.id} state={market.state} />
                  </div>
                </div>
              )}
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
        </div>
      </section>

      {chartAvailable && chart && chart.length > 0 && !terminal && (
        <div className="panel">
          <h3>Price chart</h3>
          <ChartPolyline points={chart} />
        </div>
      )}

      {tapeAvailable && (
        <div className="panel tape-panel">
          <h3>Public tape</h3>
          {!tape || tape.length === 0 ? (
            <p className="panel-hint">No trades yet — live appends appear here.</p>
          ) : (
            <ul className="tape">
              {tape.map((row) => (
                <li key={row.seq}>
                  <span className="handle">{row.handle || "anon"}</span>
                  <span className={row.side === "yes" ? "side-yes" : "side-no"}>
                    {row.action} {row.side.toUpperCase()}
                  </span>
                  <span className="num">{formatDollars(row.collateral_micro)}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <HoldersPanel marketId={market.id} tradeTick={tradeTick} />

      <CommentSection marketId={market.id} marketTerminal={terminal} />
    </div>
  );
}

function ChartPolyline({ points }: { points: PricePoint[] }) {
  if (points.length === 0) return null;
  const w = 320;
  const h = 120;
  const pad = 8;
  const values = points.map((p) => p.avg_price_micro);
  const min = Math.min(...values, 0);
  const max = Math.max(...values, 1_000_000);
  const span = Math.max(1, max - min);
  const coords = points.map((p, i) => {
    const x =
      pad +
      (points.length === 1
        ? (w - 2 * pad) / 2
        : (i / (points.length - 1)) * (w - 2 * pad));
    const y = h - pad - ((p.avg_price_micro - min) / span) * (h - 2 * pad);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });
  const poly = coords.join(" ");
  return (
    <div className="chart-wrap" aria-label="YES price chart">
      <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none">
        <line
          x1={pad}
          x2={w - pad}
          y1={h / 2}
          y2={h / 2}
          stroke="var(--border)"
          strokeDasharray="4 4"
        />
        <polyline
          fill="none"
          stroke="var(--yes)"
          strokeWidth="2.5"
          strokeLinejoin="round"
          strokeLinecap="round"
          points={poly}
        />
      </svg>
    </div>
  );
}
