//! Provider tip injection — the System `transfer` that makes a submit
//! provider actually accept / prioritize our transaction.
//!
//! ## Why this exists
//!
//! Our built swaps set a compute-unit *price* (priority fee) via
//! [`super::compute_budget::set_compute_unit_price`]. That is a
//! validator-side priority mechanism — it is NOT a provider tip.
//!
//!   * **Falcon** REQUIRES a System `transfer` of ≥ 0.001 SOL
//!     (`1_000_000` lamports) to one of its tip addresses INSIDE the
//!     submitted transaction, or it rejects the tx outright.
//!   * **Jito** needs a `transfer` to a Jito tip account to actually
//!     prioritize the bundle; without it the tx lands via normal RPC
//!     with no MEV protection.
//!
//! The gateway submits already-signed bytes, so the tip cannot be added
//! server-side — it has to be part of the instruction list the signer
//! compiles and signs. This module produces exactly that instruction.
//!
//! ## Home
//!
//! This lives in `signer-core` (not `platform-solana-tx`) on purpose:
//! `platform-solana-tx` is a `solana-sdk`-free *decoder* crate, while
//! every real tx *builder* — and thus every `Instruction` type — lives
//! here alongside [`super::compute_budget`] and [`super::ata`].
//!
//! ## Determinism
//!
//! Falcon rotates over 10 tip addresses; picking one at random per tx
//! avoids hot-account write contention. But builds must be reproducible
//! for tests + audit replay, so the address is chosen by a caller-
//! supplied [`TipSelector`] (a seed) rather than an ambient RNG. Two
//! builds with the same seed produce byte-identical transactions.

use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

/// Falcon's minimum accepted tip. A `transfer` below this is treated by
/// Falcon as "no tip" and the whole tx is rejected — so we RAISE any
/// sub-minimum Falcon tip to this value rather than send a doomed tx.
pub const FALCON_MIN_TIP_LAMPORTS: u64 = 1_000_000; // 0.001 SOL

/// Hard cap on a serialized Solana transaction (packet MTU). Falcon
/// drops anything over this. The tip `transfer` is tiny (one extra
/// account + a ~12-byte ix), but v0+ALT swap txs already run close to
/// the limit — so builders check the FINAL serialized size after the
/// tip is added and fail-closed rather than emit an oversize tx that
/// the provider silently drops.
pub const MAX_TX_BYTES: usize = 1232;

/// Guard the serialized tx against the [`MAX_TX_BYTES`] budget. Returns
/// the byte length on success so the caller can log headroom.
pub fn check_tx_size(serialized: &[u8]) -> Result<usize, TipError> {
    let len = serialized.len();
    if len > MAX_TX_BYTES {
        tracing::error!(
            tx_bytes = len,
            max = MAX_TX_BYTES,
            "built tx exceeds provider size budget — failing closed"
        );
        return Err(TipError::Oversize {
            bytes: len,
            max: MAX_TX_BYTES,
        });
    }
    Ok(len)
}

/// Falcon tip addresses (all start with the `Fa1con1` vanity prefix).
/// Any one is accepted; we spread writes across them to dodge hot-
/// account contention. Source: Falcon submit-relay docs.
pub const FALCON_TIP_ADDRESSES: [Pubkey; 10] = [
    pubkey!("Fa1con11xLjPddfzRwRUB16sbFZggp2JeJkCeWREyR8X"),
    pubkey!("Fa1con11TM1RuAQzbQzYjTy4Ekfap9Lnc9fnEbQYEd6Q"),
    pubkey!("Fa1con113Bvi76nS5AzUiRDC2fqjfzkNMUNRLgQybMYt"),
    pubkey!("Fa1con1QGHJK232s8yZpzZZwqPexnAKcoyKj626LNsMv"),
    pubkey!("Fa1con1zUzb6qJVFz5tNkPq1Ahm8H1qKW7Q48252QbkQ"),
    pubkey!("Fa1con16d3MSwd3SAiwvr2LwgkpE7ot8zntbpuec8HAx"),
    pubkey!("Fa1con1i7mpa7Qc6epYJ6r4P9AbU77DFFz173r59Df1x"),
    pubkey!("Fa1con18nWn8TdAGL7JX8PertfMUGVSc899NawokJ4Bq"),
    pubkey!("Fa1con1GKusK2EqsfzrDzGPaYZSxQtFGzJiRMMU9Zm2g"),
    pubkey!("Fa1con1RDwVwM9VrJ53CwVefD3VU9c58EMpDawV7fLMi"),
];

