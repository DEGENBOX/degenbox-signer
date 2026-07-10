// Tiny localStorage-backed operator preferences (device-local, not
// synced). Mirrors the get/set pattern already used ad-hoc in
// App.tsx / SolPositions.tsx, centralised here for the close-confirm
// preference which governs BOTH venues' position-close flows.

const SKIP_CLOSE_CONFIRM_KEY = "dbx.skipCloseConfirm";

/** When true, position closes skip the type-to-confirm dialog and fire
 *  a 100% reduce-only close directly (Perps + Solana quick-close). */
export function getSkipCloseConfirm(): boolean {
  try {
    return localStorage.getItem(SKIP_CLOSE_CONFIRM_KEY) === "true";
  } catch {
    // storage unavailable — default to the safe (confirm) path
    return false;
  }
}

export function setSkipCloseConfirm(skip: boolean): void {
  try {
    localStorage.setItem(SKIP_CLOSE_CONFIRM_KEY, skip ? "true" : "false");
  } catch {
    // session-only; nothing else to do
  }
}
