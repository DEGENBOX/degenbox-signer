// "What is running right now?" — pure derivation shared by the BOTS
// tab's Running-now section and the LIVE tab's one-line strip (operator
// feedback R4). No IPC here; callers hand in the snapshots they
// already poll.

import type { SolCopyConfigFull } from "../../ipc";
import type { BotPreset } from "./ipc";

export interface RunningSummary {
  /** True until at least one of the two sources has answered. */
  loading: boolean;
  /** Enabled (server-live) auto-buy sessions. */
  sessions: BotPreset[];
  /** Enabled copy follows. */
  follows: SolCopyConfigFull[];
  /** Enabled sessions NOT armed on this device (0 while the device
   * status is still unknown — never accuse without proof). */
  unarmed: number;
  /** Strip line, e.g. "2 presets · 1 copy follow". */
  line: string;
  /** Hover detail: names behind the counts. */
  title: string;
  dot: "green" | "amber" | "grey";
  pulse: boolean;
}

export function summarizeRunning(
  sessions: BotPreset[] | null,
  copyRows: SolCopyConfigFull[] | null,
  armedIds: Set<string> | null,
): RunningSummary {
  const loading = sessions === null && copyRows === null;
  const live = (sessions ?? []).filter((s) => s.enabled);
  const follows = (copyRows ?? []).filter((c) => c.enabled);
  const unarmed =
    armedIds === null ? 0 : live.filter((s) => !armedIds.has(s.id)).length;
  const total = live.length + follows.length;

  const parts: string[] = [];
  if (live.length > 0) {
    parts.push(`${live.length} preset${live.length === 1 ? "" : "s"}`);
  }
  if (follows.length > 0) {
    parts.push(`${follows.length} copy follow${follows.length === 1 ? "" : "s"}`);
  }
  const line = loading
    ? "checking…"
    : total === 0
      ? "nothing right now"
      : parts.join(" · ");

  const titleBits: string[] = [];
  if (live.length > 0) {
    titleBits.push(
      `Presets: ${live
        .map((s) =>
          armedIds === null
            ? s.name
            : `${s.name} (${armedIds.has(s.id) ? "on this device" : "server only"})`,
        )
        .join(", ")}`,
    );
  }
  if (follows.length > 0) {
    titleBits.push(`Following: ${follows.map((c) => c.label).join(", ")}`);
  }
  const title = titleBits.join(" · ");

  const dot: RunningSummary["dot"] =
    total === 0 ? "grey" : unarmed > 0 ? "amber" : "green";
  return {
    loading,
    sessions: live,
    follows,
    unarmed,
    line,
    title,
    dot,
    pulse: dot === "green",
  };
}
