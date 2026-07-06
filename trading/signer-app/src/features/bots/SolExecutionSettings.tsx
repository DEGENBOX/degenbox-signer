// Solana execution parameters — how this device builds and lands its
// transactions (RPC, slippage, tip, submit path). Rendered in the Sol
// BOTS tab.
//
// v0.3.0 slice 9: the old "Copy-buy budget" card (per-unlock session
// budget + per-token cap) is GONE — slice 8 retired those knobs; copy
// spend is now capped per copy config on the server and enforced on
// every buy event. The RPC override stays; its placeholder is the real
// zero-config default (the gateway's token-gated RPC proxy).

import { useEffect, useState } from "react";
import { ipc } from "../../ipc";
import { Segmented } from "../../components/ui";

type SubmitMode = "falcon_jito" | "quic" | "tpu";
const SUBMIT_MODES: SubmitMode[] = ["falcon_jito", "quic", "tpu"];

export function SolExecutionSettings() {
  const [rpcUrl, setRpcUrl] = useState("");
  const [rpcDefault, setRpcDefault] = useState("https://api.mainnet-beta.solana.com");
  const [slippage, setSlippage] = useState("");
  const [tip, setTip] = useState("");
  const [mode, setMode] = useState<SubmitMode>("falcon_jito");
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    ipc
      .solExecConfigGet()
      .then((c) => {
        setRpcUrl(c.rpc_url_override ?? "");
        setRpcDefault(c.rpc_url_default);
        setSlippage(String(c.slippage_bps));
        setTip(String(c.tip_lamports));
        if ((SUBMIT_MODES as string[]).includes(c.submit_mode)) {
          setMode(c.submit_mode as SubmitMode);
        }
        setLoaded(true);
      })
      .catch((e) => {
        setErr(String(e));
        setLoaded(true);
      });
  }, []);

  const save = async () => {
    setBusy(true);
    setErr(null);
    setMsg(null);
    const bps = slippage.trim() === "" ? null : Number(slippage);
    const tipL = tip.trim() === "" ? null : Number(tip);
    if (
      (bps != null && (!Number.isInteger(bps) || bps < 1 || bps > 10000)) ||
      (tipL != null && (!Number.isInteger(tipL) || tipL < 0))
    ) {
      setErr("Slippage needs to be 1–10000 bps and the tip a whole number of lamports.");
      setBusy(false);
      return;
    }
    try {
      await ipc.solExecParamsSet({
        rpc_url: rpcUrl.trim() === "" ? null : rpcUrl.trim(),
        slippage_bps: bps,
        tip_lamports: tipL,
        submit_mode: mode,
      });
      setMsg("Saved. The Solana engine restarted with the new settings.");
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const usingProxyDefault = rpcDefault.includes("/api/rpc/solana");

  return (
    <div className="card">
      <div className="card-title">
        Execution
        <span className="right hud-label">Solana</span>
      </div>
      <p>
        How this device builds and sends its Solana transactions. Every field has a
        sensible default; leave it empty unless you know you want something else.
      </p>
      <div className="field-group">
        <label className="field" htmlFor="sol-exec-rpc">
          RPC endpoint (optional)
        </label>
        <input
          id="sol-exec-rpc"
          className="input mono"
          value={rpcUrl}
          onChange={(e) => setRpcUrl(e.target.value)}
          placeholder={usingProxyDefault ? "DegenBox in-house RPC (default)" : rpcDefault}
        />
        <p style={{ fontSize: 11.5, color: "var(--fg-faint)", margin: "4px 0 0" }}>
          {usingProxyDefault
            ? "Leave empty and the app uses DegenBox's own RPC. It works out of the box. Paste your own endpoint (Helius, Triton) if you want the lowest possible latency."
            : "The public Solana RPC is heavily rate-limited. Link your DegenBox account to use our RPC automatically, or paste your own endpoint (Helius, Triton, …)."}
        </p>
      </div>
      <div className="field-group">
        <label className="field" htmlFor="sol-exec-slippage">
          Sell slippage in basis points (100 = 1%)
        </label>
        <input
          id="sol-exec-slippage"
          className="input mono"
          inputMode="numeric"
          value={slippage}
          onChange={(e) => setSlippage(e.target.value)}
          placeholder="100"
        />
      </div>
      <div className="field-group">
        <label className="field" htmlFor="sol-exec-tip">
          Priority tip in lamports (1000000 = 0.001 SOL)
        </label>
        <input
          id="sol-exec-tip"
          className="input mono"
          inputMode="numeric"
          value={tip}
          onChange={(e) => setTip(e.target.value)}
          placeholder="1000000"
        />
      </div>
      <div className="field-group">
        <label className="field">Submit path</label>
        <Segmented<SubmitMode>
          value={mode}
          onChange={setMode}
          options={[
            { value: "falcon_jito", label: "Falcon + Jito" },
            { value: "quic", label: "QUIC" },
            { value: "tpu", label: "TPU" },
          ]}
        />
      </div>
      <p style={{ fontSize: 11.5, color: "var(--fg-faint)" }}>
        Looking for the old copy-buy budget? It moved: copy spending is now capped per
        copy config (open a leader wallet's settings) and enforced on every buy.
      </p>
      {err && <div className="error-box">{err}</div>}
      {msg && <p style={{ marginBottom: 8 }}>{msg}</p>}
      <div className="btn-row">
        <button className="btn primary" disabled={busy || !loaded} onClick={save}>
          Save execution settings
        </button>
      </div>
    </div>
  );
}
