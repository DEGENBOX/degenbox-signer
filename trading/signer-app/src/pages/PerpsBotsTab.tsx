// Perpetuals → BOTS & SETTINGS (slice-2 IA, R5 sub-nav). The executor
// status stays pinned at the top ("is my bot live"); the configuration —
// strategy slot, caller execution settings, leader-wallet copy follows —
// moves behind a Segmented sub-nav so the tab reads as one clear surface
// instead of one long scroll (operator R5: "unübersichtlich, bessere
// Navigation innerhalb, umbenennen in Bots & Settings").

import { useState } from "react";
import { ShellSection } from "../components/ShellSection";
import { Segmented } from "../components/ui";
import { PerpsBots } from "./PerpsBots";
import { WalletStrategySlot } from "../features/perps-bots/WalletStrategySlot";
import { CallersSection } from "../features/perps-presets/CallersSection";
import { HlCopyTradeSection } from "../features/perps-presets/HlCopyTradeSection";
import type { HlStatus, StatusReport } from "../ipc";

interface Props {
  status: StatusReport | null;
  hl: HlStatus | null;
  onReload: () => void;
}

type Sub = "strategy" | "callers" | "copy";

export function PerpsBotsTab({ status, hl, onReload }: Props) {
  const [sub, setSub] = useState<Sub>("strategy");

  return (
    <>
      {/* Executor + pairing — pinned; the "is my bot live" answer. */}
      <PerpsBots status={status} hl={hl} onReload={onReload} embedded />

      <div className="bots-subnav">
        <Segmented<Sub>
          value={sub}
          onChange={setSub}
          options={[
            { value: "strategy", label: "Strategy" },
            { value: "callers", label: "Callers" },
            { value: "copy", label: "Copy trade" },
          ]}
        />
      </div>

      {sub === "strategy" &&
        (status?.hl_address ? (
          <ShellSection
            num="03"
            title="Strategy slot"
            hint="one strategy per wallet"
          >
            <WalletStrategySlot status={status} hl={hl} />
          </ShellSection>
        ) : (
          <ShellSection num="03" title="Strategy slot">
            <p className="page-sub">
              Pair your executor above to bind a strategy.
            </p>
          </ShellSection>
        ))}

      {sub === "callers" && (
        <ShellSection num="04" title="Callers">
          <CallersSection />
        </ShellSection>
      )}

      {sub === "copy" && (
        <ShellSection num="05" title="Copy trade">
          <HlCopyTradeSection />
        </ShellSection>
      )}
    </>
  );
}
