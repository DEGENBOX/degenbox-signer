// Data ownership for the Solana copy-trade surface (R4 running-now
// rework): ONE 15-second poll covering the full editable configs, the
// venue-merged copy stats and the W2.3 volume summary. Hoisted out of
// CopyTradeSection so the Bots tab's "Running now" list and the config
// table read the SAME snapshot instead of polling twice.

import { useCallback, useEffect, useState } from "react";
import {
  fetchCopySummary,
  ipc,
  type CopyTradeSummaryView,
  type CopytradeConfig,
  type SolCopyConfigFull,
} from "./ipc";

const POLL_MS = 15_000;

export interface CopyTrade {
  /** Full-field configs (the editable rows); null while loading. */
  rows: SolCopyConfigFull[] | null;
  /** Venue-merged copies-24h / last-copy stats. */
  stats: CopytradeConfig[] | null;
  /** W2.3 mirrored-volume summary (may be dark on older gateways). */
  summary: CopyTradeSummaryView | null;
  summaryErr: boolean;
  err: string | null;
  setErr: (e: string | null) => void;
  reload: () => Promise<void>;
}

export function useCopyTrade(): CopyTrade {
  const [rows, setRows] = useState<SolCopyConfigFull[] | null>(null);
  const [stats, setStats] = useState<CopytradeConfig[] | null>(null);
  const [summary, setSummary] = useState<CopyTradeSummaryView | null>(null);
  const [summaryErr, setSummaryErr] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const reload = useCallback(async () => {
    const [full, st, sum] = await Promise.allSettled([
      ipc.solCopyConfigsFull(),
      ipc.copytradeConfigs(),
      fetchCopySummary(),
    ]);
    if (full.status === "fulfilled") {
      setRows(full.value);
      setErr(null);
    } else {
      setErr(String(full.reason));
    }
    if (st.status === "fulfilled") setStats(st.value);
    if (sum.status === "fulfilled") {
      setSummary(sum.value);
      setSummaryErr(false);
    } else {
      // Older gateway / endpoint dark — keep the last value, flag it.
      setSummaryErr(true);
    }
  }, []);

  useEffect(() => {
    reload();
    const id = setInterval(reload, POLL_MS);
    return () => clearInterval(id);
  }, [reload]);

  return { rows, stats, summary, summaryErr, err, setErr, reload };
}
