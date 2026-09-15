# Design references — layout study (2026-08-12)

Source: industry references and design patterns study — `01-home.png`, `02-market-detail.png`. **These are structural/interaction references for best-in-class design. Our product keeps its own brand identity (palette, type, name, copy) — apply proven layout patterns and information hierarchy to deliver an excellent user experience.**

## Global frame
- Slim top bar: brand link (left) · "How it works" · theme toggle · Log In / Sign Up (right). Floating "Tell us what you think" feedback button.
- One adaptive page: the **hero market** dominates above the fold; rails and grids below. Market navigation swaps the hero rather than navigating to a bare subpage.

## Hero market block (the core loop, all visible at once)
- Market video/media panel + question headline.
- **Vote panel** (`Vote` heading + "Vote Now" CTA) — voting is the entry point, visually first.
- **Trade panel** (`Trade` heading): Buy/Sell **tabs**, amount input with **quick-add chips `+$1 +$10 +$20 Max`**, big side buttons ("Buy Yes"), "What is trading?" expandable explainer for newcomers, logged-out state shows "Log In" in place of execute.
- Their gate: trading UI visible but gated behind auth/vote (our vote→trade blur mechanic matches).

## Rails and social proof
- **Live & Upcoming** rail directly under the hero: current + "Coming Soon" placeholder cards (cadence made visible — even empty slots are designed).
- **Weekly Leaderboard**: two columns — **Top Voters** (accuracy earnings, e.g. `#1 <handle> $315.03`) and **Top Traders** (PnL, `#1 <handle> $17,200.39`). Rank + avatar + handle + dollar figure per row; rows are links (profiles).
- **Browse Markets** grid: each card = question headline + **% split bar with side labels** (e.g. `31% Yes | No 69%`) + meta line `pool $ | votes N | age` (e.g. `$127.8K | Votes 153 | 20 hrs ago`). "View All Markets" button under the grid.

## Patterns to carry into `web/` (with our identity)
1. Hero-market-first layout; vote panel before trade panel; explainer affordances inline.
2. Quick-add chips exactly ($1/$10/$20/Max is muscle memory in this category).
3. Browse cards: % split bar + pool/votes/age meta — the three numbers that sell a market.
4. Leaderboards split voters vs traders — it advertises both economies; ours lands with rep (Phase 3) but the layout slot should exist.
5. Coming-soon slots in the rail — cadence as a visible promise (pairs with our hourly flash markets).
6. Theme toggle in the top bar (we are dark-first; light is the toggle).

## Deltas we keep (our superiority points, not in their UI)
- Live WS price updates + frozen-window state messaging (they have neither surfaced).
- Published scoring ("How scoring works") link next to the vote panel.
- Countdown to close + hidden-tally warning from server time.
- Post-resolve payout screens (their resolution surface is notification-only).
