"use client";

import Link from "next/link";
import {
  formatCountdown,
  isTradingFrozen,
  msRemaining,
  type ServerClock,
} from "@/lib/countdown";
import { formatPoolCompact } from "@/lib/format";
import { noPctFromYes, yesPctFromMicro } from "@/lib/marketUi";
import type { MarketSummaryDto } from "@/lib/types";

function stateBadge(m: MarketSummaryDto, frozen: boolean): { label: string; cls: string } {
  if (frozen && (m.state === "live" || m.state === "closing")) {
    return { label: "Frozen", cls: "badge frozen" };
  }
  const map: Record<string, string> = {
    live: "badge live",
    closing: "badge closing",
    closed: "badge",
    resolving: "badge",
    resolved: "badge resolved",
    paid: "badge paid",
    voided: "badge voided",
    scheduled: "badge",
    draft: "badge",
  };
  return { label: m.state, cls: map[m.state] ?? "badge" };
}

export default function MarketBrowseCard({
  market,
  clock,
  nowMono,
}: {
  market: MarketSummaryDto;
  clock: ServerClock | null;
  nowMono: number;
}) {
  const frozen = isTradingFrozen(
    market.state,
    market.tally_hidden_at,
    clock,
    nowMono,
  );
  const badge = stateBadge(market, frozen);
  const title = market.question ?? market.slug;
  const yesPct = yesPctFromMicro(market.price_yes_micro);
  const noPct = noPctFromYes(yesPct);

  let closesLabel = "—";
  if (market.closes_at && clock) {
    const rem = msRemaining(market.closes_at, clock, nowMono);
    closesLabel = rem > 0 ? formatCountdown(rem) : "closed";
  }

  const poolLabel = formatPoolCompact(market.pool_micro ?? null);
  const votesLabel =
    market.votes != null && Number.isFinite(market.votes)
      ? String(market.votes)
      : "—";

  return (
    <Link href={`/m/${encodeURIComponent(market.slug)}`} className="browse-card-link">
      <article className="browse-card">
        <div className="browse-card-top">
          <p className="browse-q">{title}</p>
          <span className={badge.cls}>{badge.label}</span>
        </div>

        <div className="split-bar" aria-label={`YES ${yesPct}% · NO ${noPct}%`}>
          <div className="split-labels">
            <span className="yes-tone num">{yesPct}% Yes</span>
            <span className="no-tone num">No {noPct}%</span>
          </div>
          <div className="split-track">
            <div className="split-yes" style={{ width: `${yesPct}%` }} />
            <div className="split-no" style={{ width: `${noPct}%` }} />
          </div>
        </div>

        <div className="browse-meta num">
          <span title="Pool">Pool {poolLabel}</span>
          <span className="meta-dot" aria-hidden>
            |
          </span>
          <span title="Votes">Votes {votesLabel}</span>
          <span className="meta-dot" aria-hidden>
            |
          </span>
          <span title="Closes in">
            {frozen ? (
              <span className="pill warn">frozen</span>
            ) : (
              <>closes {closesLabel}</>
            )}
          </span>
        </div>
      </article>
    </Link>
  );
}