/// The 8 canonical Jito mainnet tip accounts. A tip to any one is read
/// by the Jito block-engine. Source: Jito Labs public tip-account list
/// (`https://docs.jito.wtf` — the fixed on-chain accounts owned by the
/// tip-distribution program). Not present elsewhere in the repo, so
/// pinned here as the single source of truth.
pub const JITO_TIP_ADDRESSES: [Pubkey; 8] = [
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    pubkey!("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];

/// Which submit provider the tip is being built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipProvider {
    /// Falcon submit relay — enforces [`FALCON_MIN_TIP_LAMPORTS`].
    Falcon,
    /// Jito block-engine bundle prioritization.
    Jito,
    /// Plain RPC path — NO tip instruction is added (today's behavior).
    None,
}

impl TipProvider {
    /// Map a `submit_mode` string (as carried in `BotConfig`) to a tip
    /// provider. FAIL-CLOSED: anything we don't recognise → [`None`]
    /// (safe plain-RPC path). Match is case-insensitive.
    ///
    /// | submit_mode                    | provider |
    /// |--------------------------------|----------|
    /// | `falcon`, `falcon_jito`        | Falcon   |
    /// | `max_race`, `max`, `quic`, `tpu` | Falcon |
    /// | `jito`                         | Jito     |
    /// | `rpc`, `paper`, ``, unknown    | None     |
    ///
    /// `falcon_jito` and `max_race` both route through Falcon's relay
    /// (which strictly rejects a tip-less tx), so they take the Falcon
    /// tip — that also satisfies Jito, which reads its own tip account
    /// when present. A `jito`-only mode takes the Jito tip.
    pub fn from_submit_mode(mode: &str) -> Self {
        let m = mode.trim().to_ascii_lowercase();
        if m.contains("falcon") {
            return TipProvider::Falcon;
        }
        match m.as_str() {
            // Race / low-latency relays go through Falcon's QUIC path.
            "max_race" | "max" | "quic" | "tpu" => TipProvider::Falcon,
            "jito" => TipProvider::Jito,
            _ => TipProvider::None,
        }
    }

    /// The tip-address table for this provider (empty for `None`).
    fn addresses(self) -> &'static [Pubkey] {
        match self {
            TipProvider::Falcon => &FALCON_TIP_ADDRESSES,
            TipProvider::Jito => &JITO_TIP_ADDRESSES,
            TipProvider::None => &[],
        }
    }
}

/// Deterministic tip-address chooser. Seed it from anything stable per
/// trade (e.g. the first 8 bytes of the recent blockhash, a slot, or a
/// fixed value in tests) so the same inputs always pick the same
/// address — builds stay reproducible.
#[derive(Debug, Clone, Copy)]
pub struct TipSelector {
    seed: u64,
}

impl TipSelector {
    /// New selector from an explicit seed.
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// Seed from a recent blockhash — spreads writes across addresses
    /// naturally (blockhash changes ~every 400ms) while staying
    /// deterministic for a given blockhash.
    pub fn from_blockhash(blockhash: &solana_sdk::hash::Hash) -> Self {
        let b = blockhash.to_bytes();
        let mut seed = [0u8; 8];
        seed.copy_from_slice(&b[..8]);
        Self {
            seed: u64::from_le_bytes(seed),
        }
    }

    /// Pick an index in `[0, len)`. `len == 0` → 0 (caller must guard).
    fn index(&self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        (self.seed % len as u64) as usize
    }
}

/// Errors from tip-instruction construction. The only failure that can
/// arise is an empty address table for a real provider, which would be
/// a programming error (the tables are non-empty constants) — modelled
/// so builders can `?`-propagate rather than panic.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TipError {
    #[error("tip provider {0:?} has no configured tip addresses")]
    NoAddresses(&'static str),
    #[error("built tx is {bytes} bytes, over the {max}-byte provider budget")]
    Oversize { bytes: usize, max: usize },
}

