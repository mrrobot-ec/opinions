/**
 * Phase 7 money surfaces: balances, withdraw, KYC prompt, self-exclusion.
 * Endpoints may 404/503 until W1/W2 land; callers treat those as empty.
 */

import { ApiClientError, coreUrl, getDemoToken, getUserId } from "./api";

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
}

export interface WithdrawReceipt {
  id: string;
  status: string;
  review_state?: string;
  amount_micro: number;
}

export interface SelfExclusionRequest {
  cooling_off_hours: number;
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
    const err = body as { code?: string; message?: string } | null;
    throw new ApiClientError(
      res.status,
      err?.code ?? "http_error",
      err?.message ?? res.statusText,
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
  return moneyRequest<WithdrawReceipt>("/withdrawals", {
    method: "POST",
    body: JSON.stringify({ ...body, user_id: userId }),
  });
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
  return moneyRequest("/me/self-exclusion", {
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
