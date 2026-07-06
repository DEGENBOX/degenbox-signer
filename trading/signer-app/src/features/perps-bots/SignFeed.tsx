// Perpetuals signing feed — the executor's journal in the Account
// tab's terminal idiom (features/account/ActivityTerminal.tsx),
// narrowed to chain === "hl" and re-gridded for the perps payload:
// the HL daemon tags every RecentSign with kind = "{payload_kind}
// {asset}" (order BTC / closePosition ETH / …), identifier = cloid,
// status from the submit result (ok / filled / failed / …).
//
//   [18:42:11] SIGN order BTC          0x3f9a…c21b  ·  ok
//   [18:44:53] SIGN closePosition ETH  0x9c01…77aa  ·  failed
//
// Copied (not imported) from ActivityTerminal because the chain
// filter + line grammar differ; the terminal SKIN (.terminal/.tl-*)
// is reused unchanged from the account feature's stylesheet.

import { useEffect, useRef, useState } from "react";
import { ipc, type RecentSign } from "./ipc";
import { shortAddr } from "../../components/ui";
import "../account/account.css";

const OK_STATUSES = new Set(["ok", "filled", "submitted", "executed"]);

function statusClass(status: string): string {
  const s = status.toLowerCase();
  if (OK_STATUSES.has(s)) return "tl-ok";
  if (s.includes("fail") || s.includes("error") || s.includes("reject")) return "tl-fail";
  return "tl-dim"; // skipped / paused / pending …
}

function fmtTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "--:--:--";
  return d.toLocaleTimeString("en-GB", { hour12: false });
}

export function SignFeed() {
  const [signs, setSigns] = useState<RecentSign[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const haveData = useRef(false);

  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const s = await ipc.recentSigns();
        if (alive) {
          haveData.current = true;
          setSigns(s);
          setErr(null);
        }
      } catch (e) {
        // Surface the error only before the first successful load —
        // afterwards keep showing the last good snapshot.
        if (alive && !haveData.current) setErr(String(e));
      }
    };
    load();
    const id = setInterval(load, 3000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // Ring buffer is newest-first; a terminal tails oldest → newest.
  const rows = signs ? signs.filter((r) => r.chain === "hl").reverse() : null;

  // Auto-scroll: stay pinned to the bottom until the user scrolls up;
  // re-pin when they return near the bottom.
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const pinned = useRef(true);
  const onScroll = () => {
    const el = bodyRef.current;
    if (!el) return;
    pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
  };
  useEffect(() => {
    const el = bodyRef.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  }, [rows?.length, err]);

  return (
    <div className="terminal" role="log" aria-label="Perpetuals signing feed">
      <div className="terminal-head">
        <span className="hud-label brackets">Signing feed</span>
        <span className="spacer" />
        <span className="hud-label">
          {rows ? `${rows.length} sign${rows.length === 1 ? "" : "s"}` : "…"}
        </span>
        <span
          className={`status-dot ${err ? "red" : "green pulse"}`}
          title={err ? "feed unavailable" : "live (polls every 3s)"}
        />
      </div>
      <div className="terminal-body" ref={bodyRef} onScroll={onScroll}>
        <span className="tl-line tl-dim">degenbox@signer:~ $ tail -f perps.journal</span>
        {err ? (
          <span className="tl-line tl-fail">!! journal unavailable: {err}</span>
        ) : rows === null ? (
          <span className="tl-line tl-dim">· opening journal …</span>
        ) : rows.length === 0 ? (
          <span className="tl-line tl-dim">
            · journal empty. Orders queued by DegenBox appear here the moment this
            device signs
          </span>
        ) : (
          rows.map((r, i) => <FeedLine key={`${r.at}-${i}`} sign={r} />)
        )}
        <span className="tl-line">
          <span className="tl-cursor" aria-hidden />
        </span>
      </div>
    </div>
  );
}

function FeedLine({ sign }: { sign: RecentSign }) {
  return (
    <span className="tl-line">
      <span className="tl-time">[{fmtTime(sign.at)}]</span>{" "}
      <span className="tl-tag">SIGN</span> <span>{sign.kind.padEnd(20)}</span>
      <span className="tl-id">{shortAddr(sign.identifier, 6, 4).padEnd(15)}</span>
      <span className="tl-dim">· </span>
      <span className={statusClass(sign.status)}>{sign.status}</span>
    </span>
  );
}
