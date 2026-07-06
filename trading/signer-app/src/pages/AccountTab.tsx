// Account (iteration 3) — small and boring by design. Device-wide only:
//   01 / ACCOUNT  — Discord identity + gateway access
//   02 / SECURITY — vault lock + passphrase cache, multi-wallet keys
//   03 / APP      — version / updates / open log file
// Everything module-specific left this surface: the signer journal, the
// paper-mode toggle, Solana execution + copy-budget forms and signer
// pairing/2FA all moved into the module tabs (see feat report slice 3).

import type { StatusReport } from "../ipc";
import { ShellSection } from "../components/ShellSection";
import { AccountSection } from "../features/account/AccountSection";
import { SecuritySection } from "../features/account/SecuritySection";
import { AppSection } from "../features/account/AppSection";
import "../features/account/account.css";

interface Props {
  status: StatusReport | null;
  onReload: () => void;
  /** Rendered inside the account overlay — suppress the page title. */
  embedded?: boolean;
}

export function AccountTab({ status, onReload, embedded }: Props) {
  return (
    <>
      {!embedded && (
        <>
          <h1>Account</h1>
          <p className="page-sub">
            Identity, security and app maintenance for this device, shared across both
            modes.
          </p>
        </>
      )}
      <ShellSection num="01" title="Account">
        <AccountSection />
      </ShellSection>
      <ShellSection num="02" title="Security">
        <SecuritySection status={status} onReload={onReload} />
      </ShellSection>
      <ShellSection num="03" title="App">
        <AppSection />
      </ShellSection>
    </>
  );
}
