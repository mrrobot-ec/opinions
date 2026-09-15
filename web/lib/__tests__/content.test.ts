import { describe, expect, it } from "vitest";
import {
  applyAssetFrame,
  buildUpcomingRail,
  findSlotConflicts,
  formatFillRate,
  msUntilPublish,
  preferredMediaUrl,
  rehydrateAssetsFromSnapshot,
  resolveAssetSrc,
  shareCardPath,
  slotFillRate,
} from "../content";

describe("asset-frame hot-swap", () => {
  it("applies poster and video kinds", () => {
    let a = { poster_asset_url: null as string | null, video_asset_url: null as string | null };
    a = applyAssetFrame(a, { kind: "poster", url: "/assets/p.svg" });
    expect(a.poster_asset_url).toBe("/assets/p.svg");
    a = applyAssetFrame(a, { kind: "market_video", url: "/assets/v.svg" });
    expect(a.video_asset_url).toBe("/assets/v.svg");
    expect(preferredMediaUrl(a).kind).toBe("video");
  });

  it("rehydrates from snapshot without losing unspecified fields", () => {
    const prev = {
      poster_asset_url: "/old-poster.svg",
      video_asset_url: "/old-video.svg",
    };
    const next = rehydrateAssetsFromSnapshot(
      { poster_asset_url: "/new-poster.svg" },
      prev,
    );
    expect(next.poster_asset_url).toBe("/new-poster.svg");
    expect(next.video_asset_url).toBe("/old-video.svg");
  });

  it("snapshot null clears to placeholder path", () => {
    const next = rehydrateAssetsFromSnapshot({
      poster_asset_url: null,
      video_asset_url: null,
    });
    expect(preferredMediaUrl(next).kind).toBe("placeholder");
  });
});

describe("slot countdown", () => {
  it("msUntilPublish decreases with mono clock", () => {
    const publishAt = new Date(1_700_000_000_000 + 60_000).toISOString();
    const serverNow = 1_700_000_000_000;
    const mono0 = 1000;
    expect(msUntilPublish(publishAt, serverNow, mono0, mono0)).toBe(60_000);
    expect(msUntilPublish(publishAt, serverNow, mono0, mono0 + 10_000)).toBe(
      50_000,
    );
  });
});

describe("upcoming rail degrade / honesty", () => {
  it("shows empty designed state when no drafts (not inventing markets)", () => {
    const rail = buildUpcomingRail([], 0, 1);
    expect(rail).toEqual([{ kind: "empty", id: "empty-0" }]);
  });

  it("lists real scheduled drafts sorted by publish_at", () => {
    const rail = buildUpcomingRail(
      [
        {
          id: "b",
          question: "Later",
          tier: "flash",
          publish_at: "2026-08-12T12:00:00Z",
          status: "approved",
        },
        {
          id: "a",
          question: "Sooner",
          tier: "flash",
          publish_at: "2026-08-12T11:00:00Z",
          status: "approved",
        },
      ],
      1,
      1,
    );
    expect(rail[0]).toMatchObject({
      kind: "draft",
      draft: { id: "a", question: "Sooner" },
    });
  });
});

describe("slot conflicts + fill rate", () => {
  it("flags double-booked publish_at within tier", () => {
    const c = findSlotConflicts([
      {
        id: "1",
        question: "q1",
        tier: "flash",
        publish_at: "2026-08-12T10:00:00Z",
      },
      {
        id: "2",
        question: "q2",
        tier: "flash",
        publish_at: "2026-08-12T10:00:00Z",
      },
      {
        id: "3",
        question: "q3",
        tier: "daily",
        publish_at: "2026-08-12T10:00:00Z",
      },
    ]);
    expect(c.has("1")).toBe(true);
    expect(c.has("2")).toBe(true);
    expect(c.has("3")).toBe(false);
  });

  it("formats fill rate", () => {
    expect(slotFillRate(3, 1)).toBe(0.75);
    expect(formatFillRate(0.75)).toBe("75%");
    expect(formatFillRate(null)).toBe("—");
  });
});

describe("share card + asset src", () => {
  it("builds share card path for img src", () => {
    expect(shareCardPath("u1", "m1")).toBe("/users/u1/share_card/m1");
  });

  it("resolves relative assets against core base", () => {
    expect(resolveAssetSrc("/assets/x.svg", "http://127.0.0.1:8080")).toBe(
      "http://127.0.0.1:8080/assets/x.svg",
    );
    expect(resolveAssetSrc("https://cdn/x.svg", "http://127.0.0.1:8080")).toBe(
      "https://cdn/x.svg",
    );
    expect(resolveAssetSrc(null, "http://127.0.0.1:8080")).toBeNull();
  });
});
