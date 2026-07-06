// Multi-wallet key material (iteration 3, defect 2). The vault is
// multi-wallet first-class: a per-venue wallet LIST (label, address,
// primary, runtime) with inline add + per-wallet backup / reveal /
// remove. Replaces the old singular "Solana wallet: not set up /
// Perpetuals agent: not set up" rows.
//
// No popups for config (platform rule): "Add wallet" and "Reveal
// secret" are INLINE expanding editors; only the destructive Remove
// keeps a type-to-confirm (allowed).

import { useCallback, useEffect, useState } from "react";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { Download, Eye, EyeOff, Plus, Trash2, X } from "lucide-react";
import { CopyButton, DangerConfirm, shortAddr } from "../../components/ui";
import { ipc, type ClientInfo, type StatusReport } from "../../ipc";

type Chain = "sol" | "hl";

/** Gateway-only rows (id `gw-<uuid>`, `runtime_state === "remote"`) are
 *  client bindings registered on the account server for wallets that are
 *  NOT sealed in this device's vault. They have no local keystore, so
 *  they can't be backed up or revealed here — `client_remove` would 404
 *  with "wallet not found". They live in their own list below the local
 *  table; the only action is deregistering the gateway row itself
 *  (`clientGatewayDeregister`, v0.3.1 A3). */
const isRemoteBinding = (c: ClientInfo) =>
  c.id.startsWith("gw-") || c.runtime_state === "remote";

/** Turn the raw runtime state ("executor:ready", "standby:registered",
 *  "locked", …) into words a person understands. The raw value stays in
 *  the tooltip for support. */
function humanRuntime(state: string): string {
  if (state === "locked") return "locked";
  if (state === "remote") return "on another device";
  const [kind, detail = ""] = state.split(":");
  if (kind === "standby") return "standing by";
  if (kind === "executor") {
    switch (detail) {
      case "ready":
        return "running";
      case "offline":
        return "offline";
      case "connecting":
        return "connecting";
      case "waiting_auth":
        return "waiting for sign-in";
      case "auth_expired":
        return "sign-in expired";
      case "error":
        return "error";
      default:
        return detail.replace(/_/g, " ") || "running";
    }
  }
  return state.replace(/_/g, " ");
}

export function WalletsCard({
  status,
  onReload,
}: {
  status: StatusReport | null;
  onReload: () => void;
}) {
  const [clients, setClients] = useState<ClientInfo[] | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setClients(await ipc.clientsList());
      setErr(null);
    } catch (e) {
      setErr(String(e));
    }
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 8000);
    return () => clearInterval(id);
  }, [load]);

  const reload = useCallback(() => {
    load();
    onReload();
  }, [load, onReload]);

  const sol = (clients ?? []).filter((c) => c.chain === "sol");
  const hl = (clients ?? []).filter((c) => c.chain === "hl");
  const vaultLocked = !(status?.sol_unlocked || status?.hl_unlocked);

  return (
    <div className="card">
      <div className="card-title">Wallets &amp; keys</div>
      <p>
        Every wallet sealed in this device's vault, per venue. Keys are encrypted under
        your passphrase and never leave this machine.
      </p>
      {err && <div className="error-box">{err}</div>}

      <WalletGroup
        chain="sol"
        title="Solana"
        wallets={sol}
        loading={clients === null}
        vaultLocked={vaultLocked}
        onChanged={reload}
      />
      <WalletGroup
        chain="hl"
        title="Perpetuals"
        wallets={hl}
        loading={clients === null}
        vaultLocked={vaultLocked}
        onChanged={reload}
      />
    </div>
  );
}

