"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  getDemoToken,
  getNotifications,
  getUnreadCount,
  getUserId,
  markNotificationsRead,
} from "@/lib/api";
import {
  formatNotificationText,
  shouldToastNotifType,
} from "@/lib/copy";
import { formatPnl } from "@/lib/format";
import { defaultWsUrl, UserNotifSocket } from "@/lib/ws";
import type { NotificationDto, UserServerFrame } from "@/lib/types";

interface ToastItem {
  id: string;
  text: string;
}

export default function NotificationBell() {
  const [userId, setUserId] = useState<string | null>(null);
  const [unread, setUnread] = useState(0);
  const [open, setOpen] = useState(false);
  const [items, setItems] = useState<NotificationDto[]>([]);
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const [available, setAvailable] = useState(true);
  const panelRef = useRef<HTMLDivElement>(null);

  const refreshList = useCallback(async (uid: string) => {
    const page = await getNotifications(uid, { limit: 30 });
    if (page === null) {
      setAvailable(false);
      return;
    }
    setAvailable(true);
    setItems(page.items);
  }, []);

  const refreshUnread = useCallback(async (uid: string) => {
    const c = await getUnreadCount(uid);
    if (c) setUnread(c.unread_count);
  }, []);

  useEffect(() => {
    const sync = () => setUserId(getUserId());
    sync();
    window.addEventListener("opinions-auth", sync);
    window.addEventListener("focus", sync);
    return () => {
      window.removeEventListener("opinions-auth", sync);
      window.removeEventListener("focus", sync);
    };
  }, []);

  useEffect(() => {
    if (!userId) {
      setUnread(0);
      setItems([]);
      return;
    }
    void refreshUnread(userId);
    void refreshList(userId);
  }, [userId, refreshList, refreshUnread]);

  useEffect(() => {
    if (!userId) return;
    const token = getDemoToken();
    if (!token) return;

    const sock = new UserNotifSocket(defaultWsUrl(), (frame: UserServerFrame) => {
      if (frame.type === "notif_snapshot") {
        setUnread(frame.unread_count);
        return;
      }
      if (frame.type === "notif") {
        const ntype = frame.notif_type || frame.notifType || "notif";
        setUnread((u) => u + 1);
        const text = formatNotificationText(ntype, frame.payload as never, {
          formatPnl,
          formatScore: (bp) => `${(bp / 100).toFixed(0)}`,
        });
        if (shouldToastNotifType(ntype)) {
          const tid = `${frame.id}:${frame.source_seq ?? 0}`;
          setToasts((t) => [...t, { id: tid, text }].slice(-5));
          window.setTimeout(() => {
            setToasts((t) => t.filter((x) => x.id !== tid));
          }, 5000);
        }
        // Badge-only for reply/mention; always refresh list if open
        setItems((prev) => {
          if (prev.some((p) => p.id === frame.id)) return prev;
          const row: NotificationDto = {
            id: frame.id,
            user_id: userId,
            type: ntype,
            market_id: frame.market_id,
            payload: frame.payload,
            read_at: null,
            created_at: frame.created_at ?? new Date().toISOString(),
            source_seq: frame.source_seq,
          };
          return [row, ...prev].slice(0, 50);
        });
      }
    });
    sock.subscribeUser(userId, token);
    return () => sock.unsubscribe();
  }, [userId]);

  useEffect(() => {
    if (!open) return;
    function onDoc(e: MouseEvent) {
      if (
        panelRef.current &&
        !panelRef.current.contains(e.target as Node)
      ) {
        setOpen(false);
      }
    }
    document.addEventListener("mousedown", onDoc);
    return () => document.removeEventListener("mousedown", onDoc);
  }, [open]);

  async function openPanel() {
    const next = !open;
    setOpen(next);
    if (!next || !userId) return;
    await refreshList(userId);
    const unreadIds = items
      .filter((i) => !i.read_at)
      .map((i) => i.id);
    // mark-read on open (scoped route)
    const ids =
      unreadIds.length > 0
        ? unreadIds
        : (await getNotifications(userId, { limit: 30 }))?.items
            .filter((i) => !i.read_at)
            .map((i) => i.id) ?? [];
    if (ids.length > 0) {
      try {
        await markNotificationsRead(userId, ids);
        setUnread(0);
        setItems((list) =>
          list.map((i) =>
            ids.includes(i.id)
              ? { ...i, read_at: new Date().toISOString() }
              : i,
          ),
        );
      } catch {
        /* optional until route lands */
      }
    }
  }

  if (!userId) {
    return null;
  }

  return (
    <div className="notif-bell-wrap" ref={panelRef}>
      <button
        type="button"
        className="notif-bell"
        aria-label={`Notifications${unread ? `, ${unread} unread` : ""}`}
        onClick={() => void openPanel()}
      >
        <span aria-hidden>🔔</span>
        {unread > 0 && (
          <span className="notif-badge num">{unread > 99 ? "99+" : unread}</span>
        )}
      </button>

      {open && (
        <div className="notif-panel" role="dialog" aria-label="Notifications">
          <div className="notif-panel-head">Notifications</div>
          {!available ? (
            <p className="panel-hint">Notifications not available yet.</p>
          ) : items.length === 0 ? (
            <p className="panel-hint">No notifications yet.</p>
          ) : (
            <ul className="notif-list">
              {items.map((n) => (
                <li
                  key={`${n.id}:${n.source_seq ?? ""}`}
                  className={n.read_at ? "read" : "unread"}
                >
                  {formatNotificationText(n.type, n.payload as never, {
                    formatPnl,
                    formatScore: (bp) => `${(bp / 100).toFixed(0)}`,
                  })}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <div className="toast-stack" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className="toast">
            {t.text}
          </div>
        ))}
      </div>
    </div>
  );
}
