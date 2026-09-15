import { afterEach, describe, expect, it, vi } from "vitest";

describe("getOrCreateDeviceId", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.resetModules();
  });

  it("creates and reuses a localStorage id", async () => {
    const store = new Map<string, string>();
    vi.stubGlobal("window", {
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => {
          store.set(k, v);
        },
        removeItem: (k: string) => {
          store.delete(k);
        },
      },
    });
    vi.stubGlobal("localStorage", {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => {
        store.set(k, v);
      },
      removeItem: (k: string) => {
        store.delete(k);
      },
    });
    vi.stubGlobal("crypto", {
      randomUUID: () => "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    });

    const { getOrCreateDeviceId } = await import("../device");
    const a = getOrCreateDeviceId();
    const b = getOrCreateDeviceId();
    expect(a).toBe(b);
    expect(a.length).toBeGreaterThan(8);
    expect(a.length).toBeLessThanOrEqual(64);
  });
});
