// Solana → BOTS (slice-2 IA, reworked for operator R4). The tab now
// answers "what is running right now?" FIRST: section 01 lists every
// live auto-buy session and copy follow with a status dot and a jump
// to its controls. Everything below is configuration — wallets +
// sessions, scanner presets (execution + assignment), leader wallets,
// execution settings. Data ownership lives here so Running now and the
// config sections read the same polls (fleet 10 s, copy trade 15 s).

import { ShellSection } from "../components/ShellSection";
import { Bots } from "./Bots";
import { useFleet } from "../features/bots/useFleet";
import {
  COPY_ANCHOR,
  RunningNow,
  WALLETS_ANCHOR,
} from "../features/bots/RunningNow";
import { useCopyTrade } from "../features/presets/useCopyTrade";
import { ScannerPresetsSection } from "../features/presets/ScannerPresetsSection";
import { CopyTradeSection } from "../features/presets/CopyTradeSection";
import { SolExecutionSettings } from "../features/bots/SolExecutionSettings";
import type { StatusReport } from "../ipc";

export function SolBotsTab({ status }: { status: StatusReport | null }) {
  const fleet = useFleet();
  const copy = useCopyTrade();

  const liveCount =
    fleet.sessions === null && copy.rows === null
      ? "…"
      : (fleet.sessions ?? []).filter((s) => s.enabled).length +
        (copy.rows ?? []).filter((c) => c.enabled).length;

  return (
    <>
      <ShellSection num="01" title="Running now" count={liveCount}>
        <RunningNow
          sessions={fleet.sessions}
          clients={fleet.clients}
          device={fleet.device}
          copyRows={copy.rows}
          copyStats={copy.stats}
        />
      </ShellSection>

      <div className="bots-presets-intro">
        <span className="hud-label brackets">Configuration</span>
        <p className="page-sub" style={{ margin: "6px 0 0" }}>
          Everything below is setup: your wallets and their auto-buy sessions,
          scanner presets with their buy/sell rules, and leader wallets to copy.
          Whatever you switch on shows up under Running now.
        </p>
      </div>

      <ShellSection
        num="02"
        title="Wallets & sessions"
        id={WALLETS_ANCHOR}
        count={fleet.clients?.length ?? "…"}
      >
        <Bots status={status} embedded fleet={fleet} />
      </ShellSection>
      <ShellSection num="03" title="Scanner presets">
        <ScannerPresetsSection />
      </ShellSection>
      <ShellSection num="04" title="Copy trade" id={COPY_ANCHOR}>
        <CopyTradeSection copy={copy} />
      </ShellSection>
      <ShellSection num="05" title="Execution">
        <SolExecutionSettings />
      </ShellSection>
    </>
  );
}
