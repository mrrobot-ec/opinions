"use client";

import { useEffect, useState } from "react";

type Theme = "dark" | "light";

const KEY = "opinions_theme";

function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
}

export default function ThemeToggle() {
  const [theme, setTheme] = useState<Theme>("dark");

  useEffect(() => {
    const stored = localStorage.getItem(KEY) as Theme | null;
    const initial: Theme =
      stored === "light" || stored === "dark" ? stored : "dark";
    setTheme(initial);
    applyTheme(initial);
  }, []);

  function toggle() {
    const next: Theme = theme === "dark" ? "light" : "dark";
    setTheme(next);
    localStorage.setItem(KEY, next);
    applyTheme(next);
  }

  return (
    <button
      type="button"
      className="icon-btn"
      onClick={toggle}
      aria-label={theme === "dark" ? "Switch to light theme" : "Switch to dark theme"}
      title={theme === "dark" ? "Light" : "Dark"}
    >
      {theme === "dark" ? (
        <span aria-hidden>☀</span>
      ) : (
        <span aria-hidden>☾</span>
      )}
    </button>
  );
}
