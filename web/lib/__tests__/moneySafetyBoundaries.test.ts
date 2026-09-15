import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const root = join(__dirname, "../..");

describe("money surface safety boundaries", () => {
  it("requires the user to re-enter the withdrawal destination", () => {
    const source = readFileSync(join(root, "app/withdraw/page.tsx"), "utf8");
    expect(source).toContain('htmlFor="withdraw-confirm-dest"');
    expect(source).toContain('id="withdraw-confirm-dest"');
  });

  it("uses exact amount parsing and refusal-aware receipt copy", () => {
    const source = readFileSync(join(root, "app/withdraw/page.tsx"), "utf8");
    expect(source).not.toContain("Math.round(Number(amount) * 1_000_000)");
    expect(source).toContain("parseUsdMicros(amount)");
    expect(source).toContain("withdrawReceiptCopy(receipt)");
  });

  it("keeps same-origin core API reads out of the service-worker cache", () => {
    const source = readFileSync(join(root, "public/sw.js"), "utf8");
    expect(source).toContain('const CACHE = "opinions-shell-v2"');
    expect(source).toContain('url.pathname.startsWith("/core-api")');
  });

  it("labels the sell input in shares rather than dollars", () => {
    const source = readFileSync(
      join(root, "app/components/TradePanel.tsx"),
      "utf8",
    );
    expect(source).toContain(
      'action === "buy" ? "Amount (USD)" : "Shares to sell"',
    );
  });

  it("does not label realized PnL as a payout", () => {
    const source = readFileSync(join(root, "app/m/[slug]/page.tsx"), "utf8");
    expect(source).toContain("Settled P&L");
    expect(source).not.toContain("Your payout");
  });
});
