// Shared UI primitives for the DegenBox client. Pure presentational —
// no IPC here.

import {
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";
import { Check, Copy, MoreHorizontal, Pencil, X } from "lucide-react";

/** Stat card: label on top, big mono value, optional sub-line. */
export function Stat({
  label,
  value,
  sub,
  tone,
  loading,
}: {
  label: string;
  value: ReactNode;
  sub?: ReactNode;
  tone?: "pos" | "neg";
  loading?: boolean;
}) {
  return (
    <div className="stat">
      <div className="stat-label">{label}</div>
      {loading ? (
        <span className="skeleton" style={{ width: "60%", height: 18, marginTop: 2 }} />
      ) : (
        <div className={`stat-value ${tone ?? ""}`}>{value}</div>
      )}
      {sub && !loading && <div className="stat-sub">{sub}</div>}
    </div>
  );
}

/** Wraps a disabled control so the tooltip still shows (disabled
 * buttons swallow hover events on some platforms). */
export function DisabledHint({ hint, children }: { hint: string; children: ReactNode }) {
  return (
    <span title={hint} style={{ display: "inline-flex" }}>
      {children}
    </span>
  );
}

/** Skeleton rows while a table loads. */
export function SkeletonRows({ rows, cols }: { rows: number; cols: number }) {
  return (
    <>
      {Array.from({ length: rows }).map((_, r) => (
        <tr key={r}>
          {Array.from({ length: cols }).map((_, c) => (
            <td key={c}>
              <span className="skeleton" style={{ width: c === 0 ? "70%" : "50%" }} />
            </td>
          ))}
        </tr>
      ))}
    </>
  );
}

export function shortAddr(s: string, head = 6, tail = 6) {
  return s.length > head + tail + 2 ? `${s.slice(0, head)}…${s.slice(-tail)}` : s;
}

export function timeAgo(iso: string | null): string {
  if (!iso) return "—";
  const ms = Date.now() - new Date(iso).getTime();
  if (!Number.isFinite(ms) || ms < 0) return "—";
  const m = Math.floor(ms / 60_000);
  if (m < 1) return "just now";
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

export function fmtUsd(v: string | null | undefined): string {
  if (v == null || v === "") return "—";
  const n = Number(v);
  if (!Number.isFinite(n)) return v;
  return n.toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: Math.abs(n) >= 1000 ? 0 : 2,
  });
}

/** Signed PnL with tone class. */
export function Pnl({ value }: { value: string | null }) {
  if (value == null || value === "") return <>—</>;
  const n = Number(value);
  if (!Number.isFinite(n)) return <>{value}</>;
  const cls = n > 0 ? "pos" : n < 0 ? "neg" : "";
  const sign = n > 0 ? "+" : "";
  return (
    <span className={cls}>
      {sign}
      {fmtUsd(value)}
    </span>
  );
}

/** Copy-to-clipboard with a transient "copied" state. Falls back to a
 * hidden textarea when the async clipboard API is unavailable in the
 * webview. */
export function CopyButton({ text, label }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const id = setTimeout(() => setCopied(false), 1400);
    return () => clearTimeout(id);
  }, [copied]);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      ta.remove();
    }
    setCopied(true);
  };

  return (
    <button
      className="btn icon"
      onClick={copy}
      title={copied ? "Copied" : (label ?? "Copy")}
      aria-label={label ?? "Copy"}
    >
      {copied ? <Check size={13} /> : <Copy size={13} />}
    </button>
  );
}

/** Focused modal dialog — used for wizards and destructive confirms.
 * Esc and the backdrop close it (unless `locked`, e.g. mid-creation).
 *
 * Motion (calm pass): entrance 300ms cubic-bezier(0.16,1,0.3,1), exit
 * 200ms ease-in. The exit keeps the dialog mounted for 200ms with the
 * LAST rendered title/children (cached in refs) so confirm dialogs
 * whose content derives from a nulled selection don't flash empty. */
