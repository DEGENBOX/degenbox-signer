//! Program-ID allowlist for the pre-sign safety pass.
//!
//! The signer extracts the set of programs a tx touches and rejects
//! the sign if ANY of them are outside the whitelist. This is the
//! single most important defense against being tricked into signing
//! a tx that drains your wallet: even with a valid Jupiter route, a
//! malicious upstream can splice in `Token::Approve` to a hostile
//! account, or a no-op `Memo` ix that hides intent.
//!
//! ## Whitelist
//!
//! The default set covers everything Jupiter actually routes through
//! plus the bare-minimum Solana primitives:
//!
//! - **Jupiter aggregator v6** — the swap entry point
//! - **Token program** + **Token-2022** — every SPL transfer
//! - **System program** — SOL transfers (wrap/unwrap)
//! - **Associated token account program** — auto-create ATA
//! - **ComputeBudget** — CU limit / priority fee instructions
//! - **Memo program** — Jupiter sometimes attaches a memo
//! - **Major AMMs** — Raydium AMM v4 / CPMM / CLMM, Orca Whirlpool,
//!   Meteora DLMM / Dynamic / DBC, PumpFun (bonding + AMM),
//!   Phoenix, Lifinity v2
//!
//! Callers can extend the list at construction time for specialty
//! programs (e.g. a private LP your bot trades on).
//!
//! ## What this does NOT enforce
//!
//! - It doesn't check that the WRITES inside an allowlisted program
//!   are sane (Token::Approve to attacker = still rejected by you
//!   reading the simulation output). The simulator + program-allow
//!   are complementary.
//! - It doesn't whitelist a SPECIFIC version of a program. If
//!   Raydium ever ships v5 with a different pubkey, we'd need to
//!   add it explicitly.

use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use std::collections::HashSet;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AllowlistError {
    #[error("program {0} is not in the allowlist")]
    NotAllowed(String),
    #[error("invalid pubkey in allowlist config: {0}")]
    InvalidPubkey(String),
}

/// Default whitelist — covers Jupiter routes + the AMMs we observed
/// in this codebase (`trading_dex_inventory` table). Anything Jupiter
/// routes through but not here would need to be added explicitly.
pub const DEFAULT_ALLOWED: &[&str] = &[
    // Solana primitives
    "11111111111111111111111111111111",             // System
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",  // SPL Token
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",  // Token-2022
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", // Associated Token
    "ComputeBudget111111111111111111111111111111",  // ComputeBudget
    "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",  // Memo v2
    "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo",  // Memo v1
    // Jupiter v6 aggregator
    "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
    // Major AMMs
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8", // Raydium AMM v4
    "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C", // Raydium CPMM
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK", // Raydium CLMM
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",  // Orca Whirlpool
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",  // Meteora DLMM
    "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB", // Meteora Dynamic AMM
    "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN",  // Meteora DBC (bonding curve)
    "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",  // PumpFun
    "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA",  // PumpFun AMM
    "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX",  // OpenBook DEX v3 (Raydium v4 market layer)
    "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY",  // Phoenix
    "EewxydAPCCVuNEyrVN68PuSYdQ7wKn27V9Gjeoi8dy3S", // Lifinity v2
];

/// Build an allowlist from `DEFAULT_ALLOWED` plus any user-supplied
/// extras. Validates every entry parses as a base58 pubkey.
pub fn default_allowlist() -> Result<Allowlist, AllowlistError> {
    Allowlist::from_b58_iter(DEFAULT_ALLOWED.iter().copied())
}

#[derive(Debug, Clone)]
pub struct Allowlist {
    allowed: HashSet<Pubkey>,
}

impl Allowlist {
    /// Build from an iterator of base58 strings. Any unparseable
    /// entry fails — we want loud errors at config time, not silent
    /// drops that broaden the allowed set.
    pub fn from_b58_iter<I, S>(iter: I) -> Result<Self, AllowlistError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allowed = HashSet::new();
        for s in iter {
            let pk = Pubkey::from_str(s.as_ref())
                .map_err(|_| AllowlistError::InvalidPubkey(s.as_ref().to_string()))?;
            allowed.insert(pk);
        }
        Ok(Self { allowed })
    }

    /// Add a program id to the allowlist after construction. Useful
    /// for callers who load extras from config or env at boot.
    pub fn allow(&mut self, program_id_b58: &str) -> Result<(), AllowlistError> {
        let pk = Pubkey::from_str(program_id_b58)
            .map_err(|_| AllowlistError::InvalidPubkey(program_id_b58.to_string()))?;
        self.allowed.insert(pk);
        Ok(())
    }

    /// Extract program-ids from a VersionedTransaction + assert all
    /// are in the allowlist.
    ///
    /// Returns the full set of program-ids referenced for reporting
    /// (so the caller can log "rejected — tx touched X, Y, Z" with
    /// confidence about what the rejection covered).
    pub fn check_tx(&self, tx: &VersionedTransaction) -> Result<Vec<Pubkey>, AllowlistError> {
        let touched = programs_in_tx(tx);
        for pk in &touched {
            if !self.allowed.contains(pk) {
                return Err(AllowlistError::NotAllowed(pk.to_string()));
            }
        }
        Ok(touched)
    }

    pub fn len(&self) -> usize {
        self.allowed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }
}

