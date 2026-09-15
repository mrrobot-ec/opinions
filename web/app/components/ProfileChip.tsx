"use client";

import { useEffect, useState } from "react";
import { getUserId, userPositions, userProfile } from "@/lib/api";
import { formatTierFeeLine } from "@/lib/repDisplay";

/**
 * Profile chip: tier badge + effective fee line from profile/positions DTO.
 * Graceful when rep fields absent (Rust chain in parallel).
 */
export default function ProfileChip() {
  const [line, setLine] = useState<string | null>(null);

  useEffect(() => {
    const sync = () => {
      void load();
    };
    async function load() {
      const uid = getUserId();
      if (!uid) {
        setLine(null);
        return;
      }
      try {
        const profile = await userProfile(uid);
        if (profile && typeof profile.tier === "number") {
          setLine(formatTierFeeLine(profile.tier));
          return;
        }
      } catch {
        /* optional */
      }
      try {
        const pos = await userPositions(uid);
        const withTier = pos.find((p) => typeof p.tier === "number");
        if (withTier && typeof withTier.tier === "number") {
          setLine(formatTierFeeLine(withTier.tier));
          return;
        }
        // Logged in but no rep yet — show tier 0 defaults from scoring.md
        setLine(formatTierFeeLine(0));
      } catch {
        setLine(formatTierFeeLine(0));
      }
    }
    void load();
    window.addEventListener("opinions-auth", sync);
    window.addEventListener("focus", sync);
    return () => {
      window.removeEventListener("opinions-auth", sync);
      window.removeEventListener("focus", sync);
    };
  }, []);

  if (!line) return null;
  return (
    <span className="profile-chip" title="Reputation tier and your effective trade fee">
      {line}
    </span>
  );
}
