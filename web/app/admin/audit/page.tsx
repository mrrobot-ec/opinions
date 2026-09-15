"use client";

import { useCallback, useEffect, useState } from "react";
import AdminOpsNav from "@/app/components/AdminOpsNav";
import { ApiClientError, getAdminToken, getOpsAudit } from "@/lib/api";
import { redactTokenDigest } from "@/lib/ops";
import type { AuditPageDto } from "@/lib/types";

export default function AdminAuditPage() {
  const [page, setPage] = useState<AuditPageDto | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [adminReady, setAdminReady] = useState(false);

  const load = useCallback(async (before?: string) => {
    if (!getAdminToken()) return;
    setBusy(true);
    setError(null);
    try {
      const next = await getOpsAudit(before);
      setPage((previous) =>
        before && previous
          ? {
              actions: [...previous.actions, ...next.actions],
              next_before: next.next_before,
            }
          : next,
      );
    } catch (cause) {
      setError(
        cause instanceof ApiClientError
          ? `${cause.message} (${cause.status})`
          : "Audit read failed",
      );
    } finally {
      setBusy(false);
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

  return (
    <div className="admin-page stack">
      <AdminOpsNav />
      <header className="panel">
        <p className="eyebrow">Append-only evidence</p>
        <h1 className="page-title">Admin audit</h1>
        <p className="panel-hint">
          Requires the audit-read capability. Token material is shown only as
          a redacted digest.
        </p>
      </header>
      {!adminReady && (
        <div className="state-box">
          <p>Set an audit-capable admin token in the Dev login panel.</p>
        </div>
      )}
      {error && (
        <p className="field-error" role="alert">
          {error}
        </p>
      )}
      {adminReady && page && (
        <section className="panel ops-table-wrap">
          <table className="table ops-table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Actor</th>
                <th>Action</th>
                <th>Subject / reason</th>
              </tr>
            </thead>
            <tbody>
              {page.actions.map((action) => (
                <tr key={action.id}>
                  <td className="num">
                    {new Date(action.at).toLocaleString()}
                  </td>
                  <td>
                    <strong>{action.actor_role}</strong>
                    <br />
                    <span className="num panel-hint">
                      {redactTokenDigest(action.actor_token_digest)}
                    </span>
                  </td>
                  <td className="num">{action.action}</td>
                  <td>
                    {action.subject}
                    {action.reason && (
                      <>
                        <br />
                        <span className="panel-hint">{action.reason}</span>
                      </>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {page.actions.length === 0 && (
            <p className="panel-hint">No audit actions in this page.</p>
          )}
          {page.next_before && (
            <button
              type="button"
              className="btn"
              disabled={busy}
              onClick={() => void load(page.next_before ?? undefined)}
            >
              {busy ? "Loading…" : "Load older"}
            </button>
          )}
        </section>
      )}
    </div>
  );
}
