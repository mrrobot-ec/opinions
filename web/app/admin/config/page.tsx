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
  settleOpsConfigProposal,
} from "@/lib/api";
import { configValueText, isSensitiveConfigKey } from "@/lib/ops";
import type { ConfigSnapshotDto } from "@/lib/types";

export default function AdminConfigPage() {
  const [snapshot, setSnapshot] = useState<ConfigSnapshotDto | null>(null);
  const [keyName, setKeyName] = useState("");
  const [jsonValue, setJsonValue] = useState("");
  const [reason, setReason] = useState("");
  const [proposalId, setProposalId] = useState("");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adminReady, setAdminReady] = useState(false);

  const load = useCallback(async () => {
    if (!getAdminToken()) return;
    setError(null);
    try {
      setSnapshot(await getOpsConfig());
    } catch (cause) {
      setError(errorText(cause));
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

  function selectEntry(key: string, value: unknown) {
    setKeyName(key);
    setJsonValue(JSON.stringify(value));
    setMessage(null);
    setError(null);
  }

  async function apply() {
    if (!snapshot || !keyName.trim() || !reason.trim()) return;
    let value: unknown;
    try {
      value = JSON.parse(jsonValue);
    } catch {
      setError("Value must be valid JSON (strings need quotes).");
      return;
    }
    setBusy(true);
    setError(null);
    setMessage(null);
    const key = keyName.trim();
    try {
      if (isSensitiveConfigKey(key)) {
        const proposal = await createOpsConfigProposal({
          patch: { [key]: value },
          reason: reason.trim(),
          idempotency_key: newIdempotencyKey(),
        });
        setMessage(
          `Proposal ${proposal.id} is ${proposal.status}; a distinct principal must confirm it.`,
        );
        setProposalId(proposal.id);
      } else {
        const applied = await setOpsConfig({
          patch: { [key]: value },
          expected_base_generation: snapshot.generation,
          reason: reason.trim(),
          idempotency_key: newIdempotencyKey(),
        });
        setMessage(
          `Applied generation ${applied.generation}: ${applied.changed_keys.join(", ")}.`,
        );
        await load();
      }
    } catch (cause) {
      setError(errorText(cause));
    } finally {
      setBusy(false);
    }
  }

  async function settle(action: "confirm" | "reject") {
    if (!proposalId.trim()) return;
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      const proposal = await settleOpsConfigProposal(
        proposalId.trim(),
        action,
        reason.trim() || undefined,
      );
      setMessage(`Proposal ${proposal.id} is ${proposal.status}.`);
      await load();
    } catch (cause) {
      setError(errorText(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="admin-page stack">
      <AdminOpsNav />
      <header className="panel">
        <p className="eyebrow">Phase 6 control plane</p>
        <h1 className="page-title">Runtime config</h1>
        <p className="panel-hint">
          Whole-snapshot validation · generation serialized · sensitive keys
          use two-person proposals.
        </p>
      </header>

      {!adminReady && <AdminTokenRequired />}
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
        <>
          <section className="panel ops-editor">
            <div className="panel-head-row">
              <h2>Patch one key</h2>
              <span className="pill">generation {snapshot.generation}</span>
            </div>
            <div className="ops-form-grid">
              <label className="field">
                <span>Key</span>
                <input
                  value={keyName}
                  onChange={(event) => setKeyName(event.target.value)}
                />
              </label>
              <label className="field">
                <span>JSON value</span>
                <input
                  value={jsonValue}
                  onChange={(event) => setJsonValue(event.target.value)}
                />
              </label>
              <label className="field ops-reason">
                <span>Reason</span>
                <input
                  value={reason}
                  onChange={(event) => setReason(event.target.value)}
                />
              </label>
            </div>
            <button
              type="button"
              className="btn btn-primary"
              disabled={busy || !keyName.trim() || !reason.trim()}
              onClick={() => void apply()}
            >
              {busy
                ? "Submitting…"
                : isSensitiveConfigKey(keyName.trim())
                  ? "Create proposal"
                  : "Apply patch"}
            </button>
          </section>

          <section className="panel">
            <div className="panel-head-row">
              <h2>Settle a proposal</h2>
              <span className="pill">distinct token required</span>
            </div>
            <p className="panel-hint">
              Switch the Dev login admin token to a second authorized principal
              before confirming. Same-token confirmation fails closed.
            </p>
            <label className="field">
              <span>Proposal UUID</span>
              <input
                value={proposalId}
                onChange={(event) => setProposalId(event.target.value)}
              />
            </label>
            <div className="chips" style={{ marginTop: "0.75rem" }}>
              <button
                type="button"
                className="btn btn-primary"
                disabled={busy || !proposalId.trim()}
                onClick={() => void settle("confirm")}
              >
                Confirm proposal
              </button>
              <button
                type="button"
                className="btn"
                disabled={busy || !proposalId.trim()}
                onClick={() => void settle("reject")}
              >
                Reject proposal
              </button>
            </div>
          </section>

          <section className="panel ops-table-wrap">
            <h2>Committed snapshot</h2>
            <table className="table ops-table">
              <thead>
                <tr>
                  <th>Key</th>
                  <th>Value</th>
                  <th>Control</th>
                </tr>
              </thead>
              <tbody>
                {[...snapshot.entries]
                  .sort((left, right) => left.key.localeCompare(right.key))
                  .map((entry) => (
                    <tr key={entry.key}>
                      <td>
                        <button
                          type="button"
                          className="text-btn num"
                          onClick={() => selectEntry(entry.key, entry.value)}
                        >
                          {entry.key}
                        </button>
                      </td>
                      <td className="num ops-value">
                        {configValueText(entry.value)}
                      </td>
                      <td>
                        {isSensitiveConfigKey(entry.key)
                          ? "two-person"
                          : "direct"}
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </section>
        </>
      )}
    </div>
  );
}

function AdminTokenRequired() {
  return (
    <div className="state-box">
      <p>
        Set an admin token in the Dev login panel to use the ops control plane.
      </p>
    </div>
  );
}

function errorText(cause: unknown): string {
  return cause instanceof ApiClientError
    ? `${cause.message} (${cause.status})`
    : cause instanceof Error
      ? cause.message
      : "Config request failed";
}
