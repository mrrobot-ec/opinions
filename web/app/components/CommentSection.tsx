"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ApiClientError,
  getComments,
  getUserId,
  postComment,
  reportComment,
  voteComment,
} from "@/lib/api";
import {
  SOCIAL_COPY,
  apiErrorCopy,
  blockedReasonCopy,
} from "@/lib/copy";
import {
  commentIndentPx,
  optimisticCommentVote,
  reconcileCommentVote,
  type CommentSort,
} from "@/lib/social";
import type { CommentDto } from "@/lib/types";

export default function CommentSection({
  marketId,
  marketTerminal,
}: {
  marketId: string;
  marketTerminal?: boolean;
}) {
  const [sort, setSort] = useState<CommentSort>("hot");
  const [items, setItems] = useState<CommentDto[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [available, setAvailable] = useState(true);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [body, setBody] = useState("");
  const [replyTo, setReplyTo] = useState<CommentDto | null>(null);
  const [postBusy, setPostBusy] = useState(false);
  const [userId, setUserId] = useState<string | null>(null);
  const [flash, setFlash] = useState<string | null>(null);

  const load = useCallback(
    async (opts: { append?: boolean; cursor?: string | null } = {}) => {
      setError(null);
      try {
        const uid = getUserId();
        setUserId(uid);
        const page = await getComments(marketId, {
          sort,
          limit: 40,
          cursor: opts.cursor ?? null,
          viewerId: uid,
        });
        if (page === null) {
          setAvailable(false);
          setItems([]);
          return;
        }
        setAvailable(true);
        setItems((prev) =>
          opts.append ? [...prev, ...page.items] : page.items,
        );
        setCursor(page.next_cursor ?? null);
      } catch (e) {
        setError(
          e instanceof Error ? e.message : "Failed to load comments",
        );
      } finally {
        setLoading(false);
      }
    },
    [marketId, sort],
  );

  useEffect(() => {
    setLoading(true);
    setCursor(null);
    void load();
    const sync = () => setUserId(getUserId());
    window.addEventListener("opinions-auth", sync);
    return () => window.removeEventListener("opinions-auth", sync);
  }, [load]);

  const roots = useMemo(() => {
    // Render flat with depth indent (server may return mixed page);
    // keep list order from API (hot/recent).
    return items;
  }, [items]);

  async function onPost() {
    const uid = getUserId();
    if (!uid) {
      setError(SOCIAL_COPY.login_to_comment);
      return;
    }
    if (marketTerminal) {
      setError("Comments are closed on resolved markets.");
      return;
    }
    setPostBusy(true);
    setError(null);
    try {
      await postComment({
        market_id: marketId,
        user_id: uid,
        body,
        parent_id: replyTo?.id ?? null,
      });
      setBody("");
      setReplyTo(null);
      await load();
    } catch (e) {
      if (e instanceof ApiClientError) {
        const reason = e.code.replace(/^blocked_?/i, "") || e.message;
        setError(
          apiErrorCopy(e.code, blockedReasonCopy(reason) || e.message),
        );
      } else {
        setError(e instanceof Error ? e.message : "Post failed");
      }
    } finally {
      setPostBusy(false);
    }
  }

  async function onVote(c: CommentDto, value: 1 | -1) {
    const uid = getUserId();
    if (!uid) {
      setError(SOCIAL_COPY.login_to_vote);
      return;
    }
    if (c.moderation_status !== "visible") return;
    const prev = (c.viewer_vote ?? 0) as 0 | 1 | -1;
    const opt = optimisticCommentVote(c.score, prev, value);
    if (!opt) {
      setError(SOCIAL_COPY.duplicate_vote);
      return;
    }
    setItems((list) =>
      list.map((x) =>
        x.id === c.id
          ? { ...x, score: opt.score, viewer_vote: opt.value }
          : x,
      ),
    );
    try {
      const res = await voteComment({
        comment_id: c.id,
        user_id: uid,
        value,
      });
      const recon = reconcileCommentVote(opt.score, opt.value, {
        ok: true,
        score: res.score ?? opt.score,
        value: (res.value ?? opt.value) as 1 | -1,
      });
      setItems((list) =>
        list.map((x) =>
          x.id === c.id
            ? { ...x, score: recon.score, viewer_vote: recon.value as 1 | -1 }
            : x,
        ),
      );
    } catch (e) {
      const recon = reconcileCommentVote(opt.score, opt.value, { ok: false });
      setItems((list) =>
        list.map((x) =>
          x.id === c.id
            ? {
                ...x,
                score: recon.score,
                viewer_vote: recon.value === 0 ? null : recon.value,
              }
            : x,
        ),
      );
      if (e instanceof ApiClientError) {
        setError(apiErrorCopy(e.code, e.message));
      }
    }
  }

  async function onReport(c: CommentDto) {
    const uid = getUserId();
    if (!uid) {
      setError(SOCIAL_COPY.login_to_vote);
      return;
    }
    if (c.moderation_status !== "visible") return;
    try {
      await reportComment({ comment_id: c.id, user_id: uid });
      setFlash(SOCIAL_COPY.report_confirmation);
      window.setTimeout(() => setFlash(null), 3000);
    } catch (e) {
      if (e instanceof ApiClientError) {
        setError(apiErrorCopy(e.code, e.message));
      } else {
        setError(e instanceof Error ? e.message : "Report failed");
      }
    }
  }

  if (!available && !loading) {
    return (
      <div className="panel comments-panel">
        <h3>Comments</h3>
        <p className="panel-hint">{SOCIAL_COPY.comments_unavailable}</p>
      </div>
    );
  }

  return (
    <div className="panel comments-panel">
      <div className="panel-head-row">
        <h3>Comments</h3>
        <div className="sort-toggle" role="group" aria-label="Comment sort">
          <button
            type="button"
            className={`btn btn-chip ${sort === "hot" ? "active" : ""}`}
            onClick={() => setSort("hot")}
          >
            Hot
          </button>
          <button
            type="button"
            className={`btn btn-chip ${sort === "recent" ? "active" : ""}`}
            onClick={() => setSort("recent")}
          >
            Recent
          </button>
        </div>
      </div>

      {flash && (
        <p className="panel-hint" role="status" style={{ color: "var(--yes)" }}>
          {flash}
        </p>
      )}
      {error && (
        <p className="field-error" role="alert">
          {error}
        </p>
      )}

      {!marketTerminal && (
        <div className="comment-compose">
          {replyTo && (
            <p className="panel-hint">
              Replying to @{replyTo.author_handle}{" "}
              <button
                type="button"
                className="btn-link"
                onClick={() => setReplyTo(null)}
              >
                cancel
              </button>
            </p>
          )}
          <textarea
            className="comment-input"
            rows={3}
            maxLength={4000}
            placeholder={
              userId
                ? "Write a comment — use @handle to mention"
                : SOCIAL_COPY.login_to_comment
            }
            value={body}
            onChange={(e) => setBody(e.target.value)}
            disabled={!userId || postBusy}
          />
          <div className="row">
            <button
              type="button"
              className="btn btn-primary"
              disabled={!userId || postBusy || !body.trim()}
              onClick={() => void onPost()}
            >
              {postBusy ? "Posting…" : "Post"}
            </button>
          </div>
        </div>
      )}

      {loading ? (
        <div className="skeleton" style={{ height: 80 }} />
      ) : roots.length === 0 ? (
        <p className="panel-hint">{SOCIAL_COPY.comments_empty}</p>
      ) : (
        <ul className="comment-list">
          {roots.map((c) => {
            const shadowed = c.moderation_status === "shadow";
            const visible = c.moderation_status === "visible";
            const isOwnShadow =
              shadowed && userId != null && c.author_id === userId;
            if (shadowed && !isOwnShadow) return null;
            return (
              <li
                key={c.id}
                className={`comment-item ${shadowed ? "shadowed" : ""}`}
                style={{ marginLeft: commentIndentPx(c.depth) }}
              >
                <div className="comment-meta">
                  <span className="handle">@{c.author_handle || "anon"}</span>
                  <span className="num comment-score">{c.score}</span>
                </div>
                {isOwnShadow && (
                  <p className="shadow-banner" role="status">
                    {SOCIAL_COPY.shadow_banner}
                  </p>
                )}
                {/* body is plain text children only — React escapes by default */}
                <p className="comment-body">{c.body}</p>
                {visible && (
                  <div className="comment-actions row">
                    <button
                      type="button"
                      className="btn btn-chip"
                      aria-label="Upvote"
                      disabled={!!c.viewer_vote}
                      onClick={() => void onVote(c, 1)}
                    >
                      ▲
                    </button>
                    <button
                      type="button"
                      className="btn btn-chip"
                      aria-label="Downvote"
                      disabled={!!c.viewer_vote}
                      onClick={() => void onVote(c, -1)}
                    >
                      ▼
                    </button>
                    {!marketTerminal && (
                      <button
                        type="button"
                        className="btn btn-chip"
                        onClick={() => setReplyTo(c)}
                      >
                        Reply
                      </button>
                    )}
                    <button
                      type="button"
                      className="btn btn-chip"
                      onClick={() => void onReport(c)}
                    >
                      Report
                    </button>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}

      {cursor && (
        <button
          type="button"
          className="btn"
          style={{ marginTop: "0.75rem" }}
          onClick={() => void load({ append: true, cursor })}
        >
          Load more
        </button>
      )}
    </div>
  );
}
