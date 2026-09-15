/**
 * Phase 7 money surfaces: balances, withdraw, KYC prompt, self-exclusion.
 * Endpoints may 404/503 until W1/W2 land; callers treat those as empty.
 */

import {
  ApiClientError,
  coreUrl,
  getDemoToken,
  getUserId,
  newIdempotencyKey,
} from "./api";

export interface MoneyBalances {
  cash_micro: number;
  credit_micro: number;
  withheld_micro: number;
  kyc_tier: number;
  self_excluded: boolean;
  cooling_off_until: string | null;
}

export interface WithdrawRequest {
  amount_micro: number;
  dest: string;
  confirm_dest: string;
  idempotency_key?: string;
}

export interface WithdrawReceipt {
  id: string | null;
  user_id: string;
  dest: string;
  amount_micro: number;
  combo: string | null;
  hold_tx_id: string | null;
  replayed: boolean;
  refused: boolean;
  refuse_code: string | null;
  refuse_message: string | null;
}

export interface SelfExclusionRequest {
  cooling_off_hours: number;
}

let activeWithdrawalAttempt: {
  fingerprint: string;
  idempotencyKey: string;
} | null = null;

export function parseUsdMicros(input: string): number | null {
  const normalized = input.trim();
  if (normalized.length === 0 || normalized.length > 24) return null;
  const match = /^(0|[1-9]\d*)(?:\.(\d{1,6}))?$/.exec(normalized);
  if (!match) return null;

  const whole = BigInt(match[1]);
  const fractional = BigInt((match[2] ?? "").padEnd(6, "0") || "0");
  const micros = whole * BigInt(1_000_000) + fractional;
  if (micros <= BigInt(0) || micros > BigInt(Number.MAX_SAFE_INTEGER)) return null;
  return Number(micros);
}

export function withdrawReceiptCopy(receipt: WithdrawReceipt): string {
  if (receipt.refused) {
    const code = receipt.refuse_code ? ` (${receipt.refuse_code})` : "";
    const reason = (receipt.refuse_message ?? "The request was refused").replace(
      /\.+$/,
      "",
    );
    return `Withdrawal refused${code}: ${reason}.`;
  }
  return `Request ${receipt.id ?? "not created"} ${
    receipt.replayed ? "was already accepted" : "was accepted"
  }${receipt.combo ? ` (${receipt.combo})` : ""}.`;
}

async function moneyRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (!headers.has("Content-Type") && init.body) {
    headers.set("Content-Type", "application/json");
  }
  const token = getDemoToken();
  if (token) {
    headers.set("x-demo-token", token);
  }
  const res = await fetch(`${coreUrl()}${path}`, { ...init, headers });
  const text = await res.text();
  let body: unknown = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = { code: "parse_error", message: text };
    }
  }
  if (!res.ok) {
    const err = body as {
      code?: string;
      message?: string;
      refuse_code?: string;
      refuse_message?: string;
    } | null;
    throw new ApiClientError(
      res.status,
      err?.refuse_code ?? err?.code ?? "http_error",
      err?.refuse_message ?? err?.message ?? res.statusText,
    );
  }
  return body as T;
}

export async function getBalances(userId = getUserId()): Promise<MoneyBalances | null> {
  if (!userId) return null;
  try {
    return await moneyRequest<MoneyBalances>(
      `/users/${encodeURIComponent(userId)}/balances`,
      { method: "GET" },
    );
  } catch (error) {
    if (error instanceof ApiClientError && (error.status === 404 || error.status === 503)) {
      return null;
    }
    throw error;
  }
}

export async function requestWithdraw(
  body: WithdrawRequest,
  userId = getUserId(),
): Promise<WithdrawReceipt> {
  if (!userId) {
    throw new ApiClientError(401, "Unauthorized", "dev login required");
  }
  const fingerprint = JSON.stringify([
    userId,
    body.amount_micro,
    body.dest,
    body.confirm_dest,
  ]);
  let idempotencyKey = body.idempotency_key;
  if (!idempotencyKey) {
    if (activeWithdrawalAttempt?.fingerprint === fingerprint) {
      idempotencyKey = activeWithdrawalAttempt.idempotencyKey;
    } else {
      idempotencyKey = newIdempotencyKey();
    }
  }
  activeWithdrawalAttempt = { fingerprint, idempotencyKey };

  const clearCompletedAttempt = () => {
    if (
      activeWithdrawalAttempt?.fingerprint === fingerprint &&
      activeWithdrawalAttempt.idempotencyKey === idempotencyKey
    ) {
      activeWithdrawalAttempt = null;
    }
  };

  try {
    const receipt = await moneyRequest<WithdrawReceipt>("/withdrawals", {
      method: "POST",
      body: JSON.stringify({
        ...body,
        user_id: userId,
        idempotency_key: idempotencyKey,
      }),
    });
    clearCompletedAttempt();
    return receipt;
  } catch (error) {
    if (error instanceof ApiClientError) {
      clearCompletedAttempt();
    }
    throw error;
  }
}

export async function startKyc(userId = getUserId()): Promise<{ challenge_id: string } | null> {
  if (!userId) return null;
  try {
    return await moneyRequest<{ challenge_id: string }>("/kyc/start", {
      method: "POST",
      body: JSON.stringify({ user_id: userId }),
    });
  } catch (error) {
    if (error instanceof ApiClientError && (error.status === 404 || error.status === 503)) {
      return null;
    }
    throw error;
  }
}

export async function setSelfExclusion(
  body: SelfExclusionRequest,
  userId = getUserId(),
): Promise<{ id: string; cooling_off_until: string }> {
  if (!userId) {
    throw new ApiClientError(401, "Unauthorized", "dev login required");
  }
  return moneyRequest("/self_exclusions", {
    method: "POST",
    body: JSON.stringify({ ...body, user_id: userId }),
  });
}

export function kycPromptCopy(tier: number, withdrawKycTier = 2): string {
  if (tier >= withdrawKycTier) {
    return "Identity verified for withdrawals.";
  }
  if (tier === 1) {
    return "Basic KYC is on file. Full verification is required to withdraw.";
  }
  return "Verify your identity before you can withdraw cash.";
}