function WalletGroup({
  chain,
  title,
  wallets,
  loading,
  vaultLocked,
  onChanged,
}: {
  chain: Chain;
  title: string;
  wallets: ClientInfo[];
  loading: boolean;
  vaultLocked: boolean;
  onChanged: () => void;
}) {
  const [adding, setAdding] = useState(false);

  // Only wallets whose keys actually live in this device's vault are
  // actionable (backup / reveal / remove). Gateway-only bindings are
  // shown read-only below so they never 404 on remove.
  const local = wallets.filter((w) => !isRemoteBinding(w));
  const remote = wallets.filter((w) => isRemoteBinding(w));

  return (
    <div className="wallet-group">
      <div className="wallet-group-head">
        <span className="hud-label">{title}</span>
        <span className="hud-label brackets">{loading ? "…" : local.length}</span>
        <button
          className="btn sm"
          style={{ marginLeft: "auto" }}
          onClick={() => setAdding((a) => !a)}
        >
          {adding ? <X size={12} /> : <Plus size={12} />} {adding ? "Cancel" : "Add wallet"}
        </button>
      </div>

      {adding && (
        <AddWalletEditor
          chain={chain}
          onDone={() => {
            setAdding(false);
            onChanged();
          }}
          onCancel={() => setAdding(false)}
        />
      )}

      {!loading && local.length === 0 && remote.length === 0 && !adding && (
        <p className="wallet-empty">
          No {title} wallet yet. Add one to trade this venue from this device.
        </p>
      )}

      {local.length > 0 && (
        <table className="table wallet-table">
          <thead>
            <tr>
              <th>Wallet</th>
              <th>Address</th>
              <th>Status</th>
              <th style={{ textAlign: "right" }}>Keys</th>
            </tr>
          </thead>
          <tbody>
            {local.map((w) => (
              <WalletRow key={w.id} w={w} vaultLocked={vaultLocked} onChanged={onChanged} />
            ))}
          </tbody>
        </table>
      )}

      {remote.length > 0 && (
        <div className="wallet-remote">
          <span className="hud-label">Registered from another device</span>
          <p className="wallet-remote-hint">
            These {title} wallets belong to your DegenBox account but were set up in a
            different install of this app. Their keys live in that install's vault, never
            on our servers. To back one up, open the app on the device that created it.
          </p>
          <ul className="wallet-remote-list">
            {remote.map((w) => (
              <RemoteRow key={w.id} w={w} onChanged={onChanged} />
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

/** One gateway-only binding: read-only identity plus a "Remove" that
 *  deregisters the row from the account server. Nothing key-related
 *  happens; the other device's vault is untouched. */
function RemoteRow({ w, onChanged }: { w: ClientInfo; onChanged: () => void }) {
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // `gateway.id` is the server row's id; the local `gw-<uuid>` id is
  // derived from it, so stripping the prefix is a safe fallback.
  const gatewayId = w.gateway?.id ?? w.id.replace(/^gw-/, "");

  const remove = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.clientGatewayDeregister(gatewayId);
      setConfirmOpen(false);
      onChanged();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <li>
      <span className="wallet-remote-name">{w.label ?? shortAddr(w.address, 4, 4)}</span>
      <span className="mono wallet-remote-addr">{shortAddr(w.address, 6, 6)}</span>
      <span className="badge">remote</span>
      <button
        className="btn xs danger"
        title="Remove this registration from your account"
        onClick={() => {
          setErr(null);
          setConfirmOpen(true);
        }}
      >
        <Trash2 size={11} /> Remove
      </button>
      <DangerConfirm
        open={confirmOpen}
        title="Remove registration"
        phrase="remove"
        busy={busy}
        error={err}
        onCancel={() => setConfirmOpen(false)}
        onConfirm={remove}
      >
        <p style={{ marginTop: 0 }}>
          This removes the registration for{" "}
          <strong>{w.label ?? shortAddr(w.address, 6, 6)}</strong> from your DegenBox
          account, along with its server-side settings (label, pause state, budget,
          preset bindings). The keys on the other device are untouched. If the app there
          is still running, it may register the wallet again.
        </p>
      </DangerConfirm>
    </li>
  );
}

function WalletRow({
  w,
  vaultLocked,
  onChanged,
}: {
  w: ClientInfo;
  vaultLocked: boolean;
  onChanged: () => void;
}) {
  const [revealing, setRevealing] = useState(false);
  const [removeOpen, setRemoveOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  const exportBackup = async () => {
    setErr(null);
    setMsg(null);
    try {
      const dest = await saveFileDialog({
        title: "Save encrypted keystore backup",
        defaultPath: `degenbox-${w.chain}-${w.address.slice(0, 6)}.json`,
        filters: [{ name: "Keystore", extensions: ["json"] }],
      });
      if (typeof dest === "string" && dest) {
        await ipc.clientExportKeystore(w.id, dest);
        setMsg("Encrypted backup written. It stays locked under your passphrase.");
      }
    } catch (e) {
      setErr(String(e));
    }
  };

  const remove = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.clientRemove(w.id);
      setRemoveOpen(false);
      onChanged();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <tr>
        <td>
          <strong>{w.label ?? shortAddr(w.address, 4, 4)}</strong>
          {w.primary && <span className="badge accent" style={{ marginLeft: 6 }}>primary</span>}
        </td>
        <td className="mono">
          <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
            {shortAddr(w.address, 6, 6)}
            <CopyButton text={w.address} label="Copy address" />
          </span>
        </td>
        <td>
          <span
            className="hud-label"
            title={[w.runtime_detail, w.runtime_state].filter(Boolean).join(" · ") || undefined}
          >
            {humanRuntime(w.runtime_state)}
          </span>
        </td>
        <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
          <button className="btn xs" title="Encrypted JSON backup" onClick={exportBackup}>
            <Download size={12} />
          </button>
          {w.chain === "sol" && (
            <button
              className="btn xs"
              style={{ marginLeft: 4 }}
              disabled={vaultLocked}
              title={vaultLocked ? "Unlock the vault first" : "Reveal the raw secret (passphrase required)"}
              onClick={() => setRevealing((r) => !r)}
            >
              {revealing ? <EyeOff size={12} /> : <Eye size={12} />}
            </button>
          )}
          <button
            className="btn xs danger"
            style={{ marginLeft: 4 }}
            title="Remove this wallet from the device"
            onClick={() => {
              setErr(null);
              setRemoveOpen(true);
            }}
          >
            <Trash2 size={12} />
          </button>
        </td>
      </tr>
      {msg && (
        <tr>
          <td colSpan={4} style={{ color: "var(--accent)", fontSize: 12 }}>
            {msg}
          </td>
        </tr>
      )}
      {err && (
        <tr>
          <td colSpan={4}>
            <div className="error-box">{err}</div>
          </td>
        </tr>
      )}
      {revealing && w.chain === "sol" && (
        <tr>
          <td colSpan={4} style={{ padding: "6px 8px 12px" }}>
            <RevealInline pubkey={w.address} onClose={() => setRevealing(false)} />
          </td>
        </tr>
      )}
      <DangerConfirm
        open={removeOpen}
        title="Remove wallet"
        phrase="remove"
        busy={busy}
        error={err}
        onCancel={() => setRemoveOpen(false)}
        onConfirm={remove}
      >
        <p style={{ marginTop: 0 }}>
          This deletes the encrypted keystore for{" "}
          <strong>{w.label ?? shortAddr(w.address, 6, 6)}</strong> from this machine and
          stops its runtime.{" "}
          {w.chain === "sol" ? (
            <strong>Without a backup, funds on this wallet are unrecoverable.</strong>
          ) : (
            <>Agent keys are sandboxed; mint a fresh one on app.hyperliquid.xyz anytime.</>
          )}{" "}
          Export a backup first if you haven't.
        </p>
      </DangerConfirm>
    </>
  );
}

/** Inline per-wallet secret reveal — passphrase in, raw secret out. No
 *  modal; the secret is shown in-row and cleared on close. */
function RevealInline({ pubkey, onClose }: { pubkey: string; onClose: () => void }) {
  const [password, setPassword] = useState("");
  const [secret, setSecret] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const reveal = async () => {
    if (!password) return;
    setBusy(true);
    setErr(null);
    try {
      setSecret(await ipc.revealSolSecret(password, pubkey));
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="reveal-inline">
      {secret ? (
        <>
          <span className="hud-label">Secret key</span>
          <div className="reveal-secret mono">
            <span>{secret}</span>
            <CopyButton text={secret} label="Copy secret" />
          </div>
          <div className="btn-row">
            <button className="btn sm" onClick={() => { setSecret(null); setPassword(""); onClose(); }}>
              Hide
            </button>
          </div>
        </>
      ) : (
        <>
          <label className="field">Passphrase to reveal the raw secret</label>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              className="input mono"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && reveal()}
              placeholder="vault passphrase"
              style={{ flex: 1 }}
              autoFocus
            />
            <button className="btn sm primary" disabled={busy || !password} onClick={reveal}>
              Reveal
            </button>
            <button className="btn sm" onClick={onClose}>
              Cancel
            </button>
          </div>
          <p className="reveal-warn">
            Anyone with this secret controls the wallet. Never paste it anywhere but a
            wallet you trust.
          </p>
          {err && <div className="error-box">{err}</div>}
        </>
      )}
    </div>
  );
}

/** Inline add-wallet editor — generate (Solana) or import a secret,
 *  label it, unlock with the vault passphrase. No modal. */
function AddWalletEditor({
  chain,
  onDone,
  onCancel,
}: {
  chain: Chain;
  onDone: () => void;
  onCancel: () => void;
}) {
  const [method, setMethod] = useState<"generate" | "import">(
    chain === "sol" ? "generate" : "import",
  );
  const [label, setLabel] = useState("");
  const [secret, setSecret] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const submit = async () => {
    if (!password) {
      setErr("Enter your vault passphrase.");
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      if (method === "generate") {
        await ipc.clientAdd("sol", password, label.trim() || undefined);
      } else {
        if (!secret.trim()) {
          setErr("Paste the secret key / seed to import.");
          setBusy(false);
          return;
        }
        await ipc.clientImport(chain, secret.trim(), password, label.trim() || undefined);
      }
      onDone();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="add-wallet">
      {chain === "sol" && (
        <div className="field-group">
          <label className="field">How</label>
          <div className="seg-row">
            <button
              className={`btn sm ${method === "generate" ? "primary" : ""}`}
              onClick={() => setMethod("generate")}
            >
              Generate new
            </button>
            <button
              className={`btn sm ${method === "import" ? "primary" : ""}`}
              onClick={() => setMethod("import")}
            >
              Import key
            </button>
          </div>
        </div>
      )}

      <div className="field-group">
        <label className="field">Label (optional)</label>
        <input
          className="input"
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          placeholder={chain === "sol" ? "e.g. Runner 1" : "e.g. Copytrade agent"}
        />
      </div>

      {method === "import" && (
        <div className="field-group">
          <label className="field">
            {chain === "sol" ? "Secret key or seed phrase" : "Agent private key"}
          </label>
          <input
            className="input mono"
            type="password"
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
            placeholder={chain === "sol" ? "base58 secret or 12/24-word seed" : "0x… agent key"}
          />
          {chain === "hl" && (
            <p className="reveal-warn" style={{ marginTop: 4 }}>
              Mint an agent key at app.hyperliquid.xyz/API. It can trade but never
              withdraw. Pair it on the Perpetuals · Bots tab after adding.
            </p>
          )}
        </div>
      )}

      <div className="field-group">
        <label className="field">Vault passphrase</label>
        <input
          className="input mono"
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          placeholder="unlocks the vault to seal the new key"
        />
      </div>

      {err && <div className="error-box">{err}</div>}
      <div className="btn-row">
        <button className="btn primary" disabled={busy} onClick={submit}>
          {method === "generate" ? "Generate & add" : "Import & add"}
        </button>
        <button className="btn" disabled={busy} onClick={onCancel}>
          Cancel
        </button>
      </div>
    </div>
  );
}
