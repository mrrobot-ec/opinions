"use client";

import { useEffect } from "react";

export default function ServiceWorkerRegister() {
  useEffect(() => {
    if (typeof window === "undefined" || !("serviceWorker" in navigator)) return;
    // Only register in production-ish builds / when not blocked by localhost policies.
    navigator.serviceWorker.register("/sw.js").catch(() => {
      /* SW optional for dev; installability still works with manifest */
    });
  }, []);
  return null;
}
