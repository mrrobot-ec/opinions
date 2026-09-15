"use client";

import Link from "next/link";
import {
  formatCountdown,
  type ServerClock,
} from "@/lib/countdown";
import {
  buildUpcomingRail,
  msUntilPublish,
  type ScheduledDraftSlot,
} from "@/lib/content";
import { CURATION } from "@/lib/curationCopy";
import type { MarketSummaryDto } from "@/lib/types";

export default function LiveUpcomingRail({
  markets,
  scheduledDrafts = [],
  clock,
  nowMono,
}: {
  markets: MarketSummaryDto[];
  /** Approved drafts with publish_at — real Coming Soon, not invented. */
  scheduledDrafts?: ScheduledDraftSlot[];
  clock: ServerClock | null;
  nowMono: number;
}) {
  const upcoming = buildUpcomingRail(scheduledDrafts, markets.length, 1);

  return (
    <section className="rail-section" aria-labelledby="rail-heading">
      <h2 id="rail-heading" className="section-title">
        Live &amp; Upcoming
      </h2>
      <div className="rail-scroll">
        {markets.map((m) => {
          let countdown = "—";
          if (m.closes_at && clock) {
            const rem =
              Date.parse(m.closes_at) -
              (clock.serverNowMs + (nowMono - clock.monoAtReceipt));
            countdown = rem > 0 ? formatCountdown(rem) : "0:00";
          } else if (m.state === "scheduled") {
            countdown = "soon";
          }
          return (
            <Link
              key={m.id}
              href={`/m/${encodeURIComponent(m.slug)}`}
              className={`rail-card rail-live state-${m.state}`}
            >
              <div className="rail-card-tags">
                <span className="rail-tag">{m.state}</span>
                {m.content_tier === "flash" && (
                  <span className="rail-tag muted">{CURATION.poster_first_chip}</span>
                )}
              </div>
              <div className="rail-countdown num">{countdown}</div>
              <div className="rail-card-title">{m.question ?? m.slug}</div>
            </Link>
          );
        })}
        {upcoming.map((item) => {
          if (item.kind === "empty") {
            return (
              <div
                key={item.id}
                className="rail-card rail-soon"
                aria-label={CURATION.empty_coming_soon}
              >
                <div className="rail-card-tags">
                  <span className="rail-tag muted">{CURATION.rail_scheduled}</span>
                </div>
                <div className="rail-countdown num dim">—:—</div>
                <div className="rail-card-title muted">
                  {CURATION.empty_coming_soon}
                </div>
                <div className="rail-soon-label">
                  {CURATION.empty_coming_soon_hint}
                </div>
              </div>
            );
          }
          const d = item.draft;
          let countdown = "—";
          if (clock) {
            const rem = msUntilPublish(
              d.publish_at,
              clock.serverNowMs,
              clock.monoAtReceipt,
              nowMono,
            );
            countdown =
              rem > 0
                ? `${CURATION.rail_opens_in} ${formatCountdown(rem)}`
                : "soon";
          }
          return (
            <div
              key={d.id}
              className="rail-card rail-soon rail-scheduled-draft"
              aria-label={d.question}
            >
              <div className="rail-card-tags">
                <span className="rail-tag">{CURATION.rail_scheduled}</span>
                {d.tier === "flash" && (
                  <span className="rail-tag muted">{CURATION.poster_first_chip}</span>
                )}
              </div>
              <div className="rail-countdown num">{countdown}</div>
              <div className="rail-card-title">{d.question}</div>
            </div>
          );
        })}
      </div>
    </section>
  );
}
