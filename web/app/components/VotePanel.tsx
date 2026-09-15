"use client";

import type { SideDto, VoteReceiptDto } from "@/lib/types";

export default function VotePanel({
  hasVoted,
  voteReceipt,
  voteSide,
  setVoteSide,
  guess,
  setGuess,
  voteBusy,
  voteError,
  onVote,
  compact,
}: {
  hasVoted: boolean;
  voteReceipt: VoteReceiptDto | null;
  voteSide: SideDto;
  setVoteSide: (s: SideDto) => void;
  guess: number;
  setGuess: (n: number) => void;
  voteBusy: boolean;
  voteError: string | null;
  onVote: () => void;
  compact?: boolean;
}) {
  return (
    <div className={`panel vote-panel ${compact ? "panel-tight" : ""}`}>
      <div className="panel-head-row">
        <h3>Vote</h3>
        <a
          className="scoring-link"
          href="/how-it-works#scoring"
          title="How scoring works"
        >
          How scoring works
        </a>
      </div>
      {hasVoted ? (
        voteReceipt ? (
          <div>
            <p style={{ margin: 0 }}>
              Recorded{" "}
              <span
                className={voteReceipt.side === "yes" ? "pill yes" : "pill no"}
              >
                {voteReceipt.side.toUpperCase()}
              </span>{" "}
              · guess{" "}
              <strong className="num">{voteReceipt.crowd_guess_pct}%</strong>
              {voteReceipt.seq != null ? (
                <>
                  {" "}
                  · <strong className="num">#{voteReceipt.seq}</strong>
                </>
              ) : (
                <> · number hidden until resolve</>
              )}
            </p>
          </div>
        ) : (
          <div>
            <p style={{ margin: 0 }}>
              <strong>Vote recorded.</strong>
            </p>
            <p className="panel-hint" style={{ marginBottom: 0 }}>
              Side and crowd guess remain hidden until resolution.
            </p>
          </div>
        )
      ) : (
        <>
          <p className="panel-hint">
            Your crowd guess is the gate — vote before you can trade.
          </p>
          <div className="side-toggle">
            <button
              type="button"
              className={`btn btn-yes ${voteSide === "yes" ? "active" : ""}`}
              onClick={() => setVoteSide("yes")}
            >
              YES
            </button>
            <button
              type="button"
              className={`btn btn-no ${voteSide === "no" ? "active" : ""}`}
              onClick={() => setVoteSide("no")}
            >
              NO
            </button>
          </div>
          <div className="field">
            <label htmlFor="guess">Crowd guess (% YES)</label>
            <div className="slider-row">
              <input
                id="guess"
                type="range"
                min={0}
                max={100}
                value={guess}
                onChange={(e) => setGuess(Number(e.target.value))}
              />
              <span className="slider-val num">{guess}%</span>
            </div>
          </div>
          {voteError && (
            <p role="alert" style={{ color: "var(--danger)", fontSize: "0.88rem" }}>
              {voteError}
            </p>
          )}
          <button
            type="button"
            className="btn btn-primary btn-block"
            disabled={voteBusy}
            onClick={onVote}
          >
            {voteBusy ? "Submitting…" : "Vote now"}
          </button>
        </>
      )}
    </div>
  );
}
