import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  timeout: Number(process.env.PLAYWRIGHT_TEST_TIMEOUT_MS ?? 360_000),
  expect: {
    timeout: 15_000,
  },
  use: {
    baseURL: process.env.PLAYWRIGHT_BASE_URL ?? "http://127.0.0.1:3000",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "mobile-golden",
      use: { ...devices["iPhone 13"] },
    },
  ],
});