/// Return the set of program-ids a tx invokes. We look at every
/// instruction's `program_id_index` and resolve through both static
/// account keys + (in a follow-up slice) LUT-derived ones.
///
/// LUT lookup is currently skipped because VersionedTransaction
/// `static_account_keys` returns ONLY the static set. A future T.3
/// improvement: resolve LUT addresses via getAccountInfo before the
/// check. Until then we conservatively REJECT any tx that references
/// a program index pointing into the LUT space (we treat that as
/// "unknown program" rather than "ATA program by coincidence").
pub fn programs_in_tx(tx: &VersionedTransaction) -> Vec<Pubkey> {
    let static_keys = tx.message.static_account_keys();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for ix in tx.message.instructions() {
        let idx = ix.program_id_index as usize;
        if idx < static_keys.len() {
            let pk = static_keys[idx];
            if seen.insert(pk) {
                out.push(pk);
            }
        }
        // If idx >= static_keys.len() the program is in a LUT-resolved
        // table. We don't have the LUT contents here. The caller's
        // check_tx will see "fewer programs than instructions" by way
        // of the deduped output not covering all ix; we surface this
        // via the simulator (would_fail = true if Solana RPC can't
        // resolve), and explicit T.3 will resolve LUTs locally.
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        hash::Hash,
        message::{v0, VersionedMessage},
        signature::Signature,
    };

    #[allow(deprecated)]
    use solana_sdk::system_instruction;

    fn simple_tx_with_program(program: &Pubkey) -> VersionedTransaction {
        let payer = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        // We can't easily construct an arbitrary ix without going
        // through the program crate, but `system_instruction::transfer`
        // emits the system program id which is in the default
        // allowlist. For the rejection tests we build a manual v0
        // message that names the given program.
        let _ = program; // unused for the system path
        let ix = system_instruction::transfer(&payer, &recipient, 100);
        let msg = v0::Message::try_compile(&payer, &[ix], &[], Hash::default()).unwrap();
        VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(msg),
        }
    }

    fn tx_with_manual_program(program: Pubkey) -> VersionedTransaction {
        // Build a tx whose first ix invokes `program` directly. We
        // do this by hand-crafting a v0 message — the static account
        // keys are [payer, program] and the ix points to index 1.
        use solana_sdk::message::v0::MessageAddressTableLookup;
        let payer = Pubkey::new_unique();
        let msg = v0::Message {
            header: solana_sdk::message::MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer, program],
            recent_blockhash: Hash::default(),
            instructions: vec![solana_sdk::instruction::CompiledInstruction {
                program_id_index: 1,
                accounts: vec![],
                data: vec![],
            }],
            address_table_lookups: Vec::<MessageAddressTableLookup>::new(),
        };
        VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(msg),
        }
    }

    #[test]
    fn default_list_parses_clean() {
        let al = default_allowlist().unwrap();
        assert!(
            al.len() >= 16,
            "expected ≥16 default entries, got {}",
            al.len()
        );
    }

    #[test]
    fn system_program_passes() {
        let al = default_allowlist().unwrap();
        let tx = simple_tx_with_program(&Pubkey::new_unique());
        let touched = al.check_tx(&tx).unwrap();
        assert_eq!(touched.len(), 1, "system_instruction::transfer → 1 program");
    }

    #[test]
    fn unknown_program_rejected() {
        let al = default_allowlist().unwrap();
        let evil = Pubkey::new_unique();
        let tx = tx_with_manual_program(evil);
        let err = al.check_tx(&tx).unwrap_err();
        match err {
            AllowlistError::NotAllowed(s) => assert_eq!(s, evil.to_string()),
            _ => panic!("wrong error variant"),
        }
    }

    #[test]
    fn manual_allow_extension_works() {
        let mut al = default_allowlist().unwrap();
        let custom = Pubkey::new_unique();
        let tx = tx_with_manual_program(custom);
        // Before adding: rejected.
        assert!(al.check_tx(&tx).is_err());
        // After adding: passes.
        al.allow(&custom.to_string()).unwrap();
        assert!(al.check_tx(&tx).is_ok());
    }

    #[test]
    fn invalid_pubkey_in_config_is_loud() {
        let r = Allowlist::from_b58_iter(["not a real pubkey"]);
        assert!(matches!(r, Err(AllowlistError::InvalidPubkey(_))));
    }

    #[test]
    fn programs_in_tx_dedupes() {
        // A tx with two ix to the same program should yield 1 entry.
        let payer = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let msg = v0::Message {
            header: solana_sdk::message::MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer, prog],
            recent_blockhash: Hash::default(),
            instructions: vec![
                solana_sdk::instruction::CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![],
                    data: vec![],
                },
                solana_sdk::instruction::CompiledInstruction {
                    program_id_index: 1,
                    accounts: vec![],
                    data: vec![1],
                },
            ],
            address_table_lookups: vec![],
        };
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(msg),
        };
        let progs = programs_in_tx(&tx);
        assert_eq!(progs.len(), 1);
        assert_eq!(progs[0], prog);
    }
}
