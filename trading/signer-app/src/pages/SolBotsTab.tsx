// Solana → BOTS & SETTINGS (slice-2 IA, R5 sub-nav). "Running now" stays
// pinned at the top (what is live right now); the configuration — wallets +
// sessions, scanner presets, leader wallets, execution settings — moves
// behind a Segmented sub-nav so the tab reads as one clear surface instead
// of one long scroll (operator R5: "unübersichtlich, bessere Navigation
// innerhalb, umbenennen in Bots & Settings"). A Manage jump from Running now
// switches to the owning sub-tab, then scrolls once the section has mounted.

import { useEffect, useRef, useState } from "react";
import { ShellSection } from "../components/ShellSection";
import { Segmented } from "../components/ui";
import { Bots } from "./Bots";
import { useFleet } from "../features/bots/useFleet";
import {
  BOTS_SUBTAB_EVENT,
  RunningNow,
  WALLETS_ANCHOR,
} from "../features/bots/RunningNow";
import { ScannerPresetsSection } from "../features/presets/ScannerPresetsSection";
import { SolExecutionSettings } from "../features/bots/SolExecutionSettings";
import type { StatusReport } from "../ipc";

type Sub = "wallets" | "presets" | "settings";

export function SolBotsTab({ status }: { status: StatusReport | null }) {
  const fleet = useFleet();
  const [sub, setSub] = useState<Sub>("wallets");
  // Anchor to scroll to once the target sub-tab's section has mounted.
  const pendingScroll = useRef<string | null>(null);

  // A Manage jump from Running now (or a wallet card) asks us to switch the
  // config sub-tab to the section that owns its anchor, then scroll to it.
  useEffect(() => {
    const onJump = (e: Event) => {
      const detail = (e as CustomEvent<{ sub: Sub; anchor: string }>).detail;
      if (!detail) return;
      pendingScroll.current = detail.anchor;
      setSub(detail.sub);
    };
    window.addEventListener(BOTS_SUBTAB_EVENT, onJump);
    return () => window.removeEventListener(BOTS_SUBTAB_EVENT, onJump);
  }, []);

  // After the sub-tab (and thus its section + anchor) renders, do the scroll.
  useEffect(() => {
    if (!pendingScroll.current) return;
    const id = pendingScroll.current;
    pendingScroll.current = null;
    requestAnimationFrame(() => {
      document.getElementById(id)?.scrollIntoView({ block: "start" });
    });
  }, [sub]);

  const liveCount =
    fleet.sessions === null
      ? "…"
      : (fleet.sessions ?? []).filter((s) => s.enabled).length;

  return (
    <>
      {/* Pinned — the "what is live right now" answer. */}
      <ShellSection num="01" title="Running now" count={liveCount}>
        <RunningNow
          sessions={fleet.sessions}
          clients={fleet.clients}
          device={fleet.device}
        />
      </ShellSection>

      <div className="bots-subnav">
        <Segmented<Sub>
          value={sub}
          onChange={setSub}
          options={[
            {
              value: "wallets",
              label: `Wallets${fleet.clients ? ` · ${fleet.clients.length}` : ""}`,
            },
            { value: "presets", label: "Presets" },
            { value: "settings", label: "Settings" },
          ]}
        />
      </div>

      {sub === "wallets" && (
        <ShellSection
          num="02"
          title="Wallets & sessions"
          id={WALLETS_ANCHOR}
          count={fleet.clients?.length ?? "…"}
        >
          <Bots status={status} embedded fleet={fleet} />
        </ShellSection>
      )}
      {sub === "presets" && (
        <ShellSection num="03" title="Scanner presets">
          <ScannerPresetsSection />
        </ShellSection>
      )}
      {sub === "settings" && (
        <ShellSection num="04" title="Execution">
          <SolExecutionSettings />
        </ShellSection>
      )}
    </>
  );
}
