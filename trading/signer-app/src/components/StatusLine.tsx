// Module status line — the first thing on every LIVE tab: at a glance,
// is my bot connected, in paper or live, alive (heartbeat), and when did
// it last do something. Presentational: each LIVE page assembles the
// items (venue data differs) + an optional paper/live control on the
// right. Reuses the theme's status-dot + hud-label; no new tokens.

import type { ReactNode } from "react";

export type DotTone = "green" | "amber" | "red" | "grey";

export interface StatusItem {
  label: string;
  value: ReactNode;
  /** Optional leading status dot. */
  dot?: DotTone;
  pulse?: boolean;
  tone?: "pos" | "neg";
  title?: string;
}

export function StatusLine({
  items,
  right,
}: {
  items: StatusItem[];
  right?: ReactNode;
}) {
  return (
    <div className="status-line">
      {items.map((it) => (
        <div className="status-item" key={it.label} title={it.title}>
          <span className="hud-label">{it.label}</span>
          <span className={`status-item-val ${it.tone ?? ""}`}>
            {it.dot && (
              <span
                className={`status-dot ${it.dot === "grey" ? "" : it.dot} ${
                  it.pulse ? "pulse" : ""
                }`}
              />
            )}
            {it.value}
          </span>
        </div>
      ))}
      {right && <div className="status-line-right">{right}</div>}
    </div>
  );
}
