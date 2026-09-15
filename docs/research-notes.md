# Research notes — external verification

*Date: 2026-08-12. Purpose: verify the spec's assumptions about the competitor and the external dependencies before build start. Verdict: **assumptions hold**; three deltas noted below.*

##  (the incumbent)

What the public record confirms (their site blocks scraping; product observations come from our own e2e walkthrough video, which remains the primary source):

- **Positioning:** "Wager not on what will happen, but what people believe" — an opinion market resolving on **what voters say, not objective reality**. Exactly the mechanism the spec targets.
- **Product state:** live on web (alpha subdomain), no download; **iOS app in development**; early access open with **weekly cash rewards** for voters.
- **Ecosystem:** listed as a Colosseum company ("an opinion markets venue for trading on collective belief") — Colosseum is the Solana accelerator, i.e. the incumbent is on the same chain we chose. US-based.
- **Category validation:** a second entrant ("Opinion") has Messari research coverage; third-party analyses of 's outcome states exist (Dancing Dragons blog, paywalled/bot-blocked). The category is no longer a single-player experiment.

**Deltas for us:**
1. **Mobile window is closing** — their iOS app is announced. Our PWA-at-launch + fast-follow native plan stands, but the "no mobile app" weakness row in the teardown has a timer on it.
2. **Weekly cash rewards for voters** — they are paying voters cash during early access; our D5 (points-only) stays, but marketing must position scoring transparency + rep utility against their cash drip.
3. Their Solana affiliation confirms our rail choice — and means shared infrastructure norms (embedded wallets, USDC) will read as familiar to their users.

## Sendblue (iMessage vendor)

- **Free sandbox confirmed** — full API, no credit card. Matches the build-phase plan (inbound-first, verified tester contacts).
- **Production pricing (2026):** AI Agent plan ~$100/line/month, unlimited messages, but **the contact must message first** (reply-only). Outbound-first requires Enterprise, reported at $1,000+/line/month.
- **Fit:** our design already assumed inbound-first (testers text in; proactive alerts ride APNs later). No design change needed; budget line updated.

## Legal landscape (inputs for counsel, not conclusions)

- CFTC-regulated event prediction markets are mainstream in 2026 — Kalshi operates nationwide; Nevada and Minnesota enacted bans that are under federal court challenge; state pushback continues on sports event contracts.
- **Vote-resolved opinion wagering is not an event contract** — no CFTC umbrella applies. The spec's "hardest possible posture" framing is confirmed.
- The **sweepstakes dual-currency model** (free-play currency + redeemable sweeps currency; ProphetX pattern) reaches 39+ states without gambling licensure and is the main structural alternative counsel must evaluate. This decision gates the payments workstream (onramps, KYC placement, marketing language).
-  pays weekly **cash** rewards to early-access voters — whatever structure permits that is worth counsel investigating.

## Sources

- Market research sources (various, 2026-08-12)
- [Colosseum company profile](https://colosseum.com/companies/fact-machine)
- [Messari — "Opinion: An Emerging Player in Prediction Markets"](https://messari.io/report/opinion-an-emerging-player-in-prediction-markets)
- [Third-party analysis] (bot-blocked)
- [Sendblue pricing](https://www.sendblue.com/pricing) · [Sendblue API](https://www.sendblue.com/api) · [Tuco AI pricing breakdown](https://tuco.ai/blog/sendblue-pricing-2026-complete-breakdown) · [Sendara pricing breakdown](https://www.sendara.io/blog/sendblue-pricing-2026)
- [BettingUSA — prediction markets legal overview](https://www.bettingusa.com/prediction-markets/) · [Casino.org prediction apps](https://www.casino.org/us/predictions/) · [VegasInsider prediction markets](https://www.vegasinsider.com/prediction-markets/best-prediction-market-apps/)
