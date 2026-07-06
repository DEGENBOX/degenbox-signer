# HL gateway ↔ signer wire-contract fixtures

Shared JSON golden fixtures pinning the **wire contract** between the
gateway (`crates/modules/hyperliquid/src/exchange/`) and the desktop
signer (`trading/signer-core/src/hl/`). The two sides live in **separate
cargo workspaces** (root vs `trading/`), so neither can compile against
the other's types — these fixtures are the single shared schema both
sides test against. The 2026-06-11 production-readiness audit traced an
entire bug class (B3/B4/B5/M11/M12: dropped `reduce_only`, rejected
`paper`/`filled_unprotected` statuses, 404ing `close:*` results, unread
`leverage`) to each side pinning only its *own* shape.

## Layout

| Prefix          | Direction        | Producer                                            | Consumer                              |
|-----------------|------------------|-----------------------------------------------------|---------------------------------------|
| `instruction_*` | gateway → signer | `executor.rs` payload builders (`build_signer_payload`, `close_position_payload`, `cancel_payload`, `cancel_by_oid_payload`, `update_leverage_payload`, `vault_transfer_payload`, `place_tpsl_payload`) | `signing.rs` payload structs (`OrderPayload`, `ClosePositionPayload`, …) |
| `result_*`      | signer → gateway | `server.rs` `ResultReq`                             | `api.rs` `OrderResultBody` (`POST /order/result`) |

One fixture per instruction **kind** the gateway emits and per result
**status** the signer emits. `instruction_order_*` has three variants to
pin the load-bearing optional fields: plain market, `reduce_only`, and
`tp_px`/`sl_px` + `leverage` (mixed-case `kPEPE` asset pins H6 exact
casing on the wire).

Every instruction envelope carries `"network": "mainnet" | "testnet"`
(M24, defense-in-depth): the signer REFUSES to sign an instruction whose
tag mismatches its own configured network — a testnet-configured signer
can never sign gateway-mainnet asset ids (and vice versa). Envelopes
without the field (pre-M24 gateways) are accepted for backward compat.

## Tests that consume these (BOTH must stay green)

* **Gateway:** `crates/modules/hyperliquid/src/exchange/contract_tests.rs`
  — serializes each instruction via the real payload builders and asserts
  **exact JSON equality** with the fixture; deserializes every
  `result_*` into `OrderResultBody` and asserts the status passes the
  `/order/result` whitelist.
* **Signer:** `trading/signer-core/src/hl/signing.rs` (`contract_tests`
  module) — deserializes each instruction fixture into its payload
  struct and asserts the load-bearing fields (incl. `reduce_only`,
  `leverage`); serializes a `ResultReq` for every result fixture and
  asserts exact JSON equality.
* Both sides carry an **inventory test** asserting the exact set of
  fixture filenames. Adding a fixture (a new field/kind/status) fails
  both inventories until each side explicitly covers it — that is the
  drift alarm.

## Changing the contract

1. Change the producer (payload builder / `ResultReq` / status set).
2. Update (or add) the fixture here — the producer-side equality test
   tells you the exact diff.
3. Run the OTHER side's tests: they now fail until its decoder handles
   the new shape (and its inventory test names the new file).
4. Keep fields backward compatible on the wire: new payload fields must
   be `#[serde(default)]` on the consumer, new result statuses must be
   added to the gateway whitelist BEFORE a signer that emits them ships.

Follow-up (deferred): fold both sides onto one shared wire-contract
crate once the workspaces merge; these fixtures are the contract until
then.
