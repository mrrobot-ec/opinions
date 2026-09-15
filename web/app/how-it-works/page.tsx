import Link from "next/link";

/**
 * Renders the published rules from docs/copy/scoring.md (source of truth).
 * Keep numbers in sync with that file when defaults change.
 */
export default function HowItWorksPage() {
  return (
    <article className="prose-page scoring-copy">
      <h1 className="page-title">How scoring works</h1>
      <p className="page-sub">
        Opinions is a vote-gated prediction market. You cast a crowd guess first;
        trading unlocks after that vote. Formulas below are launch defaults —
        numbers may be reconfigured, the math does not hide.
      </p>

      <section className="panel" id="scoring">
        <h3>Vote scoring (per market)</h3>
        <p className="panel-hint">
          After a market resolves to actual YES share <strong>a</strong>{" "}
          (0–100%):
        </p>
        <pre className="formula-block">{`accuracy = max(0, 1 − |crowd_guess − a| / 25)
majority = 1 if your side matches the winner else 0
score    = 75·accuracy + 25·majority          // 0–100`}</pre>
        <ul className="copy-list">
          <li>
            <strong>Accuracy (75%)</strong> — linear kernel; zero if you are 25+
            points off the outcome.
          </li>
          <li>
            <strong>Majority (25%)</strong> — on the winning side.
          </li>
          <li>
            <strong>Tie rule:</strong> at exactly <strong>50.00% YES</strong>,{" "}
            <em>both</em> YES and NO count as winners for majority.
          </li>
          <li>
            <strong>Voided markets</strong> produce no scores (no free majority
            rep).
          </li>
        </ul>
      </section>

      <section className="panel" id="reputation">
        <h3>Reputation (EWMA)</h3>
        <p className="panel-hint">
          <code>rep = EWMA of per-market scores</code> with half-life{" "}
          <strong>20 markets</strong> (K_PPM = 965,936). Range 0–1,000,000 micro.
          New accounts start at tier 0.
        </p>
        <p className="panel-hint">
          <strong>Quality floor:</strong> rep updates only when the market has
          enough votes (<code>min_votes_to_resolve</code>) <em>and</em> escrow ≥{" "}
          <strong>$50</strong> pot floor. Scores are still recorded for
          transparency; thin pots cannot mint tier climb.
        </p>
        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>Tier</th>
                <th>Min rep</th>
                <th>Position cap / market</th>
                <th>Effective fee (base 1%)</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td>0</td>
                <td>0</td>
                <td>$25</td>
                <td>1.00%</td>
              </tr>
              <tr>
                <td>1</td>
                <td>200,000</td>
                <td>$50</td>
                <td>1.00%</td>
              </tr>
              <tr>
                <td>2</td>
                <td>400,000</td>
                <td>$100</td>
                <td>
                  <strong>0.90%</strong>
                </td>
              </tr>
              <tr>
                <td>3</td>
                <td>600,000</td>
                <td>$250</td>
                <td>0.80%</td>
              </tr>
              <tr>
                <td>4</td>
                <td>800,000</td>
                <td>$500</td>
                <td>0.70%</td>
              </tr>
            </tbody>
          </table>
        </div>
        <p className="panel-hint">
          Voter rewards at launch are <strong>points / reputation only</strong> —
          no cash for voting.
        </p>
      </section>

      <section className="panel" id="anti-churn">
        <h3>Anti-churn fee rule</h3>
        <p className="panel-hint" style={{ margin: 0 }}>
          If you <strong>sell</strong> shares of an outcome you increased within
          the flip window (default <strong>1 hour</strong>), that sell pays the{" "}
          <strong>base fee</strong> — the tier discount does not apply on that
          sell leg. Buy legs may still be discounted. This taxes short flip
          sells; it does not ban trading.
        </p>
      </section>

      <section className="panel" id="integrity">
        <h3>Integrity bar (who may vote)</h3>
        <ol className="copy-list">
          <li>
            <strong>Phone-linked channel required</strong> — voting account must
            have a linked identity channel (e.g. iMessage).
          </li>
          <li>
            <strong>Velocity cap</strong> — at most <strong>30</strong> votes per
            rolling <strong>3,600 s</strong> per user.
          </li>
          <li>
            <strong>Young-account friction near close</strong> — accounts younger
            than <strong>72 hours</strong> cannot vote in the last{" "}
            <strong>10 minutes</strong> before close.
          </li>
        </ol>
        <p className="panel-hint">
          During the hidden-tally window, <strong>all trading freezes</strong>{" "}
          (both sides). Voting may continue until close.
        </p>
      </section>

      <section className="panel" id="review-hold">
        <h3>Review hold (large pots)</h3>
        <p className="panel-hint" style={{ margin: 0 }}>
          Escrow at or above <strong>$500</strong> enters{" "}
          <strong>Resolving</strong> for automated fairness review (usually a few
          minutes). Checks are <em>heuristics, not a guilt finding</em>. A flag
          needs <strong>two or more</strong> independent signals; flagged pots
          never auto-settle — a curator chooses resolve-at-tally or void (50¢
          neutral). Smaller pots settle on the fast path.
        </p>
      </section>

      <section className="panel" id="leaderboards">
        <h3>Leaderboards</h3>
        <p className="panel-hint" style={{ margin: 0 }}>
          <strong>Top voters</strong> rank by average score (min markets + tier
          filter) — volume alone cannot buy the board.{" "}
          <strong>Top traders</strong> rank by <strong>settled PnL</strong> in
          window, not mark-to-market. Tier badges appear on profiles and
          leaderboards — <strong>not</strong> on the public tape.
        </p>
      </section>

      <p style={{ marginTop: "1.5rem" }}>
        <Link href="/" className="btn btn-primary">
          Browse markets
        </Link>
      </p>
    </article>
  );
}
