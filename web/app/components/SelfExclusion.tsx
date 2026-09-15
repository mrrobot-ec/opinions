"use client";

import { useState } from "react";
import { ApiClientError } from "@/lib/api";
import { setSelfExclusion } from "@/lib/money";

export default function SelfExclusion({
  userId,
  excluded,
  until,
}: {
  userId: string | null;
  excluded: boolean;
  until: string | null;
}) {
  const [hours, setHours] = useState(24);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  if (excluded) {
    return (
      <div className="state-box" data-testid="self-exclusion">
        <h2>Self-exclusion is active</h2>
        <p>
          Trading, deposits, grants, and new-dest withdrawals are blocked until{" "}
          {until ?? "the cooling-off ends"}. This cannot be reversed early.
        </p>
      </div>
    );
  }

  return (
    <div className="panel" data-testid="self-exclusion">
      <h2>Self-exclusion</h2>
      <p className="page-sub">
        Immediate and irreversible for the cooling-off interval you stamp.
      </p>
      <label className="page-sub" htmlFor="cooling-off">
        Cooling-off hours
      </label>
      <input
        id="cooling-off"
        className="input"
        type="number"
        min={24}
        value={hours}
        onChange={(event) => setHours(Number(event.target.value))}
      />
      <p style={{ marginTop: 12 }}>
        <button
          type="button"
          className="btn"
          disabled={!userId || busy}
          onClick={() => {
            void (async () => {
              setBusy(true);
              setError(null);
              try {
                await setSelfExclusion({ cooling_off_hours: hours }, userId);
              } catch (err) {
                setError(
                  err instanceof ApiClientError ? err.message : "Could not stamp exclusion",
                );
              } finally {
                setBusy(false);
              }
            })();
          }}
        >
          Exclude me
        </button>
      </p>
      {error && (
        <p className="page-sub" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
