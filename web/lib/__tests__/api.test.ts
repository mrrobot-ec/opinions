import { afterEach, describe, expect, it, vi } from "vitest";

import {
  getChart,
  getComments,
  getNotifications,
  getTape,
  getViewerVoteStatus,
  userProfile,
  voteComment,
} from "../api";

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

describe("viewer vote-status client", () => {
  it("scopes the read to the authenticated viewer without a user query", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ has_voted: true }),
    });
    vi.stubGlobal("window", {});
    vi.stubGlobal("localStorage", {
      getItem: (key: string) =>
        key === "opinions_demo_token"
          ? "demo-token"
          : key === "opinions_user_id"
            ? "viewer-1"
            : null,
    });
    vi.stubGlobal("fetch", fetchMock);

    await expect(getViewerVoteStatus("rain/tomorrow")).resolves.toEqual({
      has_voted: true,
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toContain("/markets/rain%2Ftomorrow/my-vote");
    expect(url).not.toContain("user_id=");
    expect(new Headers(init.headers).get("x-user-id")).toBe("viewer-1");
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

  it("supplies the idempotency key required by the comment-vote contract", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ score: 4 }),
    });
    vi.stubGlobal("fetch", fetchMock);

    await voteComment({ comment_id: "comment-1", user_id: "user-1", value: 1 });

    const request = fetchMock.mock.calls[0][1] as RequestInit;
    expect(JSON.parse(String(request.body))).toMatchObject({
      user_id: "user-1",
      value: 1,
      idempotency_key: expect.any(String),
    });
  });
});

describe("notifications client", () => {
  it("normalizes the core notification collection for the bell", async () => {
    const notification = {
      id: 7,
      type: "mention",
      market_id: null,
      payload: {},
      read_at: null,
      created_at: "2026-08-15T00:00:00Z",
      source_seq: null,
    };
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({ notifications: [notification], unread_count: 1 }),
      }),
    );

    await expect(getNotifications("user-1")).resolves.toEqual({
      items: [notification],
      unread_count: 1,
    });
  });
});

describe("profile client", () => {
  it("normalizes the nested voter summary and market references", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        text: async () =>
          JSON.stringify({
            user_id: "user-1",
            handle: "alice",
            created_at: "2026-08-01T00:00:00Z",
            rep_micro: 700_000,
            tier: 2,
            voter: { avg_score_bp: 7_250, markets_scored: 4 },
            realized_pnl_micro: 1_500_000,
            recent_trades: [
              {
                market_id: "market-1",
                market_ref: "rain-tomorrow",
                side: "yes",
                action: "buy",
                collateral_micro: 2_000_000,
                created_at: "2026-08-14T00:00:00Z",
                trade_seq: 9,
              },
            ],
            recent_votes: [
              {
                market_id: "market-1",
                market_question: "Will it rain tomorrow?",
                cast_at: "2026-08-14T00:00:00Z",
                side: "yes",
                score_bp: 8_000,
              },
            ],
          }),
      }),
    );

    await expect(userProfile("user-1")).resolves.toMatchObject({
      avg_score_bp: 7_250,
      markets_scored: 4,
      recent_trades: [{ market_slug: "rain-tomorrow" }],
      recent_votes: [{ market_question: "Will it rain tomorrow?" }],
    });
  });
});
