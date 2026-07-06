// Numbered shell section (blackalgo `01 /` pattern) — THE one section
// header for every tab surface (calm pass): number + title, optional
// bracketed count, optional right-side meta/actions. The wrapped
// page's own <h1> is hidden via `.shell-section > h1` in app.css so
// legacy pages embed without edits.
//
// Idiom (docs/ui-idiom.md): every page section uses this head — no
// hand-rolled `section-num + hud-label` rows.

import type { ReactNode } from "react";

interface Props {
  /** Two-digit section number, e.g. "01". */
  num: string;
  title: string;
  /** Bracketed count after the title, e.g. a row count. */
  count?: ReactNode;
  /** Right-aligned actions (buttons, toggles). */
  actions?: ReactNode;
  /** Right-aligned HUD hint (legacy slot; rendered after actions). */
  hint?: string;
  /** DOM anchor so in-page jumps (Running now → Manage) can target it. */
  id?: string;
  children: ReactNode;
}

export function ShellSection({ num, title, count, actions, hint, id, children }: Props) {
  return (
    <section className="shell-section" id={id}>
      <div className="shell-section-head">
        <span className="section-num">{num}</span>
        <span className="shell-section-title">{title}</span>
        {count != null && <span className="hud-label brackets">{count}</span>}
        {(actions || hint) && (
          <span className="head-meta">
            {actions}
            {hint && <span className="hud-label brackets">{hint}</span>}
          </span>
        )}
      </div>
      {children}
    </section>
  );
}
