/*
 * Mode accent switcher — Solana (purple) vs Perpetuals (teal), D13.
 *
 * The mode flips the accent-family tokens (+ glow/bracket tints) via a
 * `.mode-sol` / `.mode-perps` class on <html> (see app.css). Surfaces,
 * ink and the semantic --up/--down colors are mode-independent.
 * Persists across restarts via localStorage.
 */

export type Mode = "sol" | "perps";

const MODE_KEY = "degenbox.signer.mode";
const CLASSES: Record<Mode, string> = { sol: "mode-sol", perps: "mode-perps" };

export function getMode(): Mode {
  try {
    const m = localStorage.getItem(MODE_KEY);
    return m === "perps" ? "perps" : "sol";
  } catch {
    return "sol";
  }
}

export function setMode(mode: Mode): void {
  try {
    localStorage.setItem(MODE_KEY, mode);
  } catch {
    // storage unavailable — class still applies for this session
  }
  const el = document.documentElement;
  el.classList.remove(CLASSES.sol, CLASSES.perps);
  el.classList.add(CLASSES[mode]);
}

/** Apply the persisted mode on boot (index.html defaults to mode-sol). */
export function initMode(): void {
  setMode(getMode());
}
