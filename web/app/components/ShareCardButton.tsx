"use client";

import { useState } from "react";
import { getUserId, shareCardImgSrc } from "@/lib/api";
import { CURATION } from "@/lib/curationCopy";
import { isTerminalState } from "@/lib/marketUi";
import type { MarketStateDto } from "@/lib/types";

/**
 * Share card — plan: embed via <img src> ONLY (never inline SVG HTML).
 */
export default function ShareCardButton({
  marketId,
  state,
}: {
  marketId: string;
  state: MarketStateDto;
}) {
  const [open, setOpen] = useState(false);
  const [broken, setBroken] = useState(false);
  const userId = typeof window !== "undefined" ? getUserId() : null;
  const terminal = isTerminalState(state);

  if (!terminal) {
    return (
      <p className="panel-hint" style={{ margin: 0 }}>
        {CURATION.share_card_pre_resolve}
      </p>
    );
  }

  if (!userId) {
    return (
      <p className="panel-hint" style={{ margin: 0 }}>
        Dev login required to load your share card.
      </p>
    );
  }

  const src = shareCardImgSrc(userId, marketId);

  return (
    <div className="share-card-wrap">
      <button
        type="button"
        className="btn btn-chip"
        onClick={() => {
          setOpen((v) => !v);
          setBroken(false);
        }}
      >
        {CURATION.share_card}
      </button>
      <span className="panel-hint" style={{ margin: 0 }}>
        {CURATION.share_card_hint}
      </span>
      {open && (
        <div className="share-card-panel">
          {broken ? (
            <p className="panel-hint">{CURATION.share_card_unavailable}</p>
          ) : (
            // eslint-disable-next-line @next/next/no-img-element
            <img
              src={src}
              alt={CURATION.share_card}
              className="share-card-img"
              onError={() => setBroken(true)}
            />
          )}
        </div>
      )}
    </div>
  );
}
