/**
 * Client-asserted device id for vote integrity capture (Phase 3).
 * Stored in localStorage — deliberately weak / spoofable; not a hardware fingerprint.
 * Server HMACs this before persistence (DEVICE_HASH_SECRET).
 */

const DEVICE_KEY = "opinions_device_id";

/** Returns a stable per-browser id; creates one on first use. */
export function getOrCreateDeviceId(): string {
  if (typeof window === "undefined" || typeof localStorage === "undefined") {
    return "ssr-no-device";
  }
  try {
    const existing = localStorage.getItem(DEVICE_KEY);
    if (existing && existing.length > 0 && existing.length <= 64) {
      return existing;
    }
    const id =
      typeof crypto !== "undefined" && crypto.randomUUID
        ? crypto.randomUUID().replace(/-/g, "").slice(0, 32)
        : `dev${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
    const clipped = id.slice(0, 64);
    localStorage.setItem(DEVICE_KEY, clipped);
    return clipped;
  } catch {
    return "device-unavailable";
  }
}
