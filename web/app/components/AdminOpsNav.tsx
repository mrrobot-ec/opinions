"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

const ITEMS = [
  ["/admin", "Curation"],
  ["/admin/config", "Config"],
  ["/admin/switches", "Switches"],
  ["/admin/audit", "Audit"],
] as const;

export default function AdminOpsNav() {
  const path = usePathname();
  return (
    <nav className="admin-ops-nav" aria-label="Admin control plane">
      {ITEMS.map(([href, label]) => (
        <Link
          key={href}
          href={href}
          className={path === href ? "active" : undefined}
        >
          {label}
        </Link>
      ))}
    </nav>
  );
}
