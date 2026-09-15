import { expect, test } from "@playwright/test";

const userId = process.env.PLAYWRIGHT_USER_ID;
const demoToken = process.env.PLAYWRIGHT_DEMO_TOKEN;

test.skip(
  !userId || !demoToken || process.env.PLAYWRIGHT_MONEY !== "1",
  "requires the Phase 7 money Playwright arm",
);

test("withdraw, KYC prompt, credit balance, and self-exclusion surfaces", async ({
  page,
}) => {
  await page.addInitScript(
    ({ id, token }) => {
      localStorage.setItem("opinions_user_id", id);
      localStorage.setItem("opinions_demo_token", token);
    },
    { id: userId!, token: demoToken! },
  );

  await page.goto("/withdraw");
  await expect(page.getByRole("heading", { name: "Withdraw" })).toBeVisible();
  await expect(page.getByTestId("credit-balance")).toBeVisible();
  await expect(page.getByTestId("kyc-prompt")).toBeVisible();
  await expect(page.getByTestId("self-exclusion")).toBeVisible();
  await expect(page.getByTestId("withdraw-form")).toBeVisible();
  await page.getByLabel("Amount (USD)").fill("5.00");
  await page.getByLabel("Destination").fill("Dest1111111111111111111111111111111111111111");
  await page.getByRole("button", { name: "Request withdrawal" }).click();
  // The rail may accept or refuse; the surface must report EITHER outcome and
  // must never leave the request silently pending.
  const result = page.getByTestId("withdraw-result");
  await expect(result).toBeVisible({ timeout: 15_000 });
  await expect(result).not.toBeEmpty();

  await page.goto("/portfolio");
  await expect(page.getByTestId("credit-balance")).toBeVisible();
});
