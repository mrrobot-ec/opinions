"use client";

import { coreUrl } from "@/lib/api";
import {
  preferredMediaUrl,
  resolveAssetSrc,
  type MarketAssets,
} from "@/lib/content";
import { CURATION } from "@/lib/curationCopy";

/**
 * Poster / video display. Assets always via <img> (or video tag for video URL).
 * Never inject raw HTML; never inline SVG markup into the document.
 */
export default function MarketMedia({
  assets,
  question,
  tier,
  compact,
}: {
  assets: MarketAssets;
  question?: string;
  tier?: string | null;
  compact?: boolean;
}) {
  const preferred = preferredMediaUrl(assets);
  const src = resolveAssetSrc(preferred.url, coreUrl());
  const flash = tier === "flash";

  return (
    <div className={`market-media ${compact ? "compact" : ""}`}>
      <div className="market-media-frame">
        {preferred.kind === "video" && src ? (
          // Video URL may be an SVG poster or future media; use img for svg, video for others
          src.endsWith(".svg") || src.includes("image/svg") ? (
            // eslint-disable-next-line @next/next/no-img-element
            <img
              src={src}
              alt={question ?? CURATION.media_placeholder}
              className="market-media-img"
            />
          ) : (
            <video
              className="market-media-img"
              src={src}
              controls
              playsInline
              poster={
                resolveAssetSrc(assets.poster_asset_url, coreUrl()) ?? undefined
              }
            />
          )
        ) : preferred.kind === "poster" && src ? (
          // eslint-disable-next-line @next/next/no-img-element
          <img
            src={src}
            alt={question ?? CURATION.media_placeholder}
            className="market-media-img"
          />
        ) : (
          <div className="market-media-placeholder" role="img" aria-label={CURATION.media_placeholder}>
            <span className="brand-mark" aria-hidden />
            <span>{CURATION.media_placeholder}</span>
            {question && (
              <span className="market-media-q">{question}</span>
            )}
          </div>
        )}
      </div>
      {flash && (
        <span className="chip poster-first" title={CURATION.flash_poster_note}>
          {CURATION.poster_first_chip}
        </span>
      )}
    </div>
  );
}