export function Modal({
  open,
  onClose,
  title,
  width,
  locked,
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: ReactNode;
  width?: number;
  locked?: boolean;
  children: ReactNode;
}) {
  const [exiting, setExiting] = useState(false);
  const wasOpen = useRef(false);
  const titleRef = useRef<ReactNode>(null);
  const childrenRef = useRef<ReactNode>(null);
  if (open) {
    titleRef.current = title;
    childrenRef.current = children;
  }

  useEffect(() => {
    if (open) {
      wasOpen.current = true;
      setExiting(false);
      return;
    }
    if (!wasOpen.current) return;
    setExiting(true);
    const id = setTimeout(() => {
      wasOpen.current = false;
      setExiting(false);
    }, 200);
    return () => clearTimeout(id);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !locked) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, locked, onClose]);

  if (!open && !exiting) return null;
  return (
    <div
      className={`modal-backdrop ${open ? "" : "closing"}`}
      onMouseDown={(e) => {
        if (open && e.target === e.currentTarget && !locked) onClose();
      }}
    >
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        style={width ? { maxWidth: width } : undefined}
      >
        <div className="modal-head">
          <div className="modal-title">{open ? title : titleRef.current}</div>
          {!locked && (
            <button className="btn icon" onClick={onClose} aria-label="Close">
              <X size={14} />
            </button>
          )}
        </div>
        <div className="modal-body">{open ? children : childrenRef.current}</div>
      </div>
    </div>
  );
}

/** Destructive confirmation: the user must type `phrase` to enable the
 * confirm button. */
export function DangerConfirm({
  open,
  title,
  phrase,
  busy,
  error,
  onCancel,
  onConfirm,
  children,
}: {
  open: boolean;
  title: string;
  phrase: string;
  busy?: boolean;
  error?: string | null;
  onCancel: () => void;
  onConfirm: () => void;
  children: ReactNode;
}) {
  const [typed, setTyped] = useState("");
  useEffect(() => {
    if (!open) setTyped("");
  }, [open]);
  return (
    <Modal open={open} onClose={onCancel} title={title} width={420}>
      {children}
      <div className="field-group" style={{ marginTop: 14 }}>
        <label className="field">
          Type <span className="mono">{phrase}</span> to confirm
        </label>
        <input
          className="input mono"
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          placeholder={phrase}
          autoFocus
        />
      </div>
      {error && <div className="error-box">{error}</div>}
      <div className="modal-foot">
        <button className="btn" onClick={onCancel} disabled={busy}>
          Cancel
        </button>
        <button
          className="btn danger solid"
          disabled={busy || typed !== phrase}
          onClick={onConfirm}
        >
          {busy ? "Working…" : title}
        </button>
      </div>
    </Modal>
  );
}

/** Animated on/off switch — used for per-client active toggles.
 * Stops row-click propagation so it works inside clickable rows. */
export function Switch({
  on,
  disabled,
  title,
  onToggle,
}: {
  on: boolean;
  disabled?: boolean;
  title?: string;
  onToggle: (next: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      className={`switch ${on ? "on" : ""}`}
      disabled={disabled}
      title={title}
      onClick={(e) => {
        e.stopPropagation();
        onToggle(!on);
      }}
    />
  );
}

export interface MenuEntry {
  label: string;
  icon?: ReactNode;
  danger?: boolean;
  disabled?: boolean;
  /** Tooltip — shown on the item (use for disabled-with-reason). */
  hint?: string;
  onClick: () => void;
}

/** Kebab (⋯) dropdown menu. Closes on outside click, Esc and item
 * selection. Pure CSS positioning — anchored to the trigger. */
export function RowMenu({
  entries,
  label,
}: {
  entries: (MenuEntry | "sep")[];
  label?: string;
}) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLSpanElement | null>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey, true);
    };
  }, [open]);

  return (
    <span className="menu-wrap" ref={wrapRef}>
      <button
        type="button"
        className={`btn icon ${open ? "menu-open" : ""}`}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={label ?? "More actions"}
        title={label ?? "More actions"}
        onClick={(e) => {
          e.stopPropagation();
          setOpen((o) => !o);
        }}
      >
        <MoreHorizontal size={14} />
      </button>
      {open && (
        <div className="menu" role="menu" onClick={(e) => e.stopPropagation()}>
          {entries.map((entry, i) =>
            entry === "sep" ? (
              <div key={`sep-${i}`} className="menu-sep" />
            ) : (
              <button
                key={entry.label}
                role="menuitem"
                className={entry.danger ? "danger" : ""}
                disabled={entry.disabled}
                title={entry.hint}
                onClick={() => {
                  setOpen(false);
                  entry.onClick();
                }}
              >
                {entry.icon}
                {entry.label}
              </button>
            ),
          )}
        </div>
      )}
    </span>
  );
}

