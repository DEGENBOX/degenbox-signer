// Perpetuals → BOTS (slice-2 IA). The sole executor + the
// one-strategy-per-wallet slot model, with presets/templates folded in
// from the killed standalone Presets tab: caller execution settings and
// leader-wallet copy follows (the two strategy kinds a wallet slot can
// hold).

import { ShellSection } from "../components/ShellSection";
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

export function PerpsBotsTab({ status, hl, onReload }: Props) {
  return (
    <>
      <PerpsBots status={status} hl={hl} onReload={onReload} embedded />

      {status?.hl_address && (
        <ShellSection num="03" title="Strategy slot" hint="one strategy per wallet">
          <WalletStrategySlot status={status} hl={hl} />
        </ShellSection>
      )}

      <ShellSection num="04" title="Callers">
        <CallersSection />
      </ShellSection>
      <ShellSection num="05" title="Copy trade">
        <HlCopyTradeSection />
      </ShellSection>
    </>
  );
}
