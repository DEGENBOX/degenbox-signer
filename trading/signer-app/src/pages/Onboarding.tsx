// First-run welcome (W5 retheme — numbered sections, corner brackets,
// HUD microcopy; mirrors the Unlock gate's visual language). No forced
// rail — three clear entry points, none of which dead-end:
//
//   1. Link Discord (THE primary auth path — one browser hand-off
//      authorizes both runtimes).
//   2. Set up the Solana wallet (PK-paste-first wizard).
//   3. Set up Hyperliquid (agent-key paste → pair wizard).
//
// Everything is skippable; the main app's zero-states pick up whatever
// was left out. The page polls status while open so a wizard finishing
// (or the Discord deep link landing) flips its card to "done" live.

import { useEffect, useState } from "react";
import {
  ArrowRight,
  Check,
  ExternalLink,
  KeyRound,
  Lock,
  ShieldCheck,
  Wallet,
} from "lucide-react";
import {
  ipc,
  type DiscordStatus,
  type HlStatus,
  type StatusReport,
} from "../ipc";
import { SolanaWizard } from "../components/SolanaWizard";
import { HlWizard } from "../components/HlWizard";

interface Props {
  onDone: () => void;
}

export function Onboarding({ onDone }: Props) {
  const [status, setStatus] = useState<StatusReport | null>(null);
  const [hl, setHl] = useState<HlStatus | null>(null);
  const [discord, setDiscord] = useState<DiscordStatus | null>(null);
  const [solOpen, setSolOpen] = useState(false);
  const [hlOpen, setHlOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const s = await ipc.status();
        if (alive) setStatus(s);
      } catch {
        /* keep last */
      }
      try {
        const h = await ipc.hlStatus();
        if (alive) setHl(h);
      } catch {
        /* keep last */
      }
      try {
        const d = await ipc.discordStatus();
        if (alive) setDiscord(d);
      } catch {
        /* keep last */
      }
    };
    load();
    const id = setInterval(load, 2000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  const startDiscord = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.discordLoginStart();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const discordLinked = !!discord?.linked && !discord.expired;
  const hasSol = !!status?.sol_pubkey;
  const hasHl = !!status?.hl_address;
  const anySetup = hasSol || hasHl;

  return (
    <div className="onboard-screen">
      <div className="onboard-scroll">
        <div className="onboard-wrap">
          <div className="onboard-brand">
            <img src="/degenbox-logo.png" alt="" aria-hidden />
            <h1>Welcome to DegenBox</h1>
            <span className="hud-label brackets">Self-custodial trading client</span>
          </div>
          <p className="onboard-intro">
            This app holds your trading keys locally. They never touch our servers.
            Orders queued in the DegenBox cloud get signed on this machine and submitted
            directly to Solana and Hyperliquid.
          </p>

          <section className="onboard-section">
            <div className="shell-section-head">
              <span className="section-num">01</span>
              <span className="shell-section-title">Link Discord</span>
              {discordLinked && (
                <span className="badge ok" style={{ marginLeft: "auto" }}>
                  linked
                </span>
              )}
            </div>
            <div className="card">
              {discordLinked ? (
                <p style={{ marginBottom: 0 }}>
                  <Check size={13} style={{ verticalAlign: -2 }} /> Linked as{" "}
                  <strong>{discord!.username}</strong>. Both runtimes are authorized.
                </p>
              ) : (
                <>
                  <p>
                    One browser login authorizes everything: Solana data, copytrade and
                    one-click Hyperliquid pairing. Recommended first step.
                  </p>
                  {discord?.pending && (
                    <div className="banner info" role="status">
                      Waiting for the browser… finish the Discord authorization there.
                    </div>
                  )}
                  {(err ?? discord?.error) && (
                    <div className="error-box">{err ?? discord?.error}</div>
                  )}
                  <div className="btn-row" style={{ marginTop: 4 }}>
                    <button className="btn discord-btn" disabled={busy} onClick={startDiscord}>
                      {discord?.pending ? "Restart login" : "Connect Discord"}{" "}
                      <ExternalLink size={12} />
                    </button>
                  </div>
                </>
              )}
            </div>
          </section>

          <section className="onboard-section">
            <div className="shell-section-head">
              <span className="section-num">02</span>
              <span className="shell-section-title">Trading keys</span>
              {anySetup && (
                <span className="hud-label brackets" style={{ marginLeft: "auto" }}>
                  {hasSol && hasHl ? "Both set up" : hasSol ? "Solana ready" : "Perps ready"}
                </span>
              )}
            </div>
            <div className="choice-grid" style={{ marginTop: 0 }}>
              <button className="choice" onClick={() => setSolOpen(true)}>
                <div className="title">
                  <span className="chain-dot sol" /> <Wallet size={16} /> Solana wallet
                  {hasSol && (
                    <span className="badge ok" style={{ marginLeft: "auto" }}>
                      done
                    </span>
                  )}
                </div>
                <p className="desc">
                  Paste your private key (Phantom / Solflare), import a keystore, or
                  create a fresh hot wallet.
                </p>
              </button>
              <button className="choice" onClick={() => setHlOpen(true)}>
                <div className="title">
                  <span className="chain-dot hl" /> <KeyRound size={16} /> Perpetuals
                  {hasHl && (
                    <span className="badge ok" style={{ marginLeft: "auto" }}>
                      done
                    </span>
                  )}
                </div>
                <p className="desc">
                  Paste your Hyperliquid API agent key and pair this device with DegenBox.
                </p>
              </button>
            </div>
          </section>

          <div className="onboard-trust">
            <span className="hud-label">
              <ShieldCheck size={11} /> Self-custody · keys stay on this machine
            </span>
            <span className="hud-label">
              <Lock size={11} /> AES-256-GCM · Argon2id
            </span>
          </div>
        </div>
      </div>

      <div className="onboard-foot">
        <span className="hud-label">Everything here can be done later inside the app</span>
        <button className="btn primary" onClick={onDone}>
          {anySetup ? "Continue to app" : "Skip for now"} <ArrowRight size={14} />
        </button>
      </div>

      <SolanaWizard
        open={solOpen}
        onClose={() => setSolOpen(false)}
        onDone={() => setSolOpen(false)}
        hasHlKeystore={hasHl}
        hasSolWallet={hasSol}
      />
      <HlWizard
        open={hlOpen}
        onClose={() => setHlOpen(false)}
        onDone={() => setHlOpen(false)}
        status={status}
        hl={hl}
      />
    </div>
  );
}
