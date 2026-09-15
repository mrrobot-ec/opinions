import { describe, expect, it } from "vitest";
import {
  isUnderReview,
  noPctFromYes,
  padComingSoon,
  pickHeroMarket,
  railMarkets,
  yesPctFromMicro,
} from "../marketUi";
import type { MarketSummaryDto } from "../types";

function m(
  partial: Partial<MarketSummaryDto> & Pick<MarketSummaryDto, "id" | "slug" | "state">,
): MarketSummaryDto {
  return {
    yes_outcome_id: "y",
    no_outcome_id: "n",
    price_yes_micro: 500_000,
    price_no_micro: 500_000,
    ...partial,
  };
}

describe("marketUi helpers", () => {
  it("pickHeroMarket prefers live then scheduled", () => {
    expect(pickHeroMarket([])).toBeNull();
    const rows = [
      m({ id: "1", slug: "a", state: "resolved" }),
      m({ id: "2", slug: "b", state: "scheduled" }),
      m({ id: "3", slug: "c", state: "live" }),
    ];
    expect(pickHeroMarket(rows)?.id).toBe("3");
    expect(pickHeroMarket(rows.slice(0, 2))?.id).toBe("2");
  });

  it("railMarkets drops drafts", () => {
    const rows = [
      m({ id: "1", slug: "a", state: "draft" }),
      m({ id: "2", slug: "b", state: "live" }),
    ];
    expect(railMarkets(rows).map((x) => x.id)).toEqual(["2"]);
  });

  it("padComingSoon fills to min slots", () => {
    expect(padComingSoon(0, 3)).toHaveLength(3);
    expect(padComingSoon(2, 3)).toHaveLength(1);
    expect(padComingSoon(5, 3)).toHaveLength(0);
    expect(padComingSoon(0, 3)[0]?.kind).toBe("coming_soon");
  });

  it("yes/no pct from micro is display-only and sums to 100", () => {
    expect(yesPctFromMicro(310_000)).toBe(31);
    expect(noPctFromYes(31)).toBe(69);
    expect(yesPctFromMicro(500_000)).toBe(50);
    expect(yesPctFromMicro(1_000_000)).toBe(100);
    expect(noPctFromYes(100)).toBe(0);
  });

  it("isUnderReview from flag or resolving state", () => {
    expect(isUnderReview({ state: "live" })).toBe(false);
    expect(isUnderReview({ state: "resolving" })).toBe(true);
    expect(isUnderReview({ state: "live", under_review: true })).toBe(true);
    expect(isUnderReview({ state: "closed", under_review: false })).toBe(false);
  });
});