/// Build the provider-tip `transfer` instruction, or `Ok(None)` when no
/// tip is needed (`provider == None`).
///
/// * `payer` — the fee-payer wallet; funds the tip (a separate spend
///   the operator opted into via their submit-mode choice).
/// * `provider` — [`TipProvider::from_submit_mode`] output.
/// * `tip_lamports` — requested tip. For [`TipProvider::Falcon`], a
///   value below [`FALCON_MIN_TIP_LAMPORTS`] is RAISED to the minimum
///   (never send a sub-minimum tip that gets the whole tx rejected).
/// * `selector` — deterministic address chooser.
///
/// Returns the resolved `(Instruction, lamports_used)` so the caller
/// can log the effective tip (which may differ from the request after
/// the Falcon-minimum raise).
pub fn tip_transfer_ix(
    payer: &Pubkey,
    provider: TipProvider,
    tip_lamports: u64,
    selector: TipSelector,
) -> Result<Option<(Instruction, u64)>, TipError> {
    if provider == TipProvider::None {
        return Ok(None);
    }

    // Falcon rejects sub-minimum tips — raise rather than send a doomed
    // tx. Other providers use the requested amount verbatim.
    let lamports = match provider {
        TipProvider::Falcon if tip_lamports < FALCON_MIN_TIP_LAMPORTS => {
            tracing::warn!(
                requested = tip_lamports,
                raised_to = FALCON_MIN_TIP_LAMPORTS,
                "falcon tip below minimum — raising to avoid tx rejection"
            );
            FALCON_MIN_TIP_LAMPORTS
        }
        _ => tip_lamports,
    };

    // A real provider with a zero tip after the raise (only possible
    // for Jito with tip_lamports == 0) is a no-op transfer that Jito
    // ignores — treat it as "no tip" so we don't waste tx bytes.
    if lamports == 0 {
        tracing::warn!(
            provider = ?provider,
            "tip provider selected but tip_lamports is 0 — emitting no tip ix"
        );
        return Ok(None);
    }

    let addrs = provider.addresses();
    let name = match provider {
        TipProvider::Falcon => "Falcon",
        TipProvider::Jito => "Jito",
        TipProvider::None => unreachable!(),
    };
    let recipient = addrs
        .get(selector.index(addrs.len()))
        .copied()
        .ok_or(TipError::NoAddresses(name))?;

    let ix = super::ata::system_transfer_ix(payer, &recipient, lamports);
    Ok(Some((ix, lamports)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;

    #[test]
    fn none_provider_adds_no_tip() {
        let payer = Pubkey::new_unique();
        let r = tip_transfer_ix(&payer, TipProvider::None, 5_000_000, TipSelector::new(0)).unwrap();
        assert!(r.is_none(), "None provider must add nothing");
    }

    #[test]
    fn falcon_below_minimum_raises() {
        let payer = Pubkey::new_unique();
        let (ix, used) = tip_transfer_ix(&payer, TipProvider::Falcon, 1, TipSelector::new(0))
            .unwrap()
            .expect("falcon tip present");
        assert_eq!(used, FALCON_MIN_TIP_LAMPORTS, "raised to minimum");
        // system_instruction::transfer encodes lamports at data[4..12].
        assert_eq!(
            u64::from_le_bytes(ix.data[4..12].try_into().unwrap()),
            FALCON_MIN_TIP_LAMPORTS
        );
        assert_eq!(ix.program_id, solana_sdk::system_program::ID);
    }

    #[test]
    fn falcon_at_or_above_minimum_kept() {
        let payer = Pubkey::new_unique();
        let (_ix, used) =
            tip_transfer_ix(&payer, TipProvider::Falcon, 2_500_000, TipSelector::new(0))
                .unwrap()
                .unwrap();
        assert_eq!(used, 2_500_000);
    }

    #[test]
    fn seeded_selector_is_deterministic_and_picks_expected_recipient() {
        let payer = Pubkey::new_unique();
        // seed 3 → index 3 into the 10-address Falcon table.
        let (ix, _) = tip_transfer_ix(&payer, TipProvider::Falcon, 1_000_000, TipSelector::new(3))
            .unwrap()
            .unwrap();
        assert_eq!(ix.accounts[1].pubkey, FALCON_TIP_ADDRESSES[3]);
        // Same seed → same recipient.
        let (ix2, _) = tip_transfer_ix(&payer, TipProvider::Falcon, 1_000_000, TipSelector::new(3))
            .unwrap()
            .unwrap();
        assert_eq!(ix.accounts[1].pubkey, ix2.accounts[1].pubkey);
    }

    #[test]
    fn selector_wraps_across_table() {
        // seed 13 % 10 = index 3.
        assert_eq!(TipSelector::new(13).index(FALCON_TIP_ADDRESSES.len()), 3);
        // seed 8 % 8 = 0 for Jito.
        assert_eq!(TipSelector::new(8).index(JITO_TIP_ADDRESSES.len()), 0);
    }

    #[test]
    fn jito_tip_uses_jito_table_verbatim() {
        let payer = Pubkey::new_unique();
        let (ix, used) = tip_transfer_ix(&payer, TipProvider::Jito, 200_000, TipSelector::new(1))
            .unwrap()
            .unwrap();
        // No minimum raise for Jito.
        assert_eq!(used, 200_000);
        assert_eq!(ix.accounts[1].pubkey, JITO_TIP_ADDRESSES[1]);
    }

    #[test]
    fn jito_zero_tip_emits_nothing() {
        let payer = Pubkey::new_unique();
        let r = tip_transfer_ix(&payer, TipProvider::Jito, 0, TipSelector::new(0)).unwrap();
        assert!(r.is_none(), "zero Jito tip is a no-op — emit nothing");
    }

    #[test]
    fn submit_mode_maps_falcon_family() {
        assert_eq!(TipProvider::from_submit_mode("falcon"), TipProvider::Falcon);
        assert_eq!(
            TipProvider::from_submit_mode("falcon_jito"),
            TipProvider::Falcon
        );
        assert_eq!(
            TipProvider::from_submit_mode("FALCON_JITO"),
            TipProvider::Falcon
        );
        assert_eq!(
            TipProvider::from_submit_mode("max_race"),
            TipProvider::Falcon
        );
        assert_eq!(TipProvider::from_submit_mode("quic"), TipProvider::Falcon);
        assert_eq!(TipProvider::from_submit_mode("tpu"), TipProvider::Falcon);
    }

    #[test]
    fn submit_mode_maps_jito_only() {
        assert_eq!(TipProvider::from_submit_mode("jito"), TipProvider::Jito);
    }

    #[test]
    fn submit_mode_fail_closed_to_none() {
        assert_eq!(TipProvider::from_submit_mode("rpc"), TipProvider::None);
        assert_eq!(TipProvider::from_submit_mode("paper"), TipProvider::None);
        assert_eq!(TipProvider::from_submit_mode(""), TipProvider::None);
        assert_eq!(
            TipProvider::from_submit_mode("something_unknown"),
            TipProvider::None
        );
    }

    #[test]
    fn from_blockhash_seed_is_stable() {
        let bh = Hash::new_from_array([7u8; 32]);
        let a = TipSelector::from_blockhash(&bh);
        let b = TipSelector::from_blockhash(&bh);
        assert_eq!(a.seed, b.seed);
    }

    #[test]
    fn size_guard_accepts_normal_tx_and_rejects_oversize() {
        assert_eq!(check_tx_size(&vec![0u8; 1000]).unwrap(), 1000);
        assert_eq!(
            check_tx_size(&vec![0u8; MAX_TX_BYTES]).unwrap(),
            MAX_TX_BYTES
        );
        assert_eq!(
            check_tx_size(&vec![0u8; MAX_TX_BYTES + 1]),
            Err(TipError::Oversize {
                bytes: MAX_TX_BYTES + 1,
                max: MAX_TX_BYTES
            })
        );
    }

    #[test]
    fn falcon_and_jito_tables_have_no_overlap() {
        for f in FALCON_TIP_ADDRESSES.iter() {
            assert!(!JITO_TIP_ADDRESSES.contains(f), "tables must not overlap");
        }
    }
}
