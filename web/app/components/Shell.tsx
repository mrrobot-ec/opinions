"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import DevLogin from "./DevLogin";
import NotificationBell from "./NotificationBell";
import ProfileChip from "./ProfileChip";
import ServiceWorkerRegister from "./ServiceWorkerRegister";
import ThemeToggle from "./ThemeToggle";

export default function Shell({ children }: { children: React.ReactNode }) {
  const path = usePathname();
  return (
    <div className="app-shell">
      <ServiceWorkerRegister />
      <div className="dev-banner" role="status">
        <span>DEV</span>
        <span>· demo token may live in localStorage · not production auth</span>
      </div>
      <header className="topnav">
        <div className="topnav-left">
          <Link href="/" className="brand">
            <span className="brand-mark" aria-hidden />
            <span>Opinions</span>
          </Link>
          <Link
            href="/how-it-works"
            className={`nav-quiet ${path === "/how-it-works" ? "active" : ""}`}
          >
            How it works
          </Link>
          <Link
            href="/admin"
            className={`nav-quiet ${path.startsWith("/admin") ? "active" : ""}`}
          >
            Admin
          </Link>
        </div>
        <nav className="nav-links" aria-label="Primary">
          <NotificationBell />
          <ProfileChip />
          <Link
            href="/portfolio"
            className={path === "/portfolio" ? "active" : undefined}
          >
            Portfolio
          </Link>
          <Link
            href="/withdraw"
            className={path === "/withdraw" ? "active" : undefined}
          >
            Withdraw
          </Link>
          <ThemeToggle />
          <DevLogin />
        </nav>
      </header>
      <main className="main">{children}</main>
      <footer className="site-footer">
        <span>Opinions</span>
        <span className="footer-sep">·</span>
        <span>Vote first. Trade second. Scores published.</span>
      </footer>
    </div>
  );
}
