import { afterEach, describe, expect, it, vi } from "vitest";

import { ApiClientError } from "../api";
import {
  getBalances,
  kycPromptCopy,
  requestWithdraw,
  setSelfExclusion,
  startKyc,
} from "../money";

afterEach(() => {
  vi.unstubAllGlobals();
  if (typeof localStorage !== "undefined") {
    localStorage.clear();
  }
});

function jsonResponse(status: number, body: unknown) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: "x",
    text: async () => JSON.stringify(body),
  };
}

describe("money client", () => {
  it("returns null balances without a session or when the route is not up", async () => {
    await expect(getBalances(null)).resolves.toBeNull();
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(jsonResponse(404, { code: "missing" })),
    );
    await expect(getBalances("user-1")).resolves.toBeNull();
  });

  it("loads cash and credit balances", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(200, {
        cash_micro: 5_000_000,
        credit_micro: 1_000_000,
        withheld_micro: 0,
        kyc_tier: 1,
        self_excluded: false,
        cooling_off_until: null,
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await expect(getBalances("user-1")).resolves.toMatchObject({
      cash_micro: 5_000_000,
      credit_micro: 1_000_000,
      kyc_tier: 1,
    });
    expect(String(fetchMock.mock.calls[0][0])).toContain("/users/user-1/balances");
  });

  it("withdraw requires a session and surfaces API errors", async () => {
    await expect(
      requestWithdraw({ amount_micro: 1, dest: "dest" }, null),
    ).rejects.toBeInstanceOf(ApiClientError);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(jsonResponse(422, { code: "KycRequired", message: "kyc" })),
    );
    await expect(
      requestWithdraw({ amount_micro: 5_000_000, dest: "Dest111" }, "user-1"),
    ).rejects.toMatchObject({ status: 422, code: "KycRequired" });
  });

  it("kyc start degrades on 503 and self-exclusion posts the cooling-off", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse(503, {})));
    await expect(startKyc("user-1")).resolves.toBeNull();
    await expect(startKyc(null)).resolves.toBeNull();

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        jsonResponse(200, { id: "ex-1", cooling_off_until: "2026-09-01T00:00:00Z" }),
      ),
    );
    await expect(setSelfExclusion({ cooling_off_hours: 24 }, "user-1")).resolves.toEqual({
      id: "ex-1",
      cooling_off_until: "2026-09-01T00:00:00Z",
    });
    await expect(setSelfExclusion({ cooling_off_hours: 24 }, null)).rejects.toBeInstanceOf(
      ApiClientError,
    );
  });

  it("kyc copy tracks the withdraw tier", () => {
    expect(kycPromptCopy(0)).toMatch(/Verify your identity/);
    expect(kycPromptCopy(1)).toMatch(/Full verification/);
    expect(kycPromptCopy(2)).toMatch(/Identity verified/);
  });

  it("rethrows unexpected balance errors and parses non-json error bodies", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 500,
        statusText: "boom",
        text: async () => "not-json",
      }),
    );
    await expect(getBalances("user-1")).rejects.toMatchObject({ status: 500 });
  });
});
