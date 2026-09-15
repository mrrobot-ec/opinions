"use client";

import { useEffect, useState } from "react";
import {
  clearAdminToken,
  clearDevSession,
  getAdminToken,
  getDemoToken,
  getUserId,
  setAdminToken,
  setDevSession,
} from "@/lib/api";

export default function DevLogin() {
  const [userId, setUserId] = useState("");
  const [token, setToken] = useState("");
  const [adminToken, setAdminTokenField] = useState("");
  const [open, setOpen] = useState(false);
  const [session, setSession] = useState<{ userId: string; token: string } | null>(
    null,
  );

  useEffect(() => {
    const u = getUserId();
    const t = getDemoToken();
    const a = getAdminToken();
    if (u && t) setSession({ userId: u, token: t });
    if (a) setAdminTokenField(a);
  }, []);

  function notifyAuth() {
    if (typeof window !== "undefined") {
      window.dispatchEvent(new Event("opinions-auth"));
    }
  }

  function save() {
    if (!userId.trim() || !token.trim()) return;
    setDevSession(userId.trim(), token.trim());
    if (adminToken.trim()) setAdminToken(adminToken.trim());
    else clearAdminToken();
    setSession({ userId: userId.trim(), token: token.trim() });
    setOpen(false);
    notifyAuth();
  }

  function logout() {
    clearDevSession();
    clearAdminToken();
    setSession(null);
    setUserId("");
    setToken("");
    setAdminTokenField("");
    notifyAuth();
  }

  return (
    <div className="dev-login">
      {session ? (
        <button
          type="button"
          className="btn btn-chip"
          onClick={() => setOpen((v) => !v)}
          title={session.userId}
        >
          DEV · {session.userId.slice(0, 8)}…
        </button>
      ) : (
        <button type="button" className="btn btn-chip" onClick={() => setOpen(true)}>
          Dev login
        </button>
      )}
      {open && (
        <div
          className="panel"
          style={{
            position: "absolute",
            right: "1rem",
            top: "calc(var(--banner-h) + var(--nav-h) + 0.5rem)",
            width: "min(340px, calc(100vw - 2rem))",
            zIndex: 50,
            boxShadow: "var(--shadow)",
          }}
        >
          <h3>DEV session</h3>
          <p className="panel-hint">
            Demo token is stored in localStorage — local demo exposure only. Not
            production auth.
          </p>
          <div className="field">
            <label htmlFor="dev-user">User UUID</label>
            <input
              id="dev-user"
              type="text"
              value={userId}
              onChange={(e) => setUserId(e.target.value)}
              placeholder="xxxxxxxx-xxxx-…"
              autoComplete="off"
            />
          </div>
          <div className="field">
            <label htmlFor="dev-token">Demo bearer token</label>
            <input
              id="dev-token"
              type="password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="demo token"
              autoComplete="off"
            />
          </div>
          <div className="field">
            <label htmlFor="dev-admin">Admin token (curation)</label>
            <input
              id="dev-admin"
              type="password"
              value={adminToken}
              onChange={(e) => setAdminTokenField(e.target.value)}
              placeholder="admin token"
              autoComplete="off"
            />
          </div>
          <div className="row">
            <button type="button" className="btn btn-primary" onClick={save}>
              Save
            </button>
            {session && (
              <button type="button" className="btn" onClick={logout}>
                Clear
              </button>
            )}
            <button type="button" className="btn" onClick={() => setOpen(false)}>
              Close
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
