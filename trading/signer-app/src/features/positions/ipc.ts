// Positions-tab IPC wrappers (W3.1). `src/ipc.ts` is read-only this
// wave, so the EXTENDED shape of the existing `sol_positions` command
// lives here: the Rust DTO (src-tauri/src/sol/gateway.rs) gained the
// fields the gateway already returned but the old mapping dropped
// (token meta, market caps, SOL-denominated figures, realized PnL).
// The base `SolPosition` consumers elsewhere keep working unchanged.

import { invoke } from "@tauri-apps/api/core";
import type { SolPosition } from "../../ipc";

export interface SolPositionEx extends SolPosition {
  /** Token display name (alpha_tokens). */
  name: string | null;
  /** Token logo URL (alpha_tokens). */
  image_url: string | null;
  /** Live market cap, USD decimal string. */
  mcap_usd: string | null;
  /** Market cap at avg entry (mcap_now × entry/now), USD. */
  entry_mcap_usd: string | null;
  /** Average entry price per token, USD. */
  avg_entry_price_usd: string | null;
  /** Live SOL/USD. */
  sol_price_usd: string | null;
  /** Net SOL cost basis (in − out). */
  cost_sol: string | null;
  /** Current value in SOL. */
  value_sol: string | null;
  /** Unrealized PnL in SOL. */
  pnl_sol: string | null;
  /** Cumulative realized PnL banked on this mint, lamports. */
  realized_pnl_lamports: number;
  /** Lifetime fill count. */
  fill_count: number;
}

/** Same Rust command as `ipc.solPositions()`, richer type. */
export const solPositionsEx = () => invoke<SolPositionEx[]>("sol_positions");
