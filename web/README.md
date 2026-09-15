# Opinions Web PWA

Next.js 15 + TypeScript shell for the Opinions core (REST + WebSocket).

## Stack

- **pnpm** `9.15.4` (via corepack)
- **Next.js** 15 (App Router), React 19
- **vitest** 3.0.5 for formatting / WS dedupe / countdown unit tests
- Installable PWA: `public/manifest.json`, `public/sw.js`, icons 192/512

## Setup

```bash
corepack use pnpm@9.15.4
cd web
pnpm install
cp .env.example .env.local   # edit URLs if needed
pnpm dev
```

Core must be running (default `http://127.0.0.1:8080`, WS `ws://127.0.0.1:8080/ws`).

## Scripts

| Script        | Purpose                          |
|---------------|----------------------------------|
| `pnpm dev`    | Dev server (turbopack)           |
| `pnpm build`  | Production build                 |
| `pnpm start`  | Serve production build           |
| `pnpm test`   | Vitest unit tests                |

## Pages

- `/` — markets list (live cards, frozen-window badge, countdown)
- `/m/[slug]` — market detail: live WS prices, vote-first gate, trade preview→confirm, tape, chart (hidden on 404), resolved/voided terminal screens
- `/portfolio` — positions + realized PnL (dev login)

## Dev auth

The **Dev login** control stores user UUID + demo bearer token in `localStorage`. A persistent **DEV** banner labels this local-demo exposure. Not production auth.

## Graceful degradation

`GET /markets/{id}/chart` and `GET /markets/{id}/tape` are plan Task 2.3 contracts. When the running core returns 404, the UI hides those panels so `pnpm build` / tests stay green.

## Design

Dark-first palette (`#070a0e`), YES mint `#3dd6a5` / NO rose `#ff6b8a`, IBM Plex Sans + Mono, `tabular-nums` on prices, designed loading/empty/error/frozen/resolved states, subtle price-change pulse.
