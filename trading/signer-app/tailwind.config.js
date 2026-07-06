/** @type {import('tailwindcss').Config} */
//
// Modeled on frontend/app/tailwind.config.js so @degenbox/ui renders
// in the desktop client. W0.2 owns the token VALUES (src/styles/app.css,
// blackalgo-inspired dark theme): `--accent` here is a resolved rgb()
// alias and the triplet lives in `--accent-rgb`; --line-strong /
// --accent-soft / --highlight are real tokens since W0.2. New theme
// tokens (edge hairline, accent-2 gradient stop, bracket tint, glow)
// are registered below. Radii are clamped to the square-corner language
// (0–4px) — this intentionally reskins @degenbox/ui's rounded-* usage.
export default {
  darkMode: "class",
  content: [
    "./index.html",
    "./src/**/*.{ts,tsx}",
    // @degenbox/ui is consumed as source — scan it so its utilities exist.
    "../../frontend/packages/ui/src/**/*.{ts,tsx}",
  ],
  theme: {
    extend: {
      colors: {
        canvas: "rgb(var(--canvas) / <alpha-value>)",
        card: "rgb(var(--card) / <alpha-value>)",
        cardHover: "rgb(var(--card-hover) / <alpha-value>)",
        ink: {
          1: "rgb(var(--ink-1) / <alpha-value>)",
          2: "rgb(var(--ink-2) / <alpha-value>)",
          3: "rgb(var(--ink-3) / <alpha-value>)",
          4: "rgb(var(--ink-4) / <alpha-value>)",
        },
        line: "rgb(var(--line) / <alpha-value>)",
        lineStrong: "rgb(var(--line-strong) / <alpha-value>)",
        edge: "rgb(var(--edge) / <alpha-value>)",
        accent: "rgb(var(--accent-rgb) / <alpha-value>)",
        accent2: "rgb(var(--accent-2-rgb) / <alpha-value>)",
        accentSoft: "rgb(var(--accent-soft) / <alpha-value>)",
        accentInk: "var(--accent-ink)",
        bracket: "rgb(var(--bracket-rgb) / <alpha-value>)",
        up: "rgb(var(--up) / <alpha-value>)",
        down: "rgb(var(--down) / <alpha-value>)",
      },
      fontFamily: {
        sans: ['"Hanken Grotesk"', '"DM Sans"', "Inter", "system-ui", "sans-serif"],
        mono: ['"JetBrains Mono"', '"Geist Mono"', "ui-monospace", "monospace"],
      },
      letterSpacing: {
        tightest: "-0.025em",
        widest: "0.16em",
      },
      // Square-corner language: 0–4px max everywhere.
      borderRadius: {
        DEFAULT: "2px",
        sm: "1px",
        md: "2px",
        lg: "4px",
        xl: "4px",
      },
      boxShadow: {
        card: "var(--shadow-card)",
        cardHover: "var(--shadow-card-hover)",
        popover: "var(--shadow-popover)",
        accent: "var(--shadow-accent)",
        glow: "var(--glow)",
      },
      transitionTimingFunction: {
        snap: "cubic-bezier(0.32, 0.72, 0, 1)",
      },
      keyframes: {
        highlight: {
          "0%": { backgroundColor: "rgb(var(--highlight) / 1)" },
          "100%": { backgroundColor: "rgb(var(--highlight) / 0)" },
        },
      },
      animation: {
        highlight: "highlight 1.6s cubic-bezier(0.32, 0.72, 0, 1)",
      },
    },
  },
  plugins: [],
};
