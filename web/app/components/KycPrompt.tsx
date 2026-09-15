"use client";

import { useState } from "react";
import { ApiClientError } from "@/lib/api";
import { kycPromptCopy, startKyc } from "@/lib/money";

export default function KycPrompt({
  tier,
  userId,
}: {
  tier: number;
  userId: string | null;
}) {
  const [message, setMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const copy = kycPromptCopy(tier);
  const needs = tier < 2;

  return (
    <div className="panel" data-testid="kyc-prompt">
      <h2>Identity</h2>
      <p className="page-sub">{copy}</p>
      {needs && (
        <p style={{ marginTop: 12 }}>
          <button
            type="button"
            className="btn"
            disabled={!userId || busy}
            onClick={() => {
              void (async () => {
                setBusy(true);
                setMessage(null);
                try {
                  const started = await startKyc(userId);
                  setMessage(
                    started
                      ? `KYC challenge ${started.challenge_id}`
                      : "KYC is not available in this environment yet.",
                  );
                } catch (error) {
                  setMessage(
                    error instanceof ApiClientError
                      ? error.message
                      : "Could not start verification",
                  );
                } finally {
                  setBusy(false);
                }
              })();
            }}
          >
            Start verification
          </button>
        </p>
      )}
      {message && (
        <p className="page-sub" role="status" style={{ marginTop: 8 }}>
          {message}
        </p>
      )}
    </div>
  );
}
