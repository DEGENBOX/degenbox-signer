// Top-right account avatar + full-screen Account overlay.
//
// Slice-2 IA: Account left the module tabs. Identity (Discord session),
// vault lock, signer pairing, 2FA and app maintenance are device-wide,
// not module-specific, so they live behind the avatar — reachable from
// either module. Opens as a full-screen overlay (NOT a slideover — the
// operator's standing rule) hosting the existing AccountTab sections.
//
// Vault/key/pairing UI stays cleanly componentised here (AccountTab →
// features/account/**) so the future remote shell can hide the
// key-material surfaces without disturbing the trading tabs.

import { useEffect, useState } from "react";
import { ArrowLeft, UserRound, X } from "lucide-react";
import { ipc, discordAvatarUrl, type DiscordStatus, type StatusReport } from "../ipc";
import { AccountTab } from "../pages/AccountTab";

export function AccountMenu({
  status,
  onReload,
}: {
  status: StatusReport | null;
  onReload: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [discord, setDiscord] = useState<DiscordStatus | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () =>
      ipc.discordStatus().then(
        (d) => alive && setDiscord(d),
        () => {},
      );
    load();
    const id = setInterval(load, 5000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // Esc closes the overlay.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  const avatarUrl = discord ? discordAvatarUrl(discord) : null;
  const linked = discord?.linked ?? false;
  const expired = discord?.expired ?? false;
  const vaultLocked = !!status && !status.hl_unlocked && !status.sol_unlocked;
  const sessionTone: "green" | "amber" | "red" = !linked
    ? "amber"
    : expired
      ? "red"
      : vaultLocked
        ? "amber"
        : "green";

  return (
    <>
      <button
        className="account-avatar"
        onClick={() => setOpen(true)}
        title={
          !linked
            ? "Not signed in (open Account)"
            : expired
              ? "Session expired (open Account to re-link)"
              : `${discord?.username ?? "Account"} (open Account)`
        }
        aria-label="Account"
      >
        {avatarUrl ? (
          <img src={avatarUrl} alt="" />
        ) : (
          <UserRound size={16} />
        )}
        <span className={`account-dot ${sessionTone}`} />
      </button>

      {open && (
        <div className="account-overlay" role="dialog" aria-label="Account" aria-modal>
          <header className="account-overlay-head">
            <button
              className="account-overlay-back"
              onClick={() => setOpen(false)}
              title="Back (Esc)"
              aria-label="Back to trading"
            >
              <ArrowLeft size={16} /> Back
            </button>
            <span className="account-overlay-title">Account</span>
            <span className="account-overlay-sub">
              Device-wide, shared across Solana &amp; Perpetuals
            </span>
            <button
              className="btn account-overlay-close"
              onClick={() => setOpen(false)}
              title="Close (Esc)"
            >
              <X size={15} /> Close
            </button>
          </header>
          <div className="account-overlay-body">
            <div className="container">
              <AccountTab status={status} onReload={onReload} embedded />
            </div>
          </div>
        </div>
      )}
    </>
  );
}
