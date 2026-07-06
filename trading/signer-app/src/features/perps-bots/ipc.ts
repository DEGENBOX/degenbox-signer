// features/perps-bots — typed IPC facade for the Perpetuals Bots tab
// (W4). CONSUMES src/ipc.ts only; no new Rust commands are referenced
// here. The perps side has no multi-bot fleet — THIS DEVICE is the
// sole executor — so the surface is the HL daemon command set:
//
//   hl_status            — conn / queue / balance / pairing snapshot
//   hl_pairing_status    — server-side pairing truth (15 s poll)
//   hl_set_paper_mode    — resolve-but-never-submit toggle
//   hl_unpair            — drop pairing token + master account
//   remove_hl_keystore   — delete the encrypted agent keystore
//   set_paused           — DEVICE-WIDE signing kill-switch (both
//                          runtimes; there is no HL-only pause command)
//   list_recent_signs    — the signing feed (chain-tagged)

import {
  ipc,
  type HlPairingStatus,
  type HlStatus,
  type RecentSign,
  type StatusReport,
} from "../../ipc";

export { ipc };
export type { HlPairingStatus, HlStatus, RecentSign, StatusReport };
