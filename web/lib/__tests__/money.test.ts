import { afterEach, describe, expect, it, vi } from "vitest";

import { ApiClientError } from "../api";
import {
  getBalances,
  kycPromptCopy,
  parseUsdMicros,
  requestWithdraw,
  setSelfExclusion,
  startKyc,
  withdrawReceiptCopy,
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
      requestWithdraw(
        { amount_micro: 1, dest: "dest", confirm_dest: "dest" },
        null,
      ),
    ).rejects.toBeInstanceOf(ApiClientError);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(jsonResponse(422, { code: "KycRequired", message: "kyc" })),
    );
    await expect(
      requestWithdraw(
        {
          amount_micro: 5_000_000,
          dest: "Dest111",
          confirm_dest: "Dest111",
        },
        "user-1",
      ),
    ).rejects.toMatchObject({ status: 422, code: "KycRequired" });
  });

  it("sends destination confirmation and an idempotency key", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(200, {
        id: "withdrawal-1",
        user_id: "user-1",
        dest: "Dest111",
        amount_micro: 5_000_000,
        combo: "W1",
        hold_tx_id: "hold-1",
        replayed: false,
        refused: false,
        refuse_code: null,
        refuse_message: null,
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    await requestWithdraw(
      {
        amount_micro: 5_000_000,
        dest: "Dest111",
        confirm_dest: "Dest111",
      },
      "user-1",
    );

    const request = fetchMock.mock.calls[0][1] as RequestInit;
    expect(JSON.parse(String(request.body))).toMatchObject({
      user_id: "user-1",
      amount_micro: 5_000_000,
      dest: "Dest111",
      confirm_dest: "Dest111",
      idempotency_key: expect.any(String),
    });
  });

  it("reuses the withdrawal key after an ambiguous failure until fields change", async () => {
    const fetchMock = vi
      .fn()
      .mockRejectedValueOnce(new TypeError("connection reset"))
      .mockResolvedValue(
        jsonResponse(200, {
          id: "withdrawal-1",
          user_id: "user-1",
          dest: "DestRetry",
          amount_micro: 5_000_000,
          combo: "W1",
          hold_tx_id: "hold-1",
          replayed: true,
          refused: false,
          refuse_code: null,
          refuse_message: null,
        }),
      );
    vi.stubGlobal("fetch", fetchMock);
    const body = {
      amount_micro: 5_000_000,
      dest: "DestRetry",
      confirm_dest: "DestRetry",
    };

    await expect(requestWithdraw(body, "user-1")).rejects.toThrow("connection reset");
    await requestWithdraw(body, "user-1");
    await requestWithdraw({ ...body, amount_micro: 6_000_000 }, "user-1");

    const sent = fetchMock.mock.calls.map((call) =>
      JSON.parse(String((call[1] as RequestInit).body)),
    );
    expect(sent[1].idempotency_key).toBe(sent[0].idempotency_key);
    expect(sent[2].idempotency_key).not.toBe(sent[1].idempotency_key);
  });

  it("rotates the withdrawal key after a definitive successful receipt", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      jsonResponse(200, {
        id: "withdrawal-success",
        user_id: "user-success",
        dest: "DestSuccess",
        amount_micro: 7_000_000,
        combo: "W2",
        hold_tx_id: "hold-success",
        replayed: false,
        refused: false,
        refuse_code: null,
        refuse_message: null,
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const body = {
      amount_micro: 7_000_000,
      dest: "DestSuccess",
      confirm_dest: "DestSuccess",
    };

    await requestWithdraw(body, "user-success");
    await requestWithdraw(body, "user-success");

    const keys = fetchMock.mock.calls.map(
      (call) => JSON.parse(String((call[1] as RequestInit).body)).idempotency_key,
    );
    expect(keys[1]).not.toBe(keys[0]);
  });

  it("parses withdrawal USD as exact integer micros", () => {
    expect(parseUsdMicros("5.123456")).toBe(5_123_456);
    expect(parseUsdMicros("0.000001")).toBe(1);
    for (const invalid of ["1e3", "NaN", "Infinity", "1.0000001", "-1", "0"]) {
      expect(parseUsdMicros(invalid)).toBeNull();
    }
  });

  it("labels a typed refusal as refused rather than accepted", () => {
    expect(
      withdrawReceiptCopy({
        id: null,
        user_id: "user-1",
        dest: "Dest111",
        amount_micro: 5_000_000,
        combo: null,
        hold_tx_id: null,
        replayed: false,
        refused: true,
        refuse_code: "kyc",
        refuse_message: "Full KYC is required",
      }),
    ).toBe("Withdrawal refused (kyc): Full KYC is required.");
  });

  it("surfaces the typed refusal envelope returned by withdrawal rails", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        jsonResponse(403, {
          refused: true,
          refuse_code: "kyc",
          refuse_message: "Full KYC is required",
        }),
      ),
    );

    await expect(
      requestWithdraw(
        {
          amount_micro: 5_000_000,
          dest: "Dest111",
          confirm_dest: "Dest111",
        },
        "user-1",
      ),
    ).rejects.toMatchObject({
      status: 403,
      code: "kyc",
      message: "Full KYC is required",
    });
  });

  it("kyc start degrades on 503 and self-exclusion posts the cooling-off", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(jsonResponse(503, {})));
    await expect(startKyc("user-1")).resolves.toBeNull();
    await expect(startKyc(null)).resolves.toBeNull();

    const fetchMock = vi.fn().mockResolvedValue(
        jsonResponse(200, { id: "ex-1", cooling_off_until: "2026-09-01T00:00:00Z" }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await expect(setSelfExclusion({ cooling_off_hours: 24 }, "user-1")).resolves.toEqual({
      id: "ex-1",
      cooling_off_until: "2026-09-01T00:00:00Z",
    });
    await expect(setSelfExclusion({ cooling_off_hours: 24 }, null)).rejects.toBeInstanceOf(
      ApiClientError,
    );
    expect(String(fetchMock.mock.calls[0][0])).toContain("/self_exclusions");
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
