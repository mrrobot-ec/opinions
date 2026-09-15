"use client";

import { formatDollars } from "@/lib/format";
import type { MoneyBalances } from "@/lib/money";

export default function CreditBalance({ balances }: { balances: MoneyBalances | null }) {
  if (!balances) {
    return (
      <div className="price-tile" data-testid="credit-balance">
        <div className="label">Bonus credits</div>
        <div className="value num" style={{ fontSize: "1.1rem" }}>
          —
        </div>
        <p className="page-sub" style={{ marginTop: 8 }}>
          Credits convert after a market is Paid. They are not withdrawable.
        </p>
      </div>
    );
  }
  return (
    <div className="price-tile" data-testid="credit-balance">
      <div className="label">Bonus credits</div>
      <div className="value num" style={{ fontSize: "1.35rem" }}>
        {formatDollars(balances.credit_micro)}
      </div>
      <p className="page-sub" style={{ marginTop: 8 }}>
        Cash {formatDollars(balances.cash_micro)}
        {balances.withheld_micro > 0
          ? ` · withheld ${formatDollars(balances.withheld_micro)}`
          : ""}
      </p>
    </div>
  );
}
