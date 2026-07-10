// "What is running right now?" — pure derivation shared by the BOTS
// tab's Running-now section and the LIVE tab's one-line strip (operator
// feedback R4). No IPC here; callers hand in the snapshots they
// already poll.

import type { BotPreset } from "./ipc";

export interface RunningSummary {
  /** True until the sessions source has answered. */
  loading: boolean;
  /** Enabled (server-live) auto-buy sessions. */
  sessions: BotPreset[];
  /** Enabled sessions NOT armed on this device (0 while the device
   * status is still unknown — never accuse without proof). */
  unarmed: number;
  /** Strip line, e.g. "2 presets". */
  line: string;
  /** Hover detail: names behind the counts. */
  title: string;
  dot: "green" | "amber" | "grey";
  pulse: boolean;
}

export function summarizeRunning(
  sessions: BotPreset[] | null,
  armedIds: Set<string> | null,
): RunningSummary {
  const loading = sessions === null;
  const live = (sessions ?? []).filter((s) => s.enabled);
  const unarmed =
    armedIds === null ? 0 : live.filter((s) => !armedIds.has(s.id)).length;
  const total = live.length;

  const line = loading
    ? "checking…"
    : total === 0
      ? "nothing right now"
      : `${live.length} preset${live.length === 1 ? "" : "s"}`;

  const title =
    live.length > 0
      ? `Presets: ${live
          .map((s) =>
            armedIds === null
              ? s.name
              : `${s.name} (${armedIds.has(s.id) ? "on this device" : "server only"})`,
          )
          .join(", ")}`
      : "";

  const dot: RunningSummary["dot"] =
    total === 0 ? "grey" : unarmed > 0 ? "amber" : "green";
  return {
    loading,
    sessions: live,
    unarmed,
    line,
    title,
    dot,
    pulse: dot === "green",
  };
}