/** Click-to-edit text. Pencil affordance appears on hover; Enter or
 * blur commits (only when changed), Esc cancels. */
export function InlineEdit({
  value,
  placeholder,
  onCommit,
  busy,
}: {
  value: string | null;
  placeholder: string;
  onCommit: (next: string) => void;
  busy?: boolean;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");

  const start = (e: ReactMouseEvent) => {
    e.stopPropagation();
    setDraft(value ?? "");
    setEditing(true);
  };

  const commit = () => {
    setEditing(false);
    const next = draft.trim();
    if (next !== (value ?? "")) onCommit(next);
  };

  if (editing) {
    return (
      <input
        className="input inline-edit-input"
        value={draft}
        autoFocus
        placeholder={placeholder}
        onClick={(e) => e.stopPropagation()}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
          if (e.key === "Escape") {
            e.stopPropagation();
            setEditing(false);
          }
        }}
      />
    );
  }

  return (
    <span className="inline-edit" onDoubleClick={start}>
      <span className={value ? "" : "inline-edit-empty"}>{value ?? placeholder}</span>
      <button
        type="button"
        className="pencil"
        aria-label="Rename"
        title="Rename"
        disabled={busy}
        onClick={start}
      >
        <Pencil size={11} />
      </button>
    </span>
  );
}

/** Numeric value that flashes green/red when it ticks up/down — the
 * "alive" feel on PnL and balances without charts. */
export function Ticker({
  value,
  format,
  className,
}: {
  value: number | null;
  format: (n: number) => ReactNode;
  className?: string;
}) {
  const [tick, setTick] = useState("");
  const prev = useRef<number | null>(null);

  useEffect(() => {
    const p = prev.current;
    prev.current = value;
    if (value != null && p != null && value !== p) {
      setTick(value > p ? "tick-up" : "tick-down");
      const id = setTimeout(() => setTick(""), 900);
      return () => clearTimeout(id);
    }
  }, [value]);

  if (value == null) return <span className={className}>—</span>;
  return <span className={`${className ?? ""} ${tick}`}>{format(value)}</span>;
}

/** Segmented control — chain filter on the fleet table. */
export function Segmented<T extends string>({
  options,
  value,
  onChange,
}: {
  options: { value: T; label: ReactNode }[];
  value: T;
  onChange: (v: T) => void;
}) {
  return (
    <div className="segmented" role="tablist">
      {options.map((o) => (
        <button
          key={o.value}
          role="tab"
          aria-selected={value === o.value}
          className={value === o.value ? "active" : ""}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** KPI cell for the `.kpi-strip` readout band — THE summary-number
 * idiom (calm pass): mono label, tabular-nums value, optional sub. */
export function Kpi({
  label,
  value,
  sub,
  tone,
  loading,
}: {
  label: string;
  value: ReactNode;
  sub?: ReactNode;
  tone?: "pos" | "neg";
  loading?: boolean;
}) {
  return (
    <div className="kpi">
      <div className="kpi-label">{label}</div>
      {loading ? (
        <span className="skeleton" style={{ width: "55%", height: 17, marginTop: 2 }} />
      ) : (
        <div className={`kpi-value ${tone ?? ""}`}>{value}</div>
      )}
      {sub && !loading && <div className="kpi-sub">{sub}</div>}
    </div>
  );
}

/** Compact designed empty state for tables / lists inside cards —
 * icon, one line, mono hint. Page-level zero states use EmptyHero. */
export function EmptyState({
  icon,
  title,
  hint,
}: {
  icon: ReactNode;
  title: string;
  hint?: string;
}) {
  return (
    <div className="empty-state">
      <span className="empty-state-icon">{icon}</span>
      <span className="empty-state-title">{title}</span>
      {hint && <span className="empty-state-hint">{hint}</span>}
    </div>
  );
}

/** Hero empty state with one primary action — no bare tables. */
export function EmptyHero({
  icon,
  title,
  desc,
  action,
}: {
  icon: ReactNode;
  title: string;
  desc: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="empty-hero">
      <div className="empty-hero-icon">{icon}</div>
      <div className="empty-hero-title">{title}</div>
      <p className="empty-hero-desc">{desc}</p>
      {action}
    </div>
  );
}
