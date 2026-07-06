//! Compute-budget instruction builders.
//!
//! The compute-budget program lets the caller bump a transaction's CU
//! limit + per-CU priority-fee price. Both are critical for memecoin
//! swaps: PumpFun buys can spend 60-80k CU and the default 200k limit
//! per-tx is plenty, BUT the per-CU price is what wins inclusion races
//! during network congestion.
//!
//! We hand-encode the two instructions we need (`SetComputeUnitLimit`
//! discriminator = 2, `SetComputeUnitPrice` discriminator = 3) instead
//! of pulling in `solana-compute-budget-interface` — single bytes,
//! audit-friendly.

use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    pubkey!("ComputeBudget111111111111111111111111111111");

/// `SetComputeUnitLimit(units)` — overrides the default 200k cap.
/// `units` is u32; passing >1.4M is rejected on-chain. We typically
/// pass 80-120k for memecoin swaps (60k for the swap + headroom for
/// idempotent ATA-create + the program's CPI overhead).
pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2u8); // discriminator
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: Vec::new(),
        data,
    }
}

/// `SetComputeUnitPrice(micro_lamports_per_cu)` — extra priority-fee
/// per CU. Validator-side tip mechanism that wins inclusion ordering
/// on contested slots. We typically pass 50_000..500_000 for memecoin
/// swaps; the gateway's `/api/trading/stats/priority-fee` endpoint
/// returns live p75/p95 percentiles to size this dynamically.
pub fn set_compute_unit_price(micro_lamports_per_cu: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3u8); // discriminator
    data.extend_from_slice(&micro_lamports_per_cu.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: Vec::new(),
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cu_limit_encoded_as_disc_plus_u32_le() {
        let ix = set_compute_unit_limit(100_000);
        assert_eq!(ix.program_id, COMPUTE_BUDGET_PROGRAM_ID);
        assert!(ix.accounts.is_empty());
        assert_eq!(ix.data[0], 2);
        assert_eq!(
            u32::from_le_bytes(ix.data[1..5].try_into().unwrap()),
            100_000
        );
        assert_eq!(ix.data.len(), 5);
    }

    #[test]
    fn cu_price_encoded_as_disc_plus_u64_le() {
        let ix = set_compute_unit_price(40_717); // realistic p75 sample
        assert_eq!(ix.data[0], 3);
        assert_eq!(
            u64::from_le_bytes(ix.data[1..9].try_into().unwrap()),
            40_717
        );
        assert_eq!(ix.data.len(), 9);
    }
}
