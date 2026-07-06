// Hyperliquid setup wizard — modal, focused, re-runnable.
//
// Steps:
//   1. Agent key  — import the HL API agent key (minted on
//                   app.hyperliquid.xyz/API; pure-random generation
//                   would produce an unregistered address, so the app
//                   deliberately imports only).
//   2. Pairing    — register with the DegenBox gateway. Primary path:
//                   the linked Discord account's token (one click).
//                   Fallback: a pasted connect token. Handles the 428
//                   TOTP retry inline.
//   3. Done.

import { useEffect, useState } from "react";
import { Check, ExternalLink, Globe, KeyRound, Link2 } from "lucide-react";
import {
  ipc,
  type DiscordStatus,
  type HlStatus,
  type StatusReport,
} from "../ipc";
import { Modal } from "./ui";
import { BackendPick, StepStrip } from "./SolanaWizard";

type Step = "key" | "pair" | "done";

interface Props {
  open: boolean;
  onClose: () => void;
  onDone: () => void;
  status: StatusReport | null;
  hl: HlStatus | null;
  /** "Add another wallet" mode: always start at the key step and pair
   * the freshly imported agent (vault-append) instead of the primary. */
  forceNewKey?: boolean;
}

export function HlWizard({ open, onClose, onDone, status, hl, forceNewKey }: Props) {
  const [step, setStep] = useState<Step>("key");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [discord, setDiscord] = useState<DiscordStatus | null>(null);

  // key step
  const [hlKey, setHlKey] = useState("");
  const [password, setPassword] = useState("");
  const [password2, setPassword2] = useState("");
  const [backend, setBackend] = useState<"file" | "keychain">("keychain");
  const [agentAddress, setAgentAddress] = useState<string | null>(null);

  // pair step
  const [serverUrl, setServerUrl] = useState("https://api-v2.degenbox.app");
  const [useDiscord, setUseDiscord] = useState(true);
  const [token, setToken] = useState("");
  const [account, setAccount] = useState("");
  const [totp, setTotp] = useState("");
  const [needsTotp, setNeedsTotp] = useState(false);
  const [pairedUser, setPairedUser] = useState<string | null>(null);

  const hasKey = forceNewKey ? !!agentAddress : !!(agentAddress ?? status?.hl_address);
  const hasOtherKeystore = !!status?.sol_pubkey;
  const discordReady = !!discord?.linked && !discord.expired;

  useEffect(() => {
    if (!open) return;
    setStep(!forceNewKey && status?.hl_address ? "pair" : "key");
    setBusy(false);
    setErr(null);
    setHlKey("");
    setPassword("");
    setPassword2("");
    setAgentAddress(null);
    setToken("");
    setTotp("");
    setNeedsTotp(false);
    setPairedUser(null);
    if (hl?.server_url) setServerUrl(hl.server_url);
    if (hl?.account_address) setAccount(hl.account_address);
    ipc
      .discordStatus()
      .then((d) => {
        setDiscord(d);
        setUseDiscord(d.linked && !d.expired);
      })
      .catch(() => {
        // Status unreadable — never leave the user on a Discord path
        // that can't work; fall back to the connect-token path.
        setDiscord(null);
        setUseDiscord(false);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setErr(null);
    try {
      await fn();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const passwordOk = hasOtherKeystore
    ? password.length >= 8
    : password.length >= 8 && password === password2;

  const importKey = () =>
    run(async () => {
      const res = await ipc.importHlKeystore(hlKey.trim(), password);
      // Wipe the pasted private key from React state immediately.
      setHlKey("");
      setAgentAddress(res.address);
      // Unlock right away so the daemon can come online after pairing.
      await ipc.unlock(password, backend);
      setStep("pair");
    });

  const accountOk = /^0x[0-9a-fA-F]{40}$/.test(account.trim());

  const pair = () =>
    run(async () => {
      const res = await ipc.hlPair(
        serverUrl,
        useDiscord ? "" : token.trim(),
        account.trim(),
        needsTotp ? totp : undefined,
        // Pair the wallet this wizard run imported — in add-another
        // mode that's a secondary, never the primary.
        agentAddress ?? undefined,
      );
      if (res.needs_totp) {
        setNeedsTotp(true);
        setErr("This account has 2FA. Enter your 6-digit code and pair again.");
        return;
      }
      setPairedUser(res.discord_handle ?? res.user_id);
      setStep("done");
    });

  const finish = () => {
    onDone();
    onClose();
  };

  const stepIndex = step === "key" ? 0 : step === "pair" ? 1 : 2;

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
          <span className="chain-dot hl" /> Hyperliquid setup
        </span>
      }
      locked={busy}
      width={520}
    >
      <StepStrip labels={["Agent key", "Pairing", "Finish"]} active={stepIndex} />

      {step === "key" && (
        <>
          {hasKey ? (
            <>
              <div className="banner info" role="status">
                <Check size={15} style={{ flexShrink: 0 }} />
                <span>
                  Agent key already imported:{" "}
                  <span className="mono">{short(agentAddress ?? status!.hl_address!)}</span>
                </span>
              </div>
              <div className="modal-foot">
                <button className="btn primary" onClick={() => setStep("pair")}>
                  Continue to pairing
                </button>
              </div>
            </>
          ) : (
            <>
              <p>
                Hyperliquid trades are signed by a sandboxed <strong>API agent key</strong>.
                It can trade for your main wallet but never withdraw. Mint one at{" "}
                <span className="mono" style={{ color: "var(--accent)" }}>
                  app.hyperliquid.xyz/API
                </span>{" "}
                and paste the private key here. This is the only place it lives.
              </p>
              <div className="field-group">
                <label className="field">Agent private key (hex, 64 chars)</label>
                <input
                  type="password"
                  className="input mono"
                  value={hlKey}
                  onChange={(e) => setHlKey(e.target.value)}
                  placeholder="0x…"
                  autoFocus
                />
              </div>
              <div className="field-group">
                <label className="field">
                  {hasOtherKeystore
                    ? "Keystore passphrase (same as your Solana wallet)"
                    : "Encryption passphrase (8+ characters)"}
                </label>
                <input
                  type="password"
                  className="input"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
              </div>
              {!hasOtherKeystore && (
                <div className="field-group">
                  <label className="field">Confirm passphrase</label>
                  <input
                    type="password"
                    className="input"
                    value={password2}
                    onChange={(e) => setPassword2(e.target.value)}
                  />
                </div>
              )}
              {hasOtherKeystore ? (
                <p style={{ fontSize: 11.5, color: "var(--fg-faint)" }}>
                  One passphrase unlocks every key in this app. The HL key is encrypted
                  under the same one as your existing wallet.
                </p>
              ) : (
                <BackendPick backend={backend} onBackend={setBackend} />
              )}
              {err && <div className="error-box">{err}</div>}
              <div className="modal-foot">
                <button
                  className="btn primary"
                  disabled={!hlKey.trim() || !passwordOk || busy}
                  onClick={importKey}
                >
                  <KeyRound size={14} /> {busy ? "Importing…" : "Import agent key"}
                </button>
              </div>
            </>
          )}
        </>
      )}

      {step === "pair" && (
        <>
          <p>
            Pair this device with DegenBox so the gateway can queue orders for it to
            sign. Your key never leaves this machine.
          </p>
          {!status?.hl_unlocked && (
            <div className="banner warn" role="status">
              Keystore is locked: pairing works, but signing starts only after you
              unlock.
            </div>
          )}
          <div className="field-group">
            <label className="field">DegenBox server</label>
            <input
              className="input mono"
              value={serverUrl}
              onChange={(e) => setServerUrl(e.target.value)}
            />
          </div>

          <label className="field" style={{ marginBottom: 6 }}>
            Authorize with
          </label>
          <div className="choice-grid" style={{ marginTop: 0, marginBottom: 13 }}>
            <button
              className={`choice ${useDiscord ? "selected" : ""}`}
              onClick={() => setUseDiscord(true)}
              disabled={!discordReady}
              type="button"
              style={!discordReady ? { opacity: 0.55, cursor: "not-allowed" } : undefined}
            >
              <div className="title">Discord account</div>
              <p className="desc">
                {discordReady
                  ? `Linked as ${discord!.username}. Pairs with one click.`
                  : "Link your Discord first (account menu, top right) to use this."}
              </p>
            </button>
            <button
              className={`choice ${!useDiscord ? "selected" : ""}`}
              onClick={() => setUseDiscord(false)}
              type="button"
            >
              <div className="title">Connect token</div>
              <p className="desc">
                Paste a token from the DegenBox dashboard → Connect a Signer.
              </p>
            </button>
          </div>

          {!useDiscord && (
            <div className="field-group">
              <label className="field">Connect token</label>
              <input
                className="input mono"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="from the DegenBox dashboard"
              />
            </div>
          )}

          <div className="field-group">
            <label className="field">HL master account (0x…)</label>
            <input
              className="input mono"
              value={account}
              onChange={(e) => setAccount(e.target.value)}
              placeholder="0x… your Hyperliquid MAIN wallet, NOT the agent"
            />
            {account.trim() !== "" && !accountOk && (
              <p style={{ fontSize: 11.5, color: "var(--amber)", margin: "5px 0 0" }}>
                Must be a 0x-prefixed 40-hex address: your main wallet, not the agent.
              </p>
            )}
          </div>

          {needsTotp && (
            <div className="field-group">
              <label className="field">Authenticator code</label>
              <input
                className="input mono"
                value={totp}
                inputMode="numeric"
                maxLength={6}
                onChange={(e) => setTotp(e.target.value.replace(/\D/g, ""))}
                placeholder="123456"
                autoFocus
              />
            </div>
          )}

          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            <button
              className="btn"
              style={{ marginRight: "auto" }}
              onClick={() => ipc.openSetupUrl(serverUrl)}
            >
              <Globe size={14} /> Open dashboard <ExternalLink size={12} />
            </button>
            <button className="btn" disabled={busy} onClick={finish}>
              Pair later
            </button>
            <button
              className="btn primary"
              disabled={
                busy ||
                !accountOk ||
                (!useDiscord && !token.trim()) ||
                (needsTotp && totp.length < 6)
              }
              onClick={pair}
            >
              <Link2 size={14} /> {busy ? "Pairing…" : "Pair signer"}
            </button>
          </div>
        </>
      )}

      {step === "done" && (
        <>
          <div className="banner info" role="status">
            <Check size={15} style={{ flexShrink: 0 }} />
            <span>
              <strong>Hyperliquid ready.</strong>{" "}
              {pairedUser ? (
                <>
                  Paired as <strong>{pairedUser}</strong>.{" "}
                </>
              ) : null}
              Queued orders are signed on this device while the keystore is unlocked.
            </span>
          </div>
          <div className="modal-foot">
            <button className="btn primary" onClick={finish}>
              Done
            </button>
          </div>
        </>
      )}
    </Modal>
  );
}

function short(s: string) {
  return s.length > 16 ? `${s.slice(0, 6)}…${s.slice(-6)}` : s;
}
