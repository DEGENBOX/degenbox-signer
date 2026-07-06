// features/bots — typed IPC facade for the Solana Bots tab (W3.3).
// CONSUMES src/ipc.ts only; no new Rust commands are referenced here.
//
// Wallet-attach matrix (what the create flow can actually do — mapped
// against src-tauri/src/clients.rs + sol/commands.rs):
//   generate   → client_add("sol")            fresh keypair into the vault
//   paste      → client_import("sol")         base58/hex private key, 32 or
//                                             64 bytes (NO BIP39 phrases —
//                                             parse_sol_secret, commands.rs:190)
//   extension  → import_extension_keystore    Chrome-extension JSON blob,
//                                             adopted into the vault (the
//                                             blob's password MUST equal the
//                                             master passphrase); returns a
//                                             pubkey only, so the vault id is
//                                             resolved by address afterwards
// "Re-attach an existing signer wallet" has NO dedicated command: the vault
// rejects duplicate addresses (vault.rs assert_new_address), so a wallet IS
// its client (1:1). What exists instead is client_activate(id, password) —
// it brings an idle/locked vault wallet's runtime online while the app is
// unlocked (the closest thing to "re-attach"), surfaced per card.

import {
  ipc,
  type BotDeviceStatus,
  type BotPreset,
  type ClientInfo,
  type CreateBotSessionReq,
  type LegSpec,
  type PresetLite,
  type SolWalletBalance,
  type StatusReport,
} from "../../ipc";

export { ipc };
export type {
  BotDeviceStatus,
  BotPreset,
  ClientInfo,
  CreateBotSessionReq,
  LegSpec,
  PresetLite,
  SolWalletBalance,
  StatusReport,
};

export const LAMPORTS = 1e9;

export type AttachMethod = "generate" | "paste" | "extension";

export interface AttachResult {
  /** Vault wallet id; null when the extension path couldn't resolve it. */
  id: string | null;
  address: string;
  /** Non-fatal — the wallet IS safely in the vault; its runtime starts
   * on the next lock/unlock cycle (or via the card's Activate action). */
  activationError: string | null;
}

/** One call for all three create paths: vault-append, then bring the
 * fresh wallet's runtime online NOW (clients.rs client_activate — the
 * vault-append alone leaves it idle until the next unlock). */
export async function attachClient(
  method: AttachMethod,
  opts: { secret?: string; label?: string; password: string },
): Promise<AttachResult> {
  let id: string | null = null;
  let address: string;

  if (method === "generate") {
    const r = await ipc.clientAdd("sol", opts.password, opts.label || undefined);
    id = r.id;
    address = r.address;
  } else if (method === "paste") {
    const r = await ipc.clientImport(
      "sol",
      (opts.secret ?? "").trim(),
      opts.password,
      opts.label || undefined,
    );
    id = r.id;
    address = r.address;
  } else {
    // Extension JSON: adopted into the vault, returns only the pubkey —
    // resolve the vault id by address, then apply the label (the adopt
    // command has no label parameter).
    const r = await ipc.importExtensionKeystore(opts.secret ?? "", opts.password);
    address = r.pubkey;
    try {
      const list = await ipc.clientsList();
      id = list.find((c) => c.chain === "sol" && c.address === address)?.id ?? null;
    } catch {
      id = null;
    }
    if (id && opts.label) {
      await ipc.clientLabel(id, opts.label).catch(() => {
        // cosmetic — the wallet is attached either way
      });
    }
  }

  let activationError: string | null = null;
  if (id) {
    try {
      await ipc.clientActivate(id, opts.password);
    } catch (e) {
      activationError = String(e);
    }
  } else {
    activationError =
      "vault id could not be resolved (the wallet comes online on the next unlock)";
  }
  return { id, address, activationError };
}
