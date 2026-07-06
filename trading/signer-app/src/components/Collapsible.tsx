// Minimal collapsible section — used to keep the activity feed present
// but compact/collapsed-by-default on the LIVE tabs (operator: no
// separate activity tab; fold it into Live). Header stays visible so the
// live/stale dot and count remain glanceable when collapsed.

import { useState, type ReactNode } from "react";
import { ChevronDown } from "lucide-react";

export function Collapsible({
  title,
  hint,
  defaultOpen = false,
  children,
}: {
  title: string;
  hint?: ReactNode;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <section className="collapsible">
      <button
        className="collapsible-head"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
      >
        <ChevronDown size={14} className={`collapsible-chev ${open ? "open" : ""}`} />
        <span className="shell-section-title">{title}</span>
        {hint && <span className="hud-label brackets">{hint}</span>}
      </button>
      {open && <div className="collapsible-body">{children}</div>}
    </section>
  );
}
