// App-wide crash guard. A signer must NEVER silently white-screen: any
// render exception below this boundary is caught and rendered as a
// legible error card (message + component stack + reload) instead of an
// unmounted, blank webview. Two scopes are used (App.tsx):
//
//   · a ROOT boundary around the whole shell — last-resort catch.
//   · a per-tab boundary around each tab body, keyed by (mode, tab), so
//     a crash in one trading surface leaves the header, mode switch,
//     kill-switch, Account and the other tab still reachable.
//
// React error boundaries only reset on remount; the per-tab boundary's
// `key` (mode-tab) already remounts on any switch, so navigating away
// and back clears a caught error. The root boundary offers an explicit
// Reload.

import { Component, type ErrorInfo, type ReactNode } from "react";
import { RefreshCw } from "lucide-react";

interface Props {
  children: ReactNode;
  /** Short context for the header line ("Solana · Live", "app"). */
  label?: string;
  /** Root boundary shows a full reload button; tab boundaries don't. */
  root?: boolean;
}

interface State {
  error: Error | null;
  info: ErrorInfo | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, info: null };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ info });
    // Keep the raw error in the console/devtools + tauri stdout so the
    // operator (and logs) still capture the full stack.
    // eslint-disable-next-line no-console
    console.error(
      `[ErrorBoundary${this.props.label ? ` ${this.props.label}` : ""}]`,
      error,
      info?.componentStack,
    );
  }

  render() {
    const { error, info } = this.state;
    if (!error) return this.props.children;

    return (
      <div className={`crash-guard ${this.props.root ? "crash-root" : ""}`} role="alert">
        <div className="crash-card corners">
          <div className="crash-head">
            <span className="hud-label brackets">
              {this.props.label ? `${this.props.label} crashed` : "Something crashed"}
            </span>
          </div>
          <p className="crash-lead">
            A screen hit an unexpected error and was isolated so the rest of the app keeps
            working. Signing and auto-execution are unaffected. This is a display fault.
          </p>
          <div className="error-box crash-msg">{String(error.message || error)}</div>
          {(error.stack || info?.componentStack) && (
            <details className="crash-details">
              <summary>Technical detail</summary>
              <pre className="crash-stack">
                {error.stack || ""}
                {info?.componentStack ? `\n--- component stack ---${info.componentStack}` : ""}
              </pre>
            </details>
          )}
          <div className="crash-actions">
            <button
              className="btn"
              onClick={() => this.setState({ error: null, info: null })}
            >
              <RefreshCw size={13} /> Retry this screen
            </button>
            {this.props.root && (
              <button className="btn primary" onClick={() => window.location.reload()}>
                Reload app
              </button>
            )}
          </div>
        </div>
      </div>
    );
  }
}
