import { expect, test } from "@playwright/test";

function requiredEnv(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required for the live mobile Playwright contract`);
  }
  return value;
}

const marketSlug = requiredEnv("PLAYWRIGHT_MARKET_SLUG");
const userId = requiredEnv("PLAYWRIGHT_USER_ID");
const demoToken = requiredEnv("PLAYWRIGHT_DEMO_TOKEN");
const pinnedWindowTimeout = Number(
  process.env.PLAYWRIGHT_PINNED_WINDOW_TIMEOUT_MS ?? 300_000,
);

test("mobile vote + crowd guess -> trade -> payout -> share-card img", async ({
  page,
}) => {
  await page.addInitScript(
    ({ id, token }) => {
      localStorage.setItem("opinions_user_id", id);
      localStorage.setItem("opinions_demo_token", token);
    },
    { id: userId, token: demoToken },
  );

  await page.goto(`/m/${encodeURIComponent(marketSlug)}`);
  await expect(page.getByRole("heading", { level: 1 })).toBeVisible();

  await page.getByRole("button", { name: "YES", exact: true }).click();
  await page.locator("#guess").evaluate((element) => {
    const input = element as HTMLInputElement;
    const setValue = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )?.set;
    setValue?.call(input, "63");
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await expect(page.getByText("63%", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Vote now" }).click();
  await expect(page.getByText(/Recorded YES/)).toBeVisible();
  await expect(page.getByText(/guess 63%/)).toBeVisible();

  await page.getByRole("button", { name: "Preview ticket" }).click();
  await expect(page.getByText("Max payout")).toBeVisible();
  await page.getByRole("button", { name: "Confirm trade" }).click();
  await expect(page.getByText(/Filled .* shares/)).toBeVisible();

  // The Phase 6 fixture pins the close/hidden offsets; wait for the public
  // lifecycle projection rather than using a synthetic clock or admin route.
  await expect(page.getByText("Settled P&L")).toBeVisible({
    timeout: pinnedWindowTimeout,
  });
  await page.getByRole("button", { name: "Share card" }).click();
  const image = page.getByRole("img", { name: "Share card" });
  await expect(image).toBeVisible();
  await expect(image).toHaveAttribute(
    "src",
    new RegExp(`/users/${userId}/share_card/`),
  );
});
