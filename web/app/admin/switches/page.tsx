"use client";

import { useCallback, useEffect, useState } from "react";
import AdminOpsNav from "@/app/components/AdminOpsNav";
import {
  ApiClientError,
  createOpsConfigProposal,
  getAdminToken,
  getOpsConfig,
  newIdempotencyKey,
  setOpsConfig,
} from "@/lib/api";
import {
  isSensitiveConfigKey,
  switchRows,
  toggleSwitchPatch,
} from "@/lib/ops";
import type { ConfigSnapshotDto } from "@/lib/types";

export default function AdminSwitchesPage() {
  const [snapshot, setSnapshot] = useState<ConfigSnapshotDto | null>(null);
  const [reason, setReason] = useState("");
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adminReady, setAdminReady] = useState(false);

  const load = useCallback(async () => {
    if (!getAdminToken()) return;
    try {
      setSnapshot(await getOpsConfig());
      setError(null);
    } catch (cause) {
      setError(
        cause instanceof ApiClientError
          ? `${cause.message} (${cause.status})`
          : "Switch read failed",
      );
    }
  }, []);

  useEffect(() => {
    const sync = () => {
      setAdminReady(!!getAdminToken());
      void load();
    };
    sync();
    window.addEventListener("opinions-auth", sync);
    return () => window.removeEventListener("opinions-auth", sync);
  }, [load]);

  async function toggle(key: string, enabled: boolean) {
    if (!snapshot || !reason.trim()) return;
    setBusyKey(key);
    setMessage(null);
    setError(null);
    const patch = toggleSwitchPatch(key, enabled);
    try {
      if (isSensitiveConfigKey(key)) {
        const proposal = await createOpsConfigProposal({
          patch,
          reason: reason.trim(),
          idempotency_key: newIdempotencyKey(),
        });
        setMessage(
          `Voting-pause proposal ${proposal.id} awaits a distinct confirmer.`,
        );
      } else {
        const applied = await setOpsConfig({
          patch,
          expected_base_generation: snapshot.generation,
          reason: reason.trim(),
          idempotency_key: newIdempotencyKey(),
        });
        setMessage(`Switch committed at generation ${applied.generation}.`);
        await load();
      }
    } catch (cause) {
      setError(
        cause instanceof ApiClientError
          ? `${cause.message} (${cause.status})`
          : "Switch update failed",
      );
    } finally {
      setBusyKey(null);
    }
  }

  const rows = snapshot ? switchRows(snapshot) : [];
  return (
    <div className="admin-page stack">
      <AdminOpsNav />
      <header className="panel">
        <p className="eyebrow">Drain-safe fences</p>
        <h1 className="page-title">Kill switches</h1>
        <p className="panel-hint">
          Trading switches apply immediately. Voting pauses require two
          distinct principals and auto-expire at the hidden boundary.
        </p>
      </header>
      {!adminReady && (
        <div className="state-box">
          <p>Set an admin token in the Dev login panel.</p>
        </div>
      )}
      {error && (
        <p className="field-error" role="alert">
          {error}
        </p>
      )}
      {message && (
        <p className="ops-success" role="status">
          {message}
        </p>
      )}
      {adminReady && snapshot && (
        <section className="panel">
          <label className="field ops-reason">
            <span>Mandatory reason</span>
            <input
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              placeholder="Incident or change ticket"
            />
          </label>
          <div className="switch-list">
            {rows.map((row) => (
              <article className="switch-row" key={row.key}>
                <div>
                  <strong className="num">{row.key}</strong>
                  <p className="panel-hint">
                    {isSensitiveConfigKey(row.key)
                      ? "two-person control"
                      : "ops immediate"}
                  </p>
                </div>
                <button
                  type="button"
                  className={`btn ${row.enabled ? "btn-danger" : "btn-primary"}`}
                  disabled={busyKey !== null || !reason.trim()}
                  onClick={() => void toggle(row.key, row.enabled)}
                >
                  {busyKey === row.key
                    ? "Submitting…"
                    : row.enabled
                      ? "Resume"
                      : "Pause"}
                </button>
              </article>
            ))}
            {rows.length === 0 && (
              <p className="panel-hint">
                No pause keys are present in this snapshot.
              </p>
            )}
          </div>
        </section>
      )}
    </div>
  );
}
