//! Sign a Jupiter-supplied `VersionedTransaction` with the signer's
//! ed25519 keypair.
//!
//! Jupiter returns the unsigned transaction base64-encoded. We
//! deserialize, attach our signature on the fee-payer slot (which
//! Jupiter set to `userPublicKey`), and re-serialize to base64 ready
//! for relay-submit.
//!
//! ## Invariants
//!
//! - The transaction's `static_account_keys[0]` MUST equal our
//!   pubkey. Jupiter assigns the fee-payer to the `userPublicKey` we
//!   sent on `/swap`; if the response routes through a different
//!   fee-payer it's an integrity failure (Jupiter bug or MitM).
//! - We sign the message over `serialize_message()`, NOT raw bytes.
//!   `VersionedTransaction::sign` does this for us; we expose the
//!   raw helper only via test fixtures.
//!
//! ## What this does NOT do
//!
//! - Does not simulate the tx. The full signer (post-T.1) will run
//!   `simulateTransaction` against an RPC to catch bad routes
//!   before they hit chain. Today's CLI submits unconditionally.
//! - Does not enforce a program-allowlist. Production must reject
//!   any tx that touches programs outside Jupiter / Raydium / Orca /
//!   PumpFun / SystemProgram / TokenProgram / Memo. Slot T.2 work.

use base64::Engine as _;
use solana_sdk::{
    signature::{Keypair, Signer as _},
    transaction::VersionedTransaction,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SignError {
    #[error("base64 decode: {0}")]
    Base64(String),
    #[error("bincode decode: {0}")]
    Bincode(String),
    #[error("fee-payer mismatch: tx wants {tx_payer}, signer has {our_pubkey}")]
    FeePayerMismatch {
        tx_payer: String,
        our_pubkey: String,
    },
    #[error("sign: {0}")]
    Sign(String),
    #[error("bincode encode: {0}")]
    BincodeEncode(String),
}

/// Decode a Jupiter-supplied base64 unsigned tx, sign it with `kp`,
/// and re-encode to base64. Returns the signed bytes ready to ship to
/// the gateway relay.
pub fn sign_jupiter_tx_b64(unsigned_b64: &str, kp: &Keypair) -> Result<String, SignError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(unsigned_b64)
        .map_err(|e| SignError::Base64(e.to_string()))?;
    let signed_bytes = sign_versioned_tx_bytes(&bytes, kp)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(signed_bytes))
}

/// Lower-level: take raw bincode-encoded `VersionedTransaction` bytes,
/// sign, return raw bytes. Tests use this; the CLI uses the b64
/// wrapper above.
pub fn sign_versioned_tx_bytes(bytes: &[u8], kp: &Keypair) -> Result<Vec<u8>, SignError> {
    let unsigned: VersionedTransaction =
        bincode::deserialize(bytes).map_err(|e| SignError::Bincode(e.to_string()))?;

    // Verify that the fee-payer (account[0]) matches our pubkey. If
    // Jupiter ever returns a tx with a different payer we're being
    // asked to sign a tx whose fees come out of someone else's
    // account — refuse.
    let payer = unsigned
        .message
        .static_account_keys()
        .first()
        .copied()
        .ok_or_else(|| SignError::Sign("no static account keys in tx".into()))?;
    let our_pk = kp.pubkey();
    if payer != our_pk {
        return Err(SignError::FeePayerMismatch {
            tx_payer: payer.to_string(),
            our_pubkey: our_pk.to_string(),
        });
    }

    // VersionedTransaction::try_new takes a message + signers and
    // produces a freshly-signed copy.
    let message = unsigned.message;
    let signed = VersionedTransaction::try_new(message, &[kp])
        .map_err(|e| SignError::Sign(e.to_string()))?;
    bincode::serialize(&signed).map_err(|e| SignError::BincodeEncode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        hash::Hash,
        message::{v0, VersionedMessage},
        pubkey::Pubkey,
    };
    // The `system_instruction` module is deprecated in solana-sdk 2.1
    // in favour of `solana-system-interface`. Test-only use; not worth
    // a new top-level dep.
    #[allow(deprecated)]
    use solana_sdk::system_instruction;

    fn build_unsigned_tx_for(payer: &Pubkey) -> Vec<u8> {
        let recipient = Pubkey::new_unique();
        let ix = system_instruction::transfer(payer, &recipient, 1_000);
        let msg = v0::Message::try_compile(payer, &[ix], &[], Hash::default()).unwrap();
        let tx = VersionedTransaction {
            signatures: vec![Default::default()],
            message: VersionedMessage::V0(msg),
        };
        bincode::serialize(&tx).unwrap()
    }

    #[test]
    fn sign_round_trip_correct_payer() {
        let kp = Keypair::new();
        let bytes = build_unsigned_tx_for(&kp.pubkey());
        let signed = sign_versioned_tx_bytes(&bytes, &kp).unwrap();
        let re: VersionedTransaction = bincode::deserialize(&signed).unwrap();
        // Signature should now be non-default.
        assert_ne!(re.signatures[0], Default::default());
        // verify_signatures works for the correct payer.
        assert!(re.verify_with_results().iter().all(|b| *b));
    }

    #[test]
    fn sign_rejects_wrong_payer() {
        let our_kp = Keypair::new();
        let other_kp = Keypair::new();
        let bytes = build_unsigned_tx_for(&other_kp.pubkey());
        let err = sign_versioned_tx_bytes(&bytes, &our_kp).unwrap_err();
        assert!(matches!(err, SignError::FeePayerMismatch { .. }));
    }

    #[test]
    fn malformed_input_is_rejected() {
        let kp = Keypair::new();
        let r = sign_versioned_tx_bytes(b"not a real tx", &kp);
        assert!(matches!(r, Err(SignError::Bincode(_))));
    }
}
