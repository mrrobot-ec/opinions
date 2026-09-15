import { afterEach, describe, expect, it, vi } from "vitest";

import { getChart, getComments, getTape } from "../api";

afterEach(() => vi.unstubAllGlobals());

describe("market chart client", () => {
  it("always supplies the required since cursor", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => "[]",
    });
    vi.stubGlobal("fetch", fetchMock);

    await getChart("market-id");

    const url = new URL(fetchMock.mock.calls[0][0]);
    expect(url.searchParams.get("bucket")).toBe("60");
    expect(url.searchParams.get("since")).toMatch(/^\d{4}-\d{2}-\d{2}T/);
  });
});

describe("market tape client", () => {
  it("normalizes the REST trade_seq field used by the live tape key", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify([
            {
              handle: "alice",
              side: "yes",
              action: "buy",
              collateral_micro: 1_000_000,
              created_at: "2026-08-13T00:00:00Z",
              trade_seq: 7,
            },
          ]),
      }),
    );

    await expect(getTape("market-id")).resolves.toEqual([
      {
        handle: "alice",
        side: "yes",
        action: "buy",
        collateral_micro: 1_000_000,
        created_at: "2026-08-13T00:00:00Z",
        seq: 7,
      },
    ]);
  });
});

describe("market comments client", () => {
  it("normalizes the REST comments collection for the comment section", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        text: async () => JSON.stringify({ comments: [], next_cursor: null }),
      }),
    );

    await expect(getComments("market-id")).resolves.toEqual({
      items: [],
      next_cursor: null,
    });
  });
});
