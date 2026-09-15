"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { getHolders } from "@/lib/api";
import { formatDollars } from "@/lib/format";
import { SOCIAL_COPY } from "@/lib/copy";
import {
  createCoalesce,
  HOLDERS_COALESCE_MS,
} from "@/lib/social";
import type { HoldersDto } from "@/lib/types";

/**
 * Top holders by committed capital.
 * Re-fetch is event-driven on trade frames (parent calls notifyTrade),
 * coalesced 2s — not a polling interval.
 */
export default function HoldersPanel({
  marketId,
  tradeTick,
}: {
  marketId: string;
  /** Increment / change when a trade frame arrives for this market. */
  tradeTick: number;
}) {
  const [data, setData] = useState<HoldersDto | null>(null);
  const [available, setAvailable] = useState(true);
  const [loading, setLoading] = useState(true);
  const coalesceRef = useRef<ReturnType<typeof createCoalesce> | null>(null);

  const fetchHolders = useCallback(async () => {
    try {
      const h = await getHolders(marketId, 10);
      if (h === null) {
        setAvailable(false);
        setData(null);
      } else {
        setAvailable(true);
        setData(h);
      }
    } catch {
      /* keep last good */
    } finally {
      setLoading(false);
    }
  }, [marketId]);

  useEffect(() => {
    setLoading(true);
    void fetchHolders();
  }, [fetchHolders]);

  useEffect(() => {
    coalesceRef.current?.cancel();
    coalesceRef.current = createCoalesce(HOLDERS_COALESCE_MS, () => {
      void fetchHolders();
    });
    return () => coalesceRef.current?.cancel();
  }, [fetchHolders]);

  useEffect(() => {
    if (tradeTick <= 0) return;
    coalesceRef.current?.trigger();
  }, [tradeTick]);

  if (!available && !loading) {
    return (
      <div className="panel holders-panel">
        <h3>Top holders</h3>
        <p className="panel-hint">Holders list not available yet.</p>
      </div>
    );
  }

  return (
    <div className="panel holders-panel">
      <div className="panel-head-row">
        <h3>Top holders</h3>
        <span
          className="panel-hint"
          style={{ margin: 0 }}
          title={SOCIAL_COPY.holders_tooltip}
        >
          {SOCIAL_COPY.holders_label}
        </span>
      </div>
      {loading && !data ? (
        <div className="skeleton" style={{ height: 100 }} />
      ) : (
        <div className="holders-grid">
          <HolderColumn side="YES" rows={data?.yes ?? []} />
          <HolderColumn side="NO" rows={data?.no ?? []} />
        </div>
      )}
    </div>
  );
}

function HolderColumn({
  side,
  rows,
}: {
  side: "YES" | "NO";
  rows: { handle: string; cost_micro: number; tier?: number }[];
}) {
  return (
    <div className={`holder-col ${side === "YES" ? "yes" : "no"}`}>
      <div className="holder-col-head">{side}</div>
      {rows.length === 0 ? (
        <p className="panel-hint">—</p>
      ) : (
        <ol className="holder-list">
          {rows.map((r, i) => (
            <li key={`${r.handle}-${i}`}>
              <span className="handle">{r.handle || "anon"}</span>
              {r.tier != null && (
                <span className="tier-badge">T{r.tier}</span>
              )}
              <span className="num">{formatDollars(r.cost_micro)}</span>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}
