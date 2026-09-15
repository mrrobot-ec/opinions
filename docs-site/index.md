# What this project is

Welcome. This guide explains a piece of software called **Opinions** to someone who has
never written a line of code. No prior knowledge is assumed. Where a technical word is
unavoidable, it is explained the first time it appears, and it is also in the
[glossary](start-here/glossary.md).

Everything in this guide was checked against the source code of the project itself. Where
a page mentions a file, that file exists; where it quotes a number, that number is in the
code. Where something is *designed but not yet connected to the real world*, the guide
says so plainly rather than letting you assume otherwise.

## The one-sentence version

Opinions is a website (and an iMessage bot) where people **bet real money on what other
people believe** — not on what actually happens in the world.

## Why that is unusual

Most betting is about facts. "Will it rain tomorrow?" has a real answer, and after
tomorrow everyone can check.

Opinions asks a different kind of question:

> *"What percentage of people will say pineapple belongs on pizza?"*

Nobody can look this up. The answer is created by the crowd itself: when the market
closes, we count how everyone voted, and **that vote count is the answer**. If 63% of
voters said yes, then "yes" is worth 63 cents and "no" is worth 37 cents.

So players are not predicting the world. They are predicting **each other**.

## The three things you can do

| Action | What it means | What it costs |
|---|---|---|
| **Vote** | Say what *you* believe, once per market, and guess what the crowd will say. | Nothing |
| **Buy** | Buy shares in "yes" or "no" because you think the final percentage will land your way. | Real money, plus a fee |
| **Sell** | Sell shares back before the market closes, at whatever the current price is. | A fee on the proceeds |

You must vote before you can trade. That rule exists so the crowd's opinion is recorded
honestly, before anyone has money riding on it. Selling matters too: you are never locked
in until the market closes, and the price you can sell at moves with demand exactly like
the price you buy at.

## A worked example

Imagine a market: *"What percent of people think cereal is a soup?"*

1. You vote **no** — you think it isn't.
2. But you also think *lots of other people* will vote yes, just to be funny. You guess
   the final number will be around 40%.
3. The market currently prices "yes" at 23 cents. That is cheaper than your guess of 40.
   So you **buy yes shares**, even though you personally voted no.
4. You spend $5. A 1% fee takes 5 cents; the remaining $4.95 buys you **21.1726 yes
   shares** at an average all-in price of about **23.6 cents** each.
5. The market closes. 41% voted yes.
6. Every yes share is now worth exactly **41 cents**. Your 21.1726 shares pay
   **$8.68**. You put in $5.

Notice what happened: you made money by reading the room correctly, not by being right
about cereal. That is the entire game.

Those are not illustrative numbers. They are what the pricing code in
`crates/domain/src/amm.rs` actually returns for that trade, and
[Prices and fees](money/fees-and-pricing.md) shows the arithmetic step by step.

## Why the software is complicated

Because real money is involved, the software has to be far more careful than a normal
website. Three promises have to hold **every second, without exception**:

1. **Money is never created or destroyed by accident.** Every cent that enters must be
   somewhere: in someone's balance, in a market's pot, in fees, in the house's funds, or
   in one of the holding accounts.
2. **Nothing happens twice.** If your phone sends the same "buy" twice because the
   network hiccupped, you buy once.
3. **Nothing is half-done.** If the system crashes mid-payout, it either completes when
   it restarts or it never happened. There is no in-between.

Almost every design decision in this project exists to protect one of those three
promises. The rest of this guide shows how.

## What is real, and what is not

This is an honest inventory, because software that looks more finished than it is will
eventually embarrass somebody.

| Built and working | Not built |
|---|---|
| The whole market: voting, pricing, buying, selling, settlement, payouts | Real identity, sanctions and phone vendors — only sandbox stand-ins exist |
| The money ledger, its invariant checks, and the withdrawal state machine | Real-money (mainnet) blockchain rails — the code targets test networks |
| Compliance rules, dual-control approvals, the admin control plane | A card-based deposit "onramp" — deposits are observed on-chain |
| The website, the iMessage service, and the 2,000-agent test swarm | The legal opinions that would let any of this operate commercially |

The project's own decision log records those last items as **open gates** that block a
launch, not development. See [How this was built](running/how-this-was-built.md).

## How to read this guide

- **Start here** — the product, in plain English. No code.
- **How it is built** — the shape of the software, still mostly plain English.
- **Following the money** — the accounting rules that keep the money honest.
- **Watch it happen** — step-by-step traces of a real trade, vote, and payout.
- **Running it yourself** — how to start it on your own computer, and how it is tested.

You can read straight through, or jump to whatever you are curious about.
