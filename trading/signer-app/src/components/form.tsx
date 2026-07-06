// Shared inline-form primitives (iteration 3 rebuild). The platform
// form language: grouped sections with clear headings, labels ABOVE
// full-width controls, numbers in mono, high contrast — no boxed
// inline-label mini-inputs ("SCALE 1 ×"). Benchmark: the web copytrade
// WalletSettingsSheet / configForm. Used by the inline config editors
// that replace the old config modals.

import type { ReactNode } from "react";

/** A labelled full-width field row. */
export function Field({
  label,
  hint,
  htmlFor,
  children,
}: {
  label: string;
  hint?: ReactNode;
  htmlFor?: string;
  children: ReactNode;
}) {
  return (
    <div className="field-row">
      <label className="field-row-label" htmlFor={htmlFor}>
        {label}
      </label>
      {children}
      {hint && <p className="field-row-hint">{hint}</p>}
    </div>
  );
}

/** Full-width number input (mono). Empty string = cleared. */
export function NumField({
  id,
  label,
  unit,
  hint,
  value,
  onChange,
  placeholder,
  inputMode = "decimal",
}: {
  id?: string;
  label: string;
  unit?: string;
  hint?: ReactNode;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  inputMode?: "decimal" | "numeric";
}) {
  return (
    <Field label={unit ? `${label} (${unit})` : label} hint={hint} htmlFor={id}>
      <input
        id={id}
        className="input mono"
        inputMode={inputMode}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
      />
    </Field>
  );
}

/** Full-width text input. */
export function TextField({
  id,
  label,
  hint,
  value,
  onChange,
  placeholder,
  mono = false,
  autoFocus = false,
  type = "text",
}: {
  id?: string;
  label: string;
  hint?: ReactNode;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  mono?: boolean;
  autoFocus?: boolean;
  type?: "text" | "password";
}) {
  return (
    <Field label={label} hint={hint} htmlFor={id}>
      <input
        id={id}
        className={`input ${mono ? "mono" : ""}`}
        type={type}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        autoFocus={autoFocus}
      />
    </Field>
  );
}

/** A checkbox line (label to the right). */
export function CheckField({
  label,
  checked,
  onChange,
  hint,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
  hint?: ReactNode;
}) {
  return (
    <div className="field-row">
      <label className="check-line">
        <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
        {label}
      </label>
      {hint && <p className="field-row-hint">{hint}</p>}
    </div>
  );
}

/** A grouped subsection with a heading inside an inline editor. */
export function FormGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="form-group">
      <div className="form-group-head">{title}</div>
      <div className="form-group-body">{children}</div>
    </div>
  );
}

/** The inline editor shell — a full-width expanding pane (never a
 *  modal). Header + body + sticky footer.
 *
 *  `anchored` — render attached to the row/card that opened it (spec
 *  §D): connector notch + accent edge instead of a free-floating box.
 *  `columns={2}` — two-column body on wide windows (spec §F). */
export function InlineEditor({
  title,
  subtitle,
  onClose,
  footer,
  children,
  anchored = false,
  columns = 1,
}: {
  title: string;
  subtitle?: ReactNode;
  onClose: () => void;
  footer: ReactNode;
  children: ReactNode;
  anchored?: boolean;
  columns?: 1 | 2;
}) {
  return (
    <section
      className={`inline-editor ${anchored ? "anchored" : ""}`}
      aria-label={title}
    >
      <header className="inline-editor-head">
        <div>
          <div className="inline-editor-title">{title}</div>
          {subtitle && <div className="inline-editor-sub">{subtitle}</div>}
        </div>
        <button className="btn sm" onClick={onClose} title="Close without saving">
          Close
        </button>
      </header>
      <div className={`inline-editor-body ${columns === 2 ? "two-col" : ""}`}>
        {children}
      </div>
      <footer className="inline-editor-foot">{footer}</footer>
    </section>
  );
}
