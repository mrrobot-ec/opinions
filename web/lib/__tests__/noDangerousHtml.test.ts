import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

/**
 * Plan Task 4.4: ban the React raw-HTML prop on product sources.
 * Pattern is split so this test file itself is not a hit.
 */
const FORBIDDEN = "dangerouslySet" + "InnerHTML";

function walk(dir: string, acc: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    if (name === "node_modules" || name === ".next" || name === "__tests__") {
      continue;
    }
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) walk(p, acc);
    else if (/\.(tsx?|jsx?)$/.test(name)) acc.push(p);
  }
  return acc;
}

describe("no raw HTML injection prop", () => {
  it("web app/lib sources never use the forbidden React HTML prop", () => {
    const root = join(__dirname, "../..");
    const files = walk(join(root, "app")).concat(walk(join(root, "lib")));
    expect(files.length).toBeGreaterThan(10);
    const hits: string[] = [];
    for (const f of files) {
      const text = readFileSync(f, "utf8");
      if (text.includes(FORBIDDEN)) {
        hits.push(f);
      }
    }
    expect(hits).toEqual([]);
  });
});
