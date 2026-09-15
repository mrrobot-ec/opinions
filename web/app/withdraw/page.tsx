"use client";

import Link from "next/link";
import { useCallback, useEffect, useState } from "react";
import CreditBalance from "@/app/components/CreditBalance";
import KycPrompt from "@/app/components/KycPrompt";
import SelfExclusion from "@/app/components/SelfExclusion";
import { ApiClientError, getUserId } from "@/lib/api";
import { formatDollars } from "@/lib/format";
import {
  getBalances,
  parseUsdMicros,
  requestWithdraw,
  withdrawReceiptCopy,
  type MoneyBalances,
  type WithdrawReceipt,
} from "@/lib/money";

export default function WithdrawPage() {
  const [userId, setUserId] = useState<string | null>(null);
  const [balances, setBalances] = useState<MoneyBalances | null>(null);
  const [amount, setAmount] = useState("5.00");
  const [dest, setDest] = useState("");
  const [confirmDest, setConfirmDest] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [receipt, setReceipt] = useState<WithdrawReceipt | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    const uid = getUserId();
    setUserId(uid);
    if (!uid) {
      setBalances(null);
      return;
    }
    try {
      setBalances(await getBalances(uid));
    } catch (err) {
      setError(err instanceof ApiClientError ? err.message : "Could not load balances");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const amountMicro = parseUsdMicros(amount);

  return (
    <>
      <h1 className="page-title">Withdraw</h1>
      <p className="page-sub">
        Hold-first cash withdrawal to a dest you own.{" "}
        <Link href="/portfolio">Back to portfolio</Link>
      </p>

      {!userId && (
        <div className="state-box">
          <h2>Dev login required</h2>
          <p>Open Dev login, paste your user UUID and demo token, then return here.</p>
        </div>
      )}

      <div className="stack" style={{ gap: 16 }}>
        <CreditBalance balances={balances} />
        <KycPrompt tier={balances?.kyc_tier ?? 0} userId={userId} />
        <SelfExclusion
          userId={userId}
          excluded={balances?.self_excluded ?? false}
          until={balances?.cooling_off_until ?? null}
        />

        <form
          className="panel"
          data-testid="withdraw-form"
          onSubmit={(event) => {
            event.preventDefault();
            if (amountMicro === null) {
              setError("Enter a positive USD amount with at most 6 decimal places");
              return;
            }
            void (async () => {
              setBusy(true);
              setError(null);
              setReceipt(null);
              try {
                const next = await requestWithdraw(
                  {
                    amount_micro: amountMicro,
                    dest: dest.trim(),
                    confirm_dest: confirmDest.trim(),
                  },
                  userId,
                );
                setReceipt(next);
              } catch (err) {
                setError(
                  err instanceof ApiClientError ? err.message : "Withdrawal was refused",
                );
              } finally {
                setBusy(false);
              }
            })();
          }}
        >
          <h2>Request cash out</h2>
          {balances && (
            <p className="page-sub">
              Available cash {formatDollars(balances.cash_micro)}. Credits cannot be
              withdrawn.
            </p>
          )}
          <label htmlFor="withdraw-amount">Amount (USD)</label>
          <input
            id="withdraw-amount"
            className="input"
            inputMode="decimal"
            value={amount}
            onChange={(event) => setAmount(event.target.value)}
          />
          <label htmlFor="withdraw-dest">Destination</label>
          <input
            id="withdraw-dest"
            className="input"
            value={dest}
            placeholder="USDC dest address"
            onChange={(event) => setDest(event.target.value)}
          />
          <label htmlFor="withdraw-confirm-dest">Re-enter destination</label>
          <input
            id="withdraw-confirm-dest"
            className="input"
            value={confirmDest}
            placeholder="Re-enter the same USDC destination"
            autoComplete="off"
            spellCheck={false}
            onChange={(event) => setConfirmDest(event.target.value)}
          />
          <p style={{ marginTop: 12 }}>
            <button
              type="submit"
              className="btn"
              disabled={
                !userId ||
                busy ||
                amountMicro === null ||
                !dest.trim() ||
                dest.trim() !== confirmDest.trim()
              }
            >
              Request withdrawal
            </button>
          </p>
          {error && (
            <p className="page-sub" role="alert" data-testid="withdraw-result">
              {error}
            </p>
          )}
          {receipt && (
            <p className="page-sub" role="status" data-testid="withdraw-result">
              {withdrawReceiptCopy(receipt)}
            </p>
          )}
        </form>
      </div>
    </>
  );
}
