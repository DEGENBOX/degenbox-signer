// Live Bot-activity polling hooks — the reliable spine for the desktop
// activity feed. Reads the SAME gateway endpoints the web Bot tabs poll
// (`/api/hyperliquid/exchange/bot/activity` + `/api/trading/bot/activity`),
// resolved through the desktop JWT by the `gateway_fetch` Tauri command
// (both routes are on its prefix allowlist — no bespoke IPC needed).
//
// No react-query in this app: a plain 4s poll mirrors the web's
// refetchInterval, keeping the last good snapshot on transient errors so
// the table never flashes empty. `error` is surfaced only before the
// first successful load.

import { useEffect, useRef, useState } from "react";
import { gwGet } from "../../lib/gateway";
import type { HlActivityRow, SolActivityRow } from "./lifecycle";

const POLL_MS = 4000;

export interface ActivityFeed<Row> {
  rows: Row[] | null;
  /** First-load error only; cleared on the first good snapshot. */
  error: string | null;
  /** A poll has landed at least once (drives the live/stale dot). */
  live: boolean;
  refetch: () => void;
}

function useActivity<Row>(path: `/api/${string}`): ActivityFeed<Row> {
  const [rows, setRows] = useState<Row[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [live, setLive] = useState(false);
  const got = useRef(false);
  const tick = useRef<() => void>(() => {});

  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const data = await gwGet<Row[]>(path);
        if (!alive) return;
        got.current = true;
        setRows(Array.isArray(data) ? data : []);
        setError(null);
        setLive(true);
      } catch (e) {
        if (!alive) return;
        setLive(false);
        // Keep the last good snapshot; only show the error pre-first-load.
        if (!got.current) setError(String(e));
      }
    };
    tick.current = load;
    load();
    const id = setInterval(load, POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [path]);

  return { rows, error, live, refetch: () => tick.current() };
}

export function useHlBotActivity(limit = 100): ActivityFeed<HlActivityRow> {
  return useActivity<HlActivityRow>(
    `/api/hyperliquid/exchange/bot/activity?limit=${limit}`,
  );
}

export function useSolBotActivity(limit = 100): ActivityFeed<SolActivityRow> {
  return useActivity<SolActivityRow>(`/api/trading/bot/activity?limit=${limit}`);
}
