//! Bot engine — signal-driven auto-trader.
//!
//! Consumes a stream of matched signals (preset-filtered server-side
//! by `module-alpha-scanner::filter` and republished on
//! `alpha.signals.matched.{preset_id}`), and for each:
//!
//! 1. Matches against the local **`PresetMatcher`** rules (mcap/liq
//!    floor, recency, blocklist). The server's filter already
//!    matched the preset semantically — the local matcher is a
//!    second-line defense that catches stale data or a server-side
//!    bug widening the criteria.
//! 2. Asks **`BudgetState`** whether we can afford this trade. Per-
//!    session / per-token / per-hour caps all evaluated.
//! 3. Builds a Jupiter swap (uses the `JupiterClient` from this
//!    crate; the caller passes one in).
//! 4. Runs the pre-sign safety pass:
//!    - decodes the unsigned tx
//!    - asserts every touched program is in the allowlist
//!    - simulates against the user's RPC URL
//! 5. Signs locally with the keystore-loaded keypair.
//! 6. Hands the signed bytes to the **`RelayClient`** to submit via
//!    the gateway's race-relay (Falcon QUIC + Jito).
//! 7. Records the spend in `BudgetState`.
//!
//! ## Why the orchestrator lives here (not in signer-desktop)
//!
//! signer-extension (WASM) and signer-desktop (Tauri) both want to
//! run a bot. Putting the logic in `signer-core` lets both transports
//! reuse it. The CLI just instantiates a `BotEngine` + drives it from
//! whatever transport-specific signal-source it has (HTTP poll for
//! the CLI; WebSocket subscription for the extension; eventually
//! NATS subscription for the desktop daemon).
//!
//! ## Trait shape
//!
//! `BotEngine::handle_one(&mut self, signal)` returns a `Decision`
//! that the caller logs / displays. Pure value, no I/O after the
//! relay-submit completes (or fails). Transports keep their I/O
//! layer; the engine is async + cancellation-safe.

use crate::{
    ata_cache::AtaCache,
    blockhash_cache::BlockhashCache,
    budget::{BudgetError, BudgetState},
    decode_jupiter_tx_b64,
    dex::{
        ata,
        pumpfun::{self, BuildTxError, BuyTxParams, SellTxParams},
        pumpfun_amm::{self, BuyTxParams as AmmBuyTxParams, SellTxParams as AmmSellTxParams},
        raydium_amm_v4,
        tip::{TipProvider, TipSelector},
    },
    fee_strategy::{DexId, FeeParams, FeeStrategy, FeeTier, Side as TradeSide},
    jupiter::{JupiterClient, SwapOptions},
    program_allow::{Allowlist, AllowlistError},
    relay::{CreateIntentReq, RelayClient, SubmitResp},
    route::{self, PumpFunAmmRoute, PumpFunRoute, RouteError, SwapRoute},
    rpc::{RpcClient, RpcError},
    sign_jupiter_tx_b64, sign_versioned_tx_bytes,
    simulator::{SimulationOutcome, Simulator},
    Keypair, Signer,
};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::str::FromStr;
use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("budget: {0}")]
    Budget(#[from] BudgetError),
    #[error("preset reject: {0}")]
    PresetReject(String),
    #[error("dedup: signal {0} already processed in this session")]
    Duplicate(String),
    #[error("jupiter: {0}")]
    Jupiter(#[from] crate::jupiter::JupiterError),
    #[error("simulator: {0}")]
    Simulate(#[from] crate::simulator::SimulateError),
    #[error("simulation rejected: {0}")]
    SimulationRejected(String),
    #[error("allowlist: {0}")]
    Allowlist(#[from] AllowlistError),
    #[error("sign: {0}")]
    Sign(#[from] crate::signer::SignError),
    #[error("relay: {0}")]
    Relay(#[from] crate::relay::RelayError),
    #[error("route: {0}")]
    Route(#[from] RouteError),
    #[error("rpc: {0}")]
    Rpc(#[from] RpcError),
    #[error("build pumpfun tx: {0}")]
    BuildPumpFunTx(#[from] BuildTxError),
    #[error("build pumpfun-amm tx: {0}")]
    BuildPumpFunAmmTx(String),
    #[error("build raydium tx: {0}")]
    BuildRaydiumTx(String),
    #[error("invalid mint pubkey: {0}")]
    InvalidMint(String),
}

/// Inbound signal — matches the shape `module-alpha-scanner` emits
/// on `alpha.signals.matched.{preset_id}`. Kept loose (string mints,
/// optional fields) so we're forward-compatible with backend
/// payload additions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    /// Stable per-signal id — used for dedup.
    pub call_id: String,
    pub chain_id: i16,
    pub token_address: String,
    pub symbol: Option<String>,
    pub price_usd: Option<f64>,
    pub market_cap_usd: Option<f64>,
    pub liquidity_usd: Option<f64>,
    pub called_at: DateTime<Utc>,
    /// The preset that matched this signal server-side.
    pub matched_preset_id: String,
    /// Optional Raydium AMM v4 pool address for `token_address`.
    /// When present, the bot will attempt a native Raydium swap rather
    /// than falling back to Jupiter. Populated by the alpha-scanner
    /// pool enrichment worker for tokens whose primary venue is
    /// Raydium (i.e. not PumpFun-native). `serde(default)` ensures
    /// older JSON payloads (without this field) deserialize cleanly.
    #[serde(default)]
    pub amm_address: Option<String>,
}

/// Local matcher — runs AFTER the server filter as a defense-in-depth
/// check + lets the user tighten criteria without redeploying server.
#[derive(Debug, Clone)]
pub struct PresetMatcher {
    /// Min mcap (USD). None = no floor.
    pub min_mcap_usd: Option<f64>,
    /// Max mcap (USD). None = no ceiling.
    pub max_mcap_usd: Option<f64>,
    /// Min liquidity (USD).
    pub min_liquidity_usd: Option<f64>,
    /// Max age (seconds since `called_at`). None = no recency limit.
    pub max_age_secs: Option<i64>,
    /// Token addresses to never trade (e.g. rug-pulls the user wants
    /// to permanently exclude even though the preset matches them).
    pub blocked_tokens: HashSet<String>,
}

impl PresetMatcher {
    pub fn evaluate(&self, sig: &Signal, now: DateTime<Utc>) -> Result<(), String> {
        if self.blocked_tokens.contains(&sig.token_address) {
            return Err(format!("token {} is in blocklist", sig.token_address));
        }
        if let (Some(min), Some(mc)) = (self.min_mcap_usd, sig.market_cap_usd) {
            if mc < min {
                return Err(format!("mcap {mc} < min {min}"));
            }
        }
        if let (Some(max), Some(mc)) = (self.max_mcap_usd, sig.market_cap_usd) {
            if mc > max {
                return Err(format!("mcap {mc} > max {max}"));
            }
        }
        if let (Some(min), Some(liq)) = (self.min_liquidity_usd, sig.liquidity_usd) {
            if liq < min {
                return Err(format!("liq {liq} < min {min}"));
            }
        }
        if let Some(max_age) = self.max_age_secs {
            let age = (now - sig.called_at).num_seconds();
            if age > max_age {
                return Err(format!("signal age {age}s > max {max_age}s"));
            }
        }
        Ok(())
    }
}

/// Outcome of `handle_one`. Caller logs / pipes to UI.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Trade was relayed. Surface the relay response so the caller
    /// can show intent-id / per-path order outcomes.
    Submitted(SubmitResp),
    /// Did NOT trade. `reason` explains why. The bot continues —
    /// this is the normal "not interested" path, not an error.
    Skipped(String),
}

impl Decision {
    pub fn submitted(&self) -> bool {
        matches!(self, Decision::Submitted(_))
    }
}

/// Static config for one bot session.
pub struct BotConfig {
    /// Lamports to spend per match.
    pub per_trade_lamports: u64,
    /// Slippage bps applied to Jupiter quotes.
    pub slippage_bps: u16,
    /// Falcon tip in lamports per submit.
    pub tip_lamports: i64,
    /// Submit mode forwarded to the gateway (`falcon` / `falcon_jito` / `max_race`).
    pub submit_mode: String,
    /// Solana RPC URL for the pre-sign simulator.
    pub rpc_url: String,
    /// Skip the simulator (UNSAFE — for offline tests only).
    pub skip_simulate: bool,
    /// Skip the allowlist (UNSAFE — for offline tests only).
    pub skip_allowlist: bool,
    /// Quote-side input mint. Almost always WSOL.
    pub input_mint: String,
    /// PumpFun-native CU limit. Default ~120_000 covers cu-budget × 2
    /// + idempotent ATA-create + buy ix comfortably.
    pub pumpfun_cu_limit: u32,
    /// PumpFun-native priority fee (micro-lamports per CU). Default
    /// 50_000 is fine for non-contested slots; tighten in prod from
    /// the gateway's `/api/trading/stats/priority-fee` p75.
    pub pumpfun_cu_price_micro_lamports: u64,
    /// Bot-session UUID (or `None` for ad-hoc / interactive bots).
    /// When set, every intent the engine creates is tagged with this
    /// id so the gateway can attribute the spend to the session row
    /// in `trading_bot_sessions`.
    pub bot_session_id: Option<String>,
    /// Preset UUID that armed this bot — surfaces in fill-analytics +
    /// audit log. Independent of the per-signal `matched_preset_id`
    /// (which is the same value in normal use but kept distinct for
    /// future multi-preset sessions).
    pub preset_id: Option<String>,
}

impl BotConfig {
    /// Which submit-provider tip (if any) this session's `submit_mode`
    /// implies. FAIL-CLOSED: an unknown mode maps to
    /// [`TipProvider::None`] (plain RPC, no tip) so a misconfigured
    /// mode never blocks trading — it just forgoes the tip. Copy trades
    /// share `self.cfg`, so they inherit the same provider automatically.
    fn tip_provider(&self) -> TipProvider {
        TipProvider::from_submit_mode(&self.submit_mode)
    }

    /// The tip amount in lamports, clamped to `u64` (a negative
    /// `tip_lamports` — which the UI shouldn't produce — is treated as
    /// zero). Falcon separately raises sub-minimum values at build time.
    fn tip_lamports_u64(&self) -> u64 {
        self.tip_lamports.max(0) as u64
    }

    /// Build the PumpFun buy-tx params from a session config + signal +
    /// live curve state + recent blockhash. Caller fetches blockhash,
    /// BondingCurveAccount and the mint's owner program; this fn does
    /// no I/O.
    fn pumpfun_buy_params(
        &self,
        user: Pubkey,
        mint: Pubkey,
        curve: &pumpfun::BondingCurveAccount,
        token_program: Pubkey,
        recent_blockhash: solana_sdk::hash::Hash,
        fee: FeeParams,
    ) -> BuyTxParams {
        BuyTxParams {
            user,
            mint,
            creator: curve.creator,
            token_program,
            sol_in_lamports: self.per_trade_lamports,
            slippage_bps: self.slippage_bps,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
            recent_blockhash,
            compute_unit_limit: fee.cu_limit,
            compute_unit_price_micro_lamports: fee.cu_price_micro_lamports,
            tip_provider: self.tip_provider(),
            tip_lamports: self.tip_lamports_u64(),
            tip_selector: TipSelector::from_blockhash(&recent_blockhash),
            // Safe default: include the create-IX. Caller (`BotEngine`)
            // overrides to `true` after an AtaCache hit.
            skip_token_ata_create: false,
        }
    }

    /// Build the PumpFun sell-tx params. Same CU budget + slippage as
    /// buys; only the side + amount differ. `token_in_amount` is in
    /// raw base units (i.e. multiplied by 10^decimals already).
    #[allow(clippy::too_many_arguments)]
    fn pumpfun_sell_params(
        &self,
        user: Pubkey,
        mint: Pubkey,
        token_in_amount: u64,
        curve: &pumpfun::BondingCurveAccount,
        token_program: Pubkey,
        recent_blockhash: solana_sdk::hash::Hash,
        fee: FeeParams,
    ) -> SellTxParams {
        SellTxParams {
            user,
            mint,
            creator: curve.creator,
            token_program,
            token_in_amount,
            slippage_bps: self.slippage_bps,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
            recent_blockhash,
            compute_unit_limit: fee.cu_limit,
            compute_unit_price_micro_lamports: fee.cu_price_micro_lamports,
            tip_provider: self.tip_provider(),
            tip_lamports: self.tip_lamports_u64(),
            tip_selector: TipSelector::from_blockhash(&recent_blockhash),
        }
    }
}

/// Stateful bot engine. Hold one per session.
pub struct BotEngine {
    matcher: PresetMatcher,
    budget: BudgetState,
    allowlist: Allowlist,
    cfg: BotConfig,
    seen: HashSet<String>,
    /// Total signals seen (including dups + skips) — useful for
    /// `stats()` UI.
    total_seen: u64,
    submitted_count: u64,
    skipped_count: u64,
    /// Optional background-refreshed blockhash cache. When `Some`,
    /// trade build paths read from the cache (sub-millisecond) instead
    /// of doing a fresh `getLatestBlockhash` RPC round-trip per trade.
    /// When `None`, the engine falls back to the direct RPC path.
    blockhash_cache: Option<BlockhashCache>,
    /// Optional cache of `(owner, mint)` pairs whose ATAs we have
    /// already seen exist. When attached, the buy/sell build paths
    /// suppress the redundant `CreateIdempotent` instruction — saves
    /// ~3.5k CU + ~120 bytes per repeat trade for the same wallet.
    ata_cache: Option<AtaCache>,
    /// Optional fee-strategy lookup. When attached, every build path
    /// pulls its CU limit + priority-fee from the strategy (which the
    /// priority-fee poller keeps fresh), instead of using the legacy
    /// `pumpfun_cu_*` fields from `BotConfig`.
    fee_strategy: Option<FeeStrategy>,
    /// Which fee tier to use when resolving via `fee_strategy`.
    /// Ignored when no strategy is attached.
    fee_tier: FeeTier,
    /// Copy-trade context: when set (via [`Self::set_copy_config_id`]),
    /// every intent this engine creates is tagged with the
    /// `sol_copy_trade_configs` UUID so the gateway can auto-arm the
    /// config's default TP/SL ladder on the buy fill and enforce the
    /// cumulative per-mint position cap. The copy-execution loop sets
    /// it per event (events for different wallets carry different
    /// config ids); plain bot/TP-SL engines never touch it.
    copy_config_id: Option<String>,
    /// When set, the NEXT trade this engine submits reuses this EXISTING
    /// `trading_intents` id (the web UI pre-created it for a MANUAL
    /// buy/sell) instead of calling `create_intent` — mirroring the
    /// daemon `/swap` `intentId` path + the FE's "reuse the intent row …
    /// without this the signer would create a SECOND one and orphan the
    /// first". [`create_or_submit_intent`](Self::create_or_submit_intent)
    /// CONSUMES it (takes) on the first submit so it can never leak into
    /// a later copy / TP-SL / bot trade that shares this engine. The
    /// manual dispatch loop sets it per event and clears it after.
    pending_intent_id: Option<String>,
}

impl BotEngine {
    pub fn new(
        matcher: PresetMatcher,
        budget: BudgetState,
        allowlist: Allowlist,
        cfg: BotConfig,
    ) -> Self {
        Self {
            matcher,
            budget,
            allowlist,
            cfg,
            seen: HashSet::new(),
            total_seen: 0,
            submitted_count: 0,
            skipped_count: 0,
            blockhash_cache: None,
            ata_cache: None,
            fee_strategy: None,
            fee_tier: FeeTier::Fast,
            copy_config_id: None,
            pending_intent_id: None,
        }
    }

    /// Set / clear the copy-trade tag every subsequently-created
    /// intent carries. See the field docs — copy loops call this per
    /// event before `execute_buy` / `execute_sell` and SHOULD clear it
    /// (pass `None`) if they reuse the engine for non-copy commands.
    pub fn set_copy_config_id(&mut self, config_id: Option<String>) {
        self.copy_config_id = config_id;
    }

    /// Stamp the EXISTING `trading_intents` id the next trade must submit
    /// to instead of creating a fresh intent — see the field docs. The
    /// manual-intent dispatch loop sets it before `execute_buy` /
    /// `execute_sell_with_dedupe` and clears it (`None`) after, so a
    /// stale id can never bleed into a subsequent copy/TP-SL/bot trade
    /// on this shared engine.
    pub fn set_pending_intent_id(&mut self, intent_id: Option<String>) {
        self.pending_intent_id = intent_id;
    }

    /// Terminal step of every submit path: relay the signed bytes.
    ///
    /// * DEFAULT — create a fresh `trading_intents` row (`create_intent`)
    ///   then submit it. This is the copy / TP-SL / bot / signal path.
    /// * MANUAL — when [`set_pending_intent_id`](Self::set_pending_intent_id)
    ///   stamped an id, submit that ALREADY-EXISTING intent directly and
    ///   skip `create_intent` (the web UI pre-created the row so it could
    ///   render a `pending` state; creating a second would orphan the
    ///   first). The stamp is TAKEN so it is used exactly once.
    ///
    /// The gateway's atomic claim (`claim_intent_for_submit`) is the
    /// double-submit authority either way, so a redelivery or a second
    /// signer just loses the race with a benign 409.
    async fn create_or_submit_intent(
        &mut self,
        req: CreateIntentReq,
        signed_b64: &str,
        relay: &RelayClient,
    ) -> Result<SubmitResp, BotError> {
        let intent_id = match self.pending_intent_id.take() {
            Some(id) => id,
            None => relay.create_intent(&req).await?.id,
        };
        Ok(relay
            .submit(&intent_id, signed_b64, Some(&self.cfg.submit_mode))
            .await?)
    }

    /// Swap the RPC endpoint the engine's per-trade `Simulator` builds
    /// against. Needed by hosts whose DEFAULT RPC is the gateway's
    /// token-gated proxy: the credential rotates (~24 h JWT lifecycle),
    /// so the embedded URL must rotate with it or simulations start
    /// 401-ing mid-session. User-override / env RPC hosts never call
    /// this.
    pub fn set_rpc_url(&mut self, url: String) {
        self.cfg.rpc_url = url;
    }

    /// Attach a background-refreshed blockhash cache. Returns `self`
    /// so callers can chain after `new()`.
    pub fn with_blockhash_cache(mut self, cache: BlockhashCache) -> Self {
        self.blockhash_cache = Some(cache);
        self
    }

    /// Attach a shared ATA-existence cache. Returns `self` for chaining.
    pub fn with_ata_cache(mut self, cache: AtaCache) -> Self {
        self.ata_cache = Some(cache);
        self
    }

    /// Attach a shared fee strategy and pin the tier this session
    /// trades at. When attached, the bot pulls CU limits + priority
    /// fee from the strategy on every trade.
    pub fn with_fee_strategy(mut self, strategy: FeeStrategy, tier: FeeTier) -> Self {
        self.fee_strategy = Some(strategy);
        self.fee_tier = tier;
        self
    }

    /// Resolve `(cu_limit, cu_price)` for a trade. Strategy wins when
    /// attached; otherwise we fall back to the legacy `pumpfun_cu_*`
    /// fields for PumpFun, and to the hardcoded venue defaults for
    /// other DEXes (preserving the pre-FeeStrategy behavior exactly).
    fn fee_params_for(&self, dex: DexId, side: TradeSide) -> FeeParams {
        if let Some(s) = &self.fee_strategy {
            return s.get(dex, side, self.fee_tier);
        }
        // Legacy fallback: per-DEX hardcoded values, with PumpFun
        // taking its limit from BotConfig (operator-tunable) and
        // others using the defaults baked into the build sites.
        let cu_limit = match (dex, side) {
            (DexId::PumpFun, _) => self.cfg.pumpfun_cu_limit,
            (DexId::PumpFunAmm, TradeSide::Buy) => 200_000,
            (DexId::PumpFunAmm, TradeSide::Sell) => 150_000,
            (DexId::RaydiumAmmV4, TradeSide::Buy) => 220_000,
            (DexId::RaydiumAmmV4, TradeSide::Sell) => 180_000,
            (DexId::Jupiter, _) => 400_000,
        };
        FeeParams {
            cu_limit,
            cu_price_micro_lamports: self.cfg.pumpfun_cu_price_micro_lamports,
        }
    }

    /// Fetch a recent blockhash via the cache if attached, otherwise
    /// fall back to a direct RPC call. Centralised here so every
    /// build-path goes through the same source.
    async fn recent_blockhash(&self, rpc: &RpcClient) -> Result<Hash, RpcError> {
        match &self.blockhash_cache {
            Some(c) => c.get().await,
            None => rpc.get_latest_blockhash().await,
        }
    }

    /// True iff the ATA-cache says `(owner, mint)`'s ATA exists.
    /// Returns `false` when no cache is attached so the safe default
    /// behavior (always include `CreateIdempotent`) prevails.
    fn ata_known(&self, owner: &Pubkey, mint: &Pubkey) -> bool {
        self.ata_cache
            .as_ref()
            .is_some_and(|c| c.is_known(owner, mint))
    }

    /// Mark the relevant ATAs as known after a successful submit.
    /// No-op when no cache is attached.
    fn mark_atas_known(&self, owner: Pubkey, mints: &[Pubkey]) {
        if let Some(c) = &self.ata_cache {
            for m in mints {
                c.mark_known(owner, *m);
            }
        }
    }

    /// Stats snapshot — pure read, no mutation.
    pub fn stats(&self) -> BotStats {
        BotStats {
            total_seen: self.total_seen,
            submitted: self.submitted_count,
            skipped: self.skipped_count,
            budget_remaining_lamports: self.budget.remaining(),
            budget_spent_lamports: self.budget.total_spent(),
        }
    }

    /// Process one inbound signal. Returns an outcome the caller can
    /// log / surface to the user. Mutates internal state regardless
    /// of outcome (counter + dedup).
    ///
    /// `rpc` is the Solana RPC client used for route discovery
    /// (PumpFun bonding-curve PDA lookup) + blockhash fetch on the
    /// native path. Should point at the user's `SOLANA_RPC_URL`.
    pub async fn handle_one(
        &mut self,
        sig: Signal,
        jup: &JupiterClient,
        relay: &RelayClient,
        rpc: &RpcClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        self.total_seen += 1;
        let now = Utc::now();

        // 1. Dedup.
        if !self.seen.insert(sig.call_id.clone()) {
            self.skipped_count += 1;
            return Err(BotError::Duplicate(sig.call_id));
        }

        // 2. Matcher.
        if let Err(reason) = self.matcher.evaluate(&sig, now) {
            self.skipped_count += 1;
            return Err(BotError::PresetReject(reason));
        }

        // 3. Budget.
        self.budget.check_at(
            &sig.token_address,
            self.cfg.per_trade_lamports,
            Instant::now(),
        )?;

        // 4. Route dispatch.
        //    • Raydium AMM v4  — when signal carries amm_address
        //    • PumpFun curve   — live bonding curve
        //    • PumpFun AMM     — graduated PumpSwap pool
        //    • Jupiter         — everything else
        //
        //    SAFETY NET (audit B1): when a NATIVE route fails BEFORE
        //    anything reached the chain (tx build error, pre-sign
        //    simulation reject, RPC blip), fall back to the Jupiter
        //    aggregator path instead of dropping the trade. This makes
        //    the bot robust to external-contract drift — if a venue
        //    changes its account layout again, trades degrade to
        //    Jupiter instead of going dark.
        let route =
            route::select_for_token_with_hint(&sig.token_address, sig.amm_address.as_deref(), rpc)
                .await?;
        let route_label = route.label();
        let native_result = match route {
            SwapRoute::PumpFun(r) => {
                self.submit_pumpfun(sig.clone(), *r, rpc, relay, signer_keypair)
                    .await
            }
            SwapRoute::PumpFunAmm(r) => {
                self.submit_pumpfun_amm_buy(sig.clone(), *r, rpc, relay, signer_keypair)
                    .await
            }
            SwapRoute::Raydium(r) => {
                self.submit_raydium_buy(
                    sig.clone(),
                    r.amm_pubkey,
                    r.amm,
                    r.market,
                    r.reserves,
                    rpc,
                    relay,
                    signer_keypair,
                )
                .await
            }
            SwapRoute::Jupiter => {
                return self.submit_jupiter(sig, jup, relay, signer_keypair).await
            }
        };
        match native_result {
            Err(e) if jupiter_fallback_eligible(&e) => {
                tracing::warn!(
                    venue = route_label,
                    mint = %sig.token_address,
                    error = %e,
                    "NATIVE ROUTE FAILED pre-submit — falling back to Jupiter"
                );
                self.submit_jupiter(sig, jup, relay, signer_keypair).await
            }
            other => other,
        }
    }

    /// Native PumpFun path: build → allowlist → simulate → sign → relay.
    async fn submit_pumpfun(
        &mut self,
        sig: Signal,
        r: PumpFunRoute,
        rpc: &RpcClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let user = signer_keypair.pubkey();
        let mint = Pubkey::from_str(&sig.token_address)
            .map_err(|_| BotError::InvalidMint(sig.token_address.clone()))?;
        let curve = r.curve;

        // 1. Fetch recent blockhash. Done after the route lookup to
        //    keep it as fresh as possible before signing.
        let blockhash = self.recent_blockhash(rpc).await?;

        // 2. Build the unsigned v0 VersionedTransaction.
        let fee = self.fee_params_for(DexId::PumpFun, TradeSide::Buy);
        let mut params =
            self.cfg
                .pumpfun_buy_params(user, mint, &curve, r.token_program, blockhash, fee);
        params.skip_token_ata_create = self.ata_known(&user, &mint);
        let unsigned_bytes = pumpfun::build_buy_tx(&params)?;
        let unsigned_b64 = base64::engine::general_purpose::STANDARD.encode(&unsigned_bytes);

        // 3. Allowlist check on the pre-sign tx. PumpFun + compute-
        //    budget + ATA + token + system programs all live in the
        //    default allowlist; nothing extra to register.
        if !self.cfg.skip_allowlist {
            let tx = bincode::deserialize(&unsigned_bytes)
                .map_err(|e| BotError::Sign(crate::signer::SignError::Bincode(e.to_string())))?;
            self.allowlist.check_tx(&tx)?;
        }

        // 4. Simulate against the user's RPC. Catches insufficient SOL,
        //    closed bonding curve (race with graduation), slippage
        //    violations from the curve drifting between route lookup
        //    and submit. The skipped-counter is NOT bumped here — the
        //    dispatch layer may still rescue the trade via Jupiter.
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&unsigned_b64, false).await?;
            if outcome.would_fail {
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        // 5. Sign locally.
        let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, signer_keypair)?;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        // 6. Create intent + submit. `quote_snapshot` carries the live
        //    curve reserves so the audit log can replay how we sized
        //    the trade (no Jupiter quote on this path).
        let quote_snapshot = serde_json::json!({
            "route": "pumpfun",
            "virtual_sol_reserves": curve.virtual_sol_reserves,
            "virtual_token_reserves": curve.virtual_token_reserves,
            "sol_in_lamports": self.cfg.per_trade_lamports,
            "slippage_bps": self.cfg.slippage_bps,
            "token_program": r.token_program.to_string(),
        });
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "buy".into(),
                    input_mint: self.cfg.input_mint.clone(),
                    output_mint: sig.token_address.clone(),
                    amount_in_lamports: self.cfg.per_trade_lamports as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(quote_snapshot),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token: Some(client_token(&sig.call_id, "buy")),
                },
                &signed_b64,
                relay,
            )
            .await?;

        // 7. Mark the token ATA as known so subsequent buys can skip
        //    the redundant CreateIdempotent. Best-effort; failures
        //    later in confirmation flow do not invalidate here (the
        //    submit itself succeeded, which is the relevant signal).
        self.mark_atas_known(user, &[mint]);

        // 8. Record spend.
        self.budget.record_at(
            &sig.token_address,
            self.cfg.per_trade_lamports,
            Instant::now(),
        );
        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }

    /// Jupiter aggregator path: quote → swap → allowlist → simulate → sign → relay.
    async fn submit_jupiter(
        &mut self,
        sig: Signal,
        jup: &JupiterClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        // 1. Jupiter quote + swap.
        let quote = jup
            .quote(
                &self.cfg.input_mint,
                &sig.token_address,
                self.cfg.per_trade_lamports,
                self.cfg.slippage_bps,
            )
            .await?;
        let swap = jup
            .swap(
                &quote,
                &signer_keypair.pubkey().to_string(),
                SwapOptions {
                    wrap_unwrap_sol: true,
                    priority_fee_lamports: None,
                },
            )
            .await?;

        // 2. Allowlist (decode unsigned tx, check program ids).
        if !self.cfg.skip_allowlist {
            let tx = decode_jupiter_tx_b64(&swap.swap_transaction)?;
            self.allowlist.check_tx(&tx)?;
        }

        // 3. Simulate.
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&swap.swap_transaction, false).await?;
            if outcome.would_fail {
                self.skipped_count += 1;
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        // 4. Sign.
        let signed_b64 = sign_jupiter_tx_b64(&swap.swap_transaction, signer_keypair)?;

        // 5. Create intent + submit.
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "buy".into(),
                    input_mint: self.cfg.input_mint.clone(),
                    output_mint: sig.token_address.clone(),
                    amount_in_lamports: self.cfg.per_trade_lamports as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(serde_json::to_value(&quote).unwrap_or_default()),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token: Some(client_token(&sig.call_id, "buy")),
                },
                &signed_b64,
                relay,
            )
            .await?;

        // 6. Record spend.
        self.budget.record_at(
            &sig.token_address,
            self.cfg.per_trade_lamports,
            Instant::now(),
        );
        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }

    /// Execute a BUY of `mint` for exactly `sol_in_lamports` — a direct
    /// command (copy-trade execution), not a preset-matched signal.
    ///
    /// Internally this routes through [`Self::handle_one`] with a
    /// synthetic [`Signal`], so the trade takes the EXACT same path as
    /// every preset-driven buy: route dispatch (PumpFun curve /
    /// PumpFun-AMM / Raydium / Jupiter) → program allowlist → pre-sign
    /// simulation → local sign → gateway relay submit. The deliberate
    /// deltas vs a signal buy:
    ///   * the per-trade size + slippage come from the command (the
    ///     copy config), not the static [`BotConfig`] — both are
    ///     restored after the call;
    ///   * `dedupe_id` (the backend copy-intent UUID) feeds the
    ///     engine's seen-set, so a WS redelivery can't double-buy;
    ///   * matcher + budget still apply: the copy loop's matcher is
    ///     permissive by construction, and the budget caps are the
    ///     operator's client-side safety net on top of the server's
    ///     position cap.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_buy(
        &mut self,
        dedupe_id: String,
        mint: String,
        sol_in_lamports: u64,
        slippage_bps: Option<u16>,
        amm_hint: Option<String>,
        jup: &JupiterClient,
        relay: &RelayClient,
        rpc: &RpcClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let saved_lamports = self.cfg.per_trade_lamports;
        let saved_slippage = self.cfg.slippage_bps;
        self.cfg.per_trade_lamports = sol_in_lamports;
        if let Some(bps) = slippage_bps {
            self.cfg.slippage_bps = bps;
        }
        let sig = Signal {
            call_id: dedupe_id,
            chain_id: 1,
            token_address: mint,
            symbol: None,
            price_usd: None,
            market_cap_usd: None,
            liquidity_usd: None,
            called_at: Utc::now(),
            matched_preset_id: "copytrade".into(),
            amm_address: amm_hint,
        };
        let res = self.handle_one(sig, jup, relay, rpc, signer_keypair).await;
        // Restore the session config regardless of outcome — `?` would
        // leak the override into the next trade.
        self.cfg.per_trade_lamports = saved_lamports;
        self.cfg.slippage_bps = saved_slippage;
        res
    }

    /// Execute a SELL on `token_mint` for `token_in_amount` base
    /// units. Unlike `handle_one`, sells are direct commands (not
    /// signal-driven) so they bypass the matcher, dedup, and budget
    /// gates. Caller (TP/SL engine, manual UI, CLI) is responsible
    /// for sizing the amount.
    ///
    /// Thin wrapper around [`Self::execute_sell_with_dedupe`] with no
    /// dedupe id — kept signature-stable for existing transports.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_sell(
        &mut self,
        token_mint: String,
        token_in_amount: u64,
        amm_hint: Option<String>,
        jup: &JupiterClient,
        relay: &RelayClient,
        rpc: &RpcClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        self.execute_sell_with_dedupe(
            None,
            token_mint,
            token_in_amount,
            amm_hint,
            jup,
            relay,
            rpc,
            signer_keypair,
        )
        .await
    }

    /// Full sell entry point.
    ///
    /// * `dedupe_id` — stable id of the TRIGGERING event (TP/SL
    ///   `target_id`, copy `intent_id`). When set, the created intent
    ///   carries a deterministic `client_token` so the gateway dedups
    ///   the same event executed by two unlocked devices (audit M4).
    /// * `amm_hint` optionally supplies a Raydium AMM v4 pool address —
    ///   pass `Some(addr)` when the position was opened via the Raydium
    ///   path so we can sell on the same venue without an extra PumpFun
    ///   lookup. Pass `None` for PumpFun positions or unknown venues.
    ///
    /// SELL ROBUSTNESS (audit M10, client side): before building the
    /// swap, the requested amount is clamped to the wallet's ACTUAL
    /// on-chain token balance — the gateway may send pooled
    /// cross-wallet amounts. A zero/missing balance returns
    /// `Decision::Skipped` with a loud log instead of a doomed
    /// oversized sell.
    ///
    /// Route dispatch mirrors `handle_one`: Raydium (if hinted) →
    /// live PumpFun curve → PumpFun-AMM → Jupiter, with the same
    /// Jupiter fallback when a native route fails pre-submit.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_sell_with_dedupe(
        &mut self,
        dedupe_id: Option<String>,
        token_mint: String,
        token_in_amount: u64,
        amm_hint: Option<String>,
        jup: &JupiterClient,
        relay: &RelayClient,
        rpc: &RpcClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let client_token = dedupe_id.as_deref().map(|id| client_token(id, "sell"));

        // M10 clamp: sell min(requested, on-chain balance). Balance
        // probe failures degrade to the requested amount (the pre-sign
        // simulation still guards an oversize) — only a CONFIRMED
        // zero/missing balance skips the trade.
        let token_in_amount = match self
            .onchain_token_balance(&token_mint, rpc, &signer_keypair.pubkey())
            .await
        {
            Ok(Some(balance)) => {
                if balance == 0 {
                    tracing::warn!(mint = %token_mint, requested = token_in_amount,
                        "sell skipped — wallet holds ZERO balance of this token");
                    self.skipped_count += 1;
                    return Ok(Decision::Skipped(
                        "wallet holds zero balance of this token".into(),
                    ));
                }
                if balance < token_in_amount {
                    tracing::warn!(mint = %token_mint, requested = token_in_amount, balance,
                        "sell amount clamped to on-chain balance (gateway may aggregate cross-wallet)");
                }
                token_in_amount.min(balance)
            }
            Ok(None) => {
                tracing::warn!(mint = %token_mint, requested = token_in_amount,
                    "sell skipped — wallet has no token account for this mint");
                self.skipped_count += 1;
                return Ok(Decision::Skipped(
                    "wallet has no token account for this mint".into(),
                ));
            }
            Err(e) => {
                tracing::warn!(mint = %token_mint, error = %e,
                    "balance probe failed — proceeding with requested sell amount");
                token_in_amount
            }
        };

        let route =
            route::select_for_token_with_hint(&token_mint, amm_hint.as_deref(), rpc).await?;
        let route_label = route.label();
        let native_result = match route {
            SwapRoute::PumpFun(r) => {
                self.submit_sell_pumpfun(
                    token_mint.clone(),
                    token_in_amount,
                    *r,
                    client_token.clone(),
                    rpc,
                    relay,
                    signer_keypair,
                )
                .await
            }
            SwapRoute::PumpFunAmm(r) => {
                self.submit_pumpfun_amm_sell(
                    token_mint.clone(),
                    token_in_amount,
                    *r,
                    client_token.clone(),
                    rpc,
                    relay,
                    signer_keypair,
                )
                .await
            }
            SwapRoute::Raydium(r) => {
                self.submit_raydium_sell(
                    token_mint.clone(),
                    token_in_amount,
                    r.amm_pubkey,
                    r.amm,
                    r.market,
                    r.reserves,
                    client_token.clone(),
                    rpc,
                    relay,
                    signer_keypair,
                )
                .await
            }
            SwapRoute::Jupiter => {
                return self
                    .submit_sell_jupiter(
                        token_mint,
                        token_in_amount,
                        client_token,
                        jup,
                        relay,
                        signer_keypair,
                    )
                    .await
            }
        };
        match native_result {
            Err(e) if jupiter_fallback_eligible(&e) => {
                tracing::warn!(
                    venue = route_label,
                    mint = %token_mint,
                    error = %e,
                    "NATIVE SELL ROUTE FAILED pre-submit — falling back to Jupiter"
                );
                self.submit_sell_jupiter(
                    token_mint,
                    token_in_amount,
                    client_token,
                    jup,
                    relay,
                    signer_keypair,
                )
                .await
            }
            other => other,
        }
    }

    /// Fetch the wallet's raw on-chain balance for `token_mint`.
    /// `Ok(None)` = the ATA does not exist. The mint's owner program
    /// is resolved first so Token-2022 ATAs probe the right address.
    async fn onchain_token_balance(
        &self,
        token_mint: &str,
        rpc: &RpcClient,
        user: &Pubkey,
    ) -> Result<Option<u64>, BotError> {
        let mint =
            Pubkey::from_str(token_mint).map_err(|_| BotError::InvalidMint(token_mint.into()))?;
        let token_program = match rpc.get_account_owner(&mint).await? {
            Some(owner) => owner,
            None => ata::TOKEN_PROGRAM_ID,
        };
        let user_ata = ata::derive_with_program(user, &mint, &token_program);
        Ok(rpc.get_token_account_balance(&user_ata).await?)
    }

    /// Native PumpFun sell. Mirrors `submit_pumpfun` (buy) but uses
    /// `build_sell_tx` + tags the intent with `side="sell"`.
    #[allow(clippy::too_many_arguments)]
    async fn submit_sell_pumpfun(
        &mut self,
        token_mint: String,
        token_in_amount: u64,
        r: PumpFunRoute,
        client_token: Option<String>,
        rpc: &RpcClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let user = signer_keypair.pubkey();
        let mint =
            Pubkey::from_str(&token_mint).map_err(|_| BotError::InvalidMint(token_mint.clone()))?;
        let curve = r.curve;

        let blockhash = self.recent_blockhash(rpc).await?;
        let fee = self.fee_params_for(DexId::PumpFun, TradeSide::Sell);
        let params = self.cfg.pumpfun_sell_params(
            user,
            mint,
            token_in_amount,
            &curve,
            r.token_program,
            blockhash,
            fee,
        );
        let unsigned_bytes = pumpfun::build_sell_tx(&params)?;
        let unsigned_b64 = base64::engine::general_purpose::STANDARD.encode(&unsigned_bytes);

        if !self.cfg.skip_allowlist {
            let tx = bincode::deserialize(&unsigned_bytes)
                .map_err(|e| BotError::Sign(crate::signer::SignError::Bincode(e.to_string())))?;
            self.allowlist.check_tx(&tx)?;
        }
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&unsigned_b64, false).await?;
            if outcome.would_fail {
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, signer_keypair)?;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        // For sells the audit log carries token-in + curve reserves +
        // the expected SOL output the bonding-curve math implies at
        // submit-time. Position-tracker on the gateway uses this to
        // size the unrealised-pnl flip.
        let quote_snapshot = serde_json::json!({
            "route": "pumpfun",
            "side": "sell",
            "virtual_sol_reserves": curve.virtual_sol_reserves,
            "virtual_token_reserves": curve.virtual_token_reserves,
            "token_in_amount": token_in_amount,
            "slippage_bps": self.cfg.slippage_bps,
        });
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "sell".into(),
                    input_mint: token_mint.clone(),
                    output_mint: self.cfg.input_mint.clone(), // WSOL
                    amount_in_lamports: token_in_amount as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(quote_snapshot),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token,
                },
                &signed_b64,
                relay,
            )
            .await?;

        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }

    /// Jupiter sell path: token → SOL via aggregator quote.
    async fn submit_sell_jupiter(
        &mut self,
        token_mint: String,
        token_in_amount: u64,
        client_token: Option<String>,
        jup: &JupiterClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        // Quote the reverse direction. Input = the token we hold;
        // output = the session's WSOL.
        let quote = jup
            .quote(
                &token_mint,
                &self.cfg.input_mint,
                token_in_amount,
                self.cfg.slippage_bps,
            )
            .await?;
        let swap = jup
            .swap(
                &quote,
                &signer_keypair.pubkey().to_string(),
                SwapOptions {
                    wrap_unwrap_sol: true,
                    priority_fee_lamports: None,
                },
            )
            .await?;

        if !self.cfg.skip_allowlist {
            let tx = decode_jupiter_tx_b64(&swap.swap_transaction)?;
            self.allowlist.check_tx(&tx)?;
        }
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&swap.swap_transaction, false).await?;
            if outcome.would_fail {
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        let signed_b64 = sign_jupiter_tx_b64(&swap.swap_transaction, signer_keypair)?;
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "sell".into(),
                    input_mint: token_mint,
                    output_mint: self.cfg.input_mint.clone(),
                    amount_in_lamports: token_in_amount as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(serde_json::to_value(&quote).unwrap_or_default()),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token,
                },
                &signed_b64,
                relay,
            )
            .await?;

        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }

    // ── PumpFun-AMM (Pumpswap) native paths ──────────────────────
    //
    // Same pattern as classic-PumpFun: route selector decides the
    // venue + carries the state, these methods just build + sign +
    // relay. Slippage applies to BOTH base + quote bounds because
    // the on-chain `buy` checks `base_amount_out` literally + the
    // `max_quote_amount_in` separately — we widen the quote side so
    // a small reserve drift between route lookup + submit doesn't
    // revert. Sells: `base_amount_in` is the exact-in side, slippage
    // applies to the min-out only.

    #[allow(clippy::too_many_arguments)]
    async fn submit_pumpfun_amm_buy(
        &mut self,
        sig: Signal,
        r: PumpFunAmmRoute,
        rpc: &RpcClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let user = signer_keypair.pubkey();
        let pool = r.pool;
        let reserves = r.reserves;
        let blockhash = self.recent_blockhash(rpc).await?;
        let fee = self.fee_params_for(DexId::PumpFunAmm, TradeSide::Buy);
        let params = AmmBuyTxParams {
            user,
            pool: r.pool_pubkey,
            base_mint: pool.base_mint,
            quote_mint: pool.quote_mint,
            base_token_program: r.base_token_program,
            quote_token_program: r.quote_token_program,
            coin_creator: pool.coin_creator,
            quote_in_amount: self.cfg.per_trade_lamports,
            slippage_bps: self.cfg.slippage_bps,
            reserves,
            recent_blockhash: blockhash,
            compute_unit_limit: fee.cu_limit,
            compute_unit_price_micro_lamports: fee.cu_price_micro_lamports,
            tip_provider: self.cfg.tip_provider(),
            tip_lamports: self.cfg.tip_lamports_u64(),
            tip_selector: TipSelector::from_blockhash(&blockhash),
            skip_base_ata_create: self.ata_known(&user, &pool.base_mint),
            // Ignored for WSOL quotes — the builder wraps + closes.
            skip_quote_ata_create: false,
        };
        let unsigned_bytes = pumpfun_amm::build_buy_tx(&params)
            .map_err(|e| BotError::BuildPumpFunAmmTx(e.to_string()))?;
        let unsigned_b64 = base64::engine::general_purpose::STANDARD.encode(&unsigned_bytes);

        if !self.cfg.skip_allowlist {
            let tx = bincode::deserialize(&unsigned_bytes)
                .map_err(|e| BotError::Sign(crate::signer::SignError::Bincode(e.to_string())))?;
            self.allowlist.check_tx(&tx)?;
        }
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&unsigned_b64, false).await?;
            if outcome.would_fail {
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, signer_keypair)?;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        let quote_snapshot = serde_json::json!({
            "route": "pumpfun_amm",
            "side": "buy",
            "pool": r.pool_pubkey.to_string(),
            "base_reserve": reserves.base,
            "quote_reserve": reserves.quote,
            "quote_in_lamports": self.cfg.per_trade_lamports,
            "slippage_bps": self.cfg.slippage_bps,
        });
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "buy".into(),
                    input_mint: self.cfg.input_mint.clone(),
                    output_mint: sig.token_address.clone(),
                    amount_in_lamports: self.cfg.per_trade_lamports as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(quote_snapshot),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token: Some(client_token(&sig.call_id, "buy")),
                },
                &signed_b64,
                relay,
            )
            .await?;

        // Only the base-token ATA persists — the WSOL ATA is closed at
        // the end of the swap tx, so it must never enter the cache.
        self.mark_atas_known(user, &[pool.base_mint]);

        self.budget.record_at(
            &sig.token_address,
            self.cfg.per_trade_lamports,
            Instant::now(),
        );
        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }

    // ── Raydium AMM v4 native paths ──────────────────────────────────
    //
    // Same structural pattern as PumpFun-AMM: route selector pre-fetches
    // all on-chain state (AMM + market + vault balances) and passes it
    // in here. These methods do zero RPC calls except for the blockhash.
    //
    // CU budget: Raydium v4 swaps invoke the OpenBook CPI (bids/asks/
    // event-queue reads) → higher CU than PumpSwap. Observed on-chain
    // ~150k–190k; 220k buy / 180k sell gives comfortable headroom.

    #[allow(clippy::too_many_arguments)]
    async fn submit_raydium_buy(
        &mut self,
        sig: Signal,
        amm_pubkey: Pubkey,
        amm: raydium_amm_v4::AmmState,
        market: raydium_amm_v4::MarketState,
        reserves: raydium_amm_v4::PoolReserves,
        rpc: &RpcClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let user = signer_keypair.pubkey();
        let blockhash = self.recent_blockhash(rpc).await?;
        let fee = self.fee_params_for(DexId::RaydiumAmmV4, TradeSide::Buy);
        let coin_mint = amm.coin_mint;
        let params = raydium_amm_v4::BuyTxParams {
            user,
            amm_pubkey,
            amm,
            market,
            reserves,
            quote_in_lamports: self.cfg.per_trade_lamports,
            slippage_bps: self.cfg.slippage_bps,
            recent_blockhash: blockhash,
            compute_unit_limit: fee.cu_limit,
            compute_unit_price_micro_lamports: fee.cu_price_micro_lamports,
            tip_provider: self.cfg.tip_provider(),
            tip_lamports: self.cfg.tip_lamports_u64(),
            tip_selector: TipSelector::from_blockhash(&blockhash),
            skip_coin_ata_create: self.ata_known(&user, &coin_mint),
            // Ignored for WSOL quotes — the builder wraps + closes.
            skip_pc_ata_create: false,
        };
        let unsigned_bytes = raydium_amm_v4::build_buy_tx(&params)
            .map_err(|e| BotError::BuildRaydiumTx(e.to_string()))?;
        let unsigned_b64 = base64::engine::general_purpose::STANDARD.encode(&unsigned_bytes);

        if !self.cfg.skip_allowlist {
            let tx = bincode::deserialize(&unsigned_bytes)
                .map_err(|e| BotError::Sign(crate::signer::SignError::Bincode(e.to_string())))?;
            self.allowlist.check_tx(&tx)?;
        }
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&unsigned_b64, false).await?;
            if outcome.would_fail {
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, signer_keypair)?;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        let quote_snapshot = serde_json::json!({
            "route": "raydium_amm_v4",
            "side": "buy",
            "amm": amm_pubkey.to_string(),
            "coin_reserve": reserves.coin,
            "pc_reserve": reserves.pc,
            "quote_in_lamports": self.cfg.per_trade_lamports,
            "slippage_bps": self.cfg.slippage_bps,
        });
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "buy".into(),
                    input_mint: self.cfg.input_mint.clone(),
                    output_mint: sig.token_address.clone(),
                    amount_in_lamports: self.cfg.per_trade_lamports as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(quote_snapshot),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token: Some(client_token(&sig.call_id, "buy")),
                },
                &signed_b64,
                relay,
            )
            .await?;

        // Only the coin ATA persists — the WSOL ATA is closed at the
        // end of the swap tx, so it must never enter the cache.
        self.mark_atas_known(user, &[coin_mint]);

        self.budget.record_at(
            &sig.token_address,
            self.cfg.per_trade_lamports,
            Instant::now(),
        );
        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_raydium_sell(
        &mut self,
        token_mint: String,
        token_in_amount: u64,
        amm_pubkey: Pubkey,
        amm: raydium_amm_v4::AmmState,
        market: raydium_amm_v4::MarketState,
        reserves: raydium_amm_v4::PoolReserves,
        client_token: Option<String>,
        rpc: &RpcClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let user = signer_keypair.pubkey();
        let blockhash = self.recent_blockhash(rpc).await?;
        let fee = self.fee_params_for(DexId::RaydiumAmmV4, TradeSide::Sell);
        let params = raydium_amm_v4::SellTxParams {
            user,
            amm_pubkey,
            amm,
            market,
            reserves,
            coin_in_amount: token_in_amount,
            slippage_bps: self.cfg.slippage_bps,
            recent_blockhash: blockhash,
            compute_unit_limit: fee.cu_limit,
            compute_unit_price_micro_lamports: fee.cu_price_micro_lamports,
            tip_provider: self.cfg.tip_provider(),
            tip_lamports: self.cfg.tip_lamports_u64(),
            tip_selector: TipSelector::from_blockhash(&blockhash),
            // Ignored for WSOL proceeds — the builder creates + closes.
            skip_pc_ata_create: false,
        };
        let unsigned_bytes = raydium_amm_v4::build_sell_tx(&params)
            .map_err(|e| BotError::BuildRaydiumTx(e.to_string()))?;
        let unsigned_b64 = base64::engine::general_purpose::STANDARD.encode(&unsigned_bytes);

        if !self.cfg.skip_allowlist {
            let tx = bincode::deserialize(&unsigned_bytes)
                .map_err(|e| BotError::Sign(crate::signer::SignError::Bincode(e.to_string())))?;
            self.allowlist.check_tx(&tx)?;
        }
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&unsigned_b64, false).await?;
            if outcome.would_fail {
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, signer_keypair)?;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        let quote_snapshot = serde_json::json!({
            "route": "raydium_amm_v4",
            "side": "sell",
            "amm": amm_pubkey.to_string(),
            "coin_reserve": reserves.coin,
            "pc_reserve": reserves.pc,
            "coin_in_amount": token_in_amount,
            "slippage_bps": self.cfg.slippage_bps,
        });
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "sell".into(),
                    input_mint: token_mint,
                    output_mint: self.cfg.input_mint.clone(),
                    amount_in_lamports: token_in_amount as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(quote_snapshot),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token,
                },
                &signed_b64,
                relay,
            )
            .await?;

        // The WSOL proceeds ATA is closed inside the swap tx — do NOT
        // mark it known or a later buy would skip its re-creation.

        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_pumpfun_amm_sell(
        &mut self,
        token_mint: String,
        token_in_amount: u64,
        r: PumpFunAmmRoute,
        client_token: Option<String>,
        rpc: &RpcClient,
        relay: &RelayClient,
        signer_keypair: &Keypair,
    ) -> Result<Decision, BotError> {
        let user = signer_keypair.pubkey();
        let pool = r.pool;
        let reserves = r.reserves;
        let pool_pubkey = r.pool_pubkey;
        let blockhash = self.recent_blockhash(rpc).await?;
        let fee = self.fee_params_for(DexId::PumpFunAmm, TradeSide::Sell);
        let quote_mint = pool.quote_mint;
        let params = AmmSellTxParams {
            user,
            pool: pool_pubkey,
            base_mint: pool.base_mint,
            quote_mint,
            base_token_program: r.base_token_program,
            quote_token_program: r.quote_token_program,
            coin_creator: pool.coin_creator,
            base_in_amount: token_in_amount,
            slippage_bps: self.cfg.slippage_bps,
            reserves,
            recent_blockhash: blockhash,
            compute_unit_limit: fee.cu_limit,
            compute_unit_price_micro_lamports: fee.cu_price_micro_lamports,
            tip_provider: self.cfg.tip_provider(),
            tip_lamports: self.cfg.tip_lamports_u64(),
            tip_selector: TipSelector::from_blockhash(&blockhash),
            // Ignored for WSOL proceeds — the builder creates + closes.
            skip_quote_ata_create: false,
        };
        let unsigned_bytes = pumpfun_amm::build_sell_tx(&params)
            .map_err(|e| BotError::BuildPumpFunAmmTx(e.to_string()))?;
        let unsigned_b64 = base64::engine::general_purpose::STANDARD.encode(&unsigned_bytes);

        if !self.cfg.skip_allowlist {
            let tx = bincode::deserialize(&unsigned_bytes)
                .map_err(|e| BotError::Sign(crate::signer::SignError::Bincode(e.to_string())))?;
            self.allowlist.check_tx(&tx)?;
        }
        if !self.cfg.skip_simulate {
            let sim = Simulator::new(self.cfg.rpc_url.clone());
            let outcome: SimulationOutcome = sim.simulate(&unsigned_b64, false).await?;
            if outcome.would_fail {
                return Err(BotError::SimulationRejected(
                    outcome.failure_reason.unwrap_or_default(),
                ));
            }
        }

        let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, signer_keypair)?;
        let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);

        let quote_snapshot = serde_json::json!({
            "route": "pumpfun_amm",
            "side": "sell",
            "pool": pool_pubkey.to_string(),
            "base_reserve": reserves.base,
            "quote_reserve": reserves.quote,
            "base_in_amount": token_in_amount,
            "slippage_bps": self.cfg.slippage_bps,
        });
        let resp = self
            .create_or_submit_intent(
                CreateIntentReq {
                    side: "sell".into(),
                    input_mint: token_mint,
                    output_mint: self.cfg.input_mint.clone(),
                    amount_in_lamports: token_in_amount as i64,
                    slippage_bps: Some(self.cfg.slippage_bps as i32),
                    submit_mode: Some(self.cfg.submit_mode.clone()),
                    tip_lamports: Some(self.cfg.tip_lamports),
                    preset_id: self.cfg.preset_id.clone(),
                    bot_session_id: self.cfg.bot_session_id.clone(),
                    quote_snapshot: Some(quote_snapshot),
                    copy_config_id: self.copy_config_id.clone(),
                    client_token,
                },
                &signed_b64,
                relay,
            )
            .await?;

        // The WSOL proceeds ATA is closed inside the swap tx — do NOT
        // mark it known or a later buy would skip its re-creation.

        self.submitted_count += 1;
        Ok(Decision::Submitted(resp))
    }
}

/// Deterministic idempotency token for `CreateIntentReq.client_token`
/// (audit M4). Derived from the triggering event id so two devices
/// executing the SAME event produce the SAME token and the gateway
/// dedups intent creation. Truncated to the gateway's 100-char cap
/// (longer tokens are silently ignored server-side).
fn client_token(event_key: &str, side: &str) -> String {
    let mut t = format!("dbx:{side}:{event_key}");
    t.truncate(100);
    t
}

/// Which native-route failures are safe to retry via Jupiter: only
/// errors that happen strictly BEFORE anything reached the gateway or
/// the chain. Relay/sign errors are NOT eligible — the native tx may
/// already be in flight, and a Jupiter retry would double-execute.
fn jupiter_fallback_eligible(e: &BotError) -> bool {
    matches!(
        e,
        BotError::SimulationRejected(_)
            | BotError::BuildPumpFunTx(_)
            | BotError::BuildPumpFunAmmTx(_)
            | BotError::BuildRaydiumTx(_)
            | BotError::Rpc(_)
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct BotStats {
    pub total_seen: u64,
    pub submitted: u64,
    pub skipped: u64,
    pub budget_remaining_lamports: u64,
    /// Total lamports this engine has spent on buys this session.
    /// Unlike `budget_remaining_lamports` it stays meaningful when the
    /// session budget is effectively unlimited (per-config budgets are
    /// enforced server-side since v0.3.0 slice 6/8).
    pub budget_spent_lamports: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn matcher_defaults() -> PresetMatcher {
        PresetMatcher {
            min_mcap_usd: Some(100_000.0),
            max_mcap_usd: Some(1_000_000_000.0),
            min_liquidity_usd: Some(10_000.0),
            max_age_secs: Some(300),
            blocked_tokens: HashSet::new(),
        }
    }

    fn sig(token: &str, mcap: f64, liq: f64, age_secs: i64) -> Signal {
        let now = Utc::now();
        Signal {
            call_id: format!("call-{token}-{age_secs}"),
            chain_id: 1,
            token_address: token.into(),
            symbol: Some("TOK".into()),
            price_usd: Some(1.0),
            market_cap_usd: Some(mcap),
            liquidity_usd: Some(liq),
            called_at: now - Duration::seconds(age_secs),
            matched_preset_id: "preset-1".into(),
            amm_address: None,
        }
    }

    #[test]
    fn matcher_accepts_within_range() {
        let m = matcher_defaults();
        let s = sig("TOKEN1", 5_000_000.0, 50_000.0, 30);
        assert!(m.evaluate(&s, Utc::now()).is_ok());
    }

    #[test]
    fn matcher_rejects_low_mcap() {
        let m = matcher_defaults();
        let s = sig("TOKEN1", 50_000.0, 50_000.0, 30);
        let err = m.evaluate(&s, Utc::now()).unwrap_err();
        assert!(err.contains("mcap"));
    }

    #[test]
    fn matcher_rejects_low_liquidity() {
        let m = matcher_defaults();
        let s = sig("TOKEN1", 5_000_000.0, 500.0, 30);
        assert!(m.evaluate(&s, Utc::now()).unwrap_err().contains("liq"));
    }

    #[test]
    fn matcher_rejects_stale_signal() {
        let m = matcher_defaults();
        let s = sig("TOKEN1", 5_000_000.0, 50_000.0, 600);
        assert!(m.evaluate(&s, Utc::now()).unwrap_err().contains("age"));
    }

    #[test]
    fn matcher_blocklist_wins_over_other_criteria() {
        let mut m = matcher_defaults();
        m.blocked_tokens.insert("BLOCKED_TOKEN".into());
        let s = sig("BLOCKED_TOKEN", 5_000_000.0, 50_000.0, 30);
        let err = m.evaluate(&s, Utc::now()).unwrap_err();
        assert!(err.contains("blocklist"));
    }

    #[test]
    fn matcher_handles_missing_optional_fields() {
        let m = matcher_defaults();
        // If price/mcap/liq missing on the wire, we don't reject —
        // only fail when both the rule AND the field are present.
        // (Production: caller can decide to tighten this.)
        let mut s = sig("TOKEN1", 5_000_000.0, 50_000.0, 30);
        s.market_cap_usd = None;
        s.liquidity_usd = None;
        assert!(m.evaluate(&s, Utc::now()).is_ok());
    }

    // ─── budget + dedup tests (no network) ─────────────────────────

    // We can't easily test the full `handle_one` without mocking
    // reqwest, but we CAN exercise the budget + dedup paths because
    // they short-circuit before any I/O. The tests below construct
    // a BotEngine, manually fire the early checks, and assert state
    // transitions.
    fn test_engine(budget_lamports: u64) -> BotEngine {
        use crate::budget::BudgetConfig;
        BotEngine::new(
            matcher_defaults(),
            BudgetState::new(BudgetConfig {
                session_budget_lamports: budget_lamports,
                per_token_cap_lamports: None,
                per_hour_cap_lamports: None,
            }),
            crate::default_allowlist().unwrap(),
            BotConfig {
                per_trade_lamports: 100_000_000,
                slippage_bps: 100,
                tip_lamports: 1_000_000,
                submit_mode: "falcon_jito".into(),
                rpc_url: "http://localhost".into(),
                skip_simulate: true,
                skip_allowlist: true,
                input_mint: "So11111111111111111111111111111111111111112".into(),
                pumpfun_cu_limit: 120_000,
                pumpfun_cu_price_micro_lamports: 50_000,
                bot_session_id: None,
                preset_id: None,
            },
        )
    }

    #[test]
    fn budget_check_inline_before_network_io() {
        // Spend the entire budget then ensure check() fails BEFORE
        // any reqwest call would happen — i.e. handle_one's budget
        // check is the first guard after dedup.
        let engine = test_engine(50_000_000); // smaller than per-trade
        let _ = engine
            .budget
            .check("TOK", 100_000_000)
            .expect_err("should fail — per-trade > budget");
        // Stats unchanged because we tested the underlying state
        // directly, not via handle_one.
        assert_eq!(engine.stats().total_seen, 0);
    }

    #[test]
    fn dedup_set_records_call_id() {
        let mut engine = test_engine(1_000_000_000);
        assert!(engine.seen.insert("call-A".into()));
        assert!(!engine.seen.insert("call-A".into()));
    }

    #[test]
    fn engine_defaults_without_blockhash_cache() {
        let engine = test_engine(1_000_000_000);
        assert!(engine.blockhash_cache.is_none());
    }

    #[test]
    fn with_blockhash_cache_attaches_handle() {
        use crate::blockhash_cache::BlockhashCache;
        let cache = BlockhashCache::new_inert(RpcClient::new("http://stub"));
        let engine = test_engine(1_000_000_000).with_blockhash_cache(cache);
        assert!(engine.blockhash_cache.is_some());
    }

    #[test]
    fn engine_defaults_without_ata_cache() {
        let engine = test_engine(1_000_000_000);
        assert!(engine.ata_cache.is_none());
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        // Without a cache the engine always answers "unknown" so the
        // safe default of including the create-IX is preserved.
        assert!(!engine.ata_known(&owner, &mint));
    }

    #[test]
    fn with_ata_cache_attaches_handle_and_flows_through() {
        use crate::ata_cache::AtaCache;
        let cache = AtaCache::new();
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        cache.mark_known(owner, mint);
        let engine = test_engine(1_000_000_000).with_ata_cache(cache);
        assert!(engine.ata_cache.is_some());
        assert!(engine.ata_known(&owner, &mint));
        let other = Pubkey::new_unique();
        assert!(!engine.ata_known(&owner, &other));
    }

    #[test]
    fn mark_atas_known_writes_through_cache() {
        use crate::ata_cache::AtaCache;
        let cache = AtaCache::new();
        let engine = test_engine(1_000_000_000).with_ata_cache(cache.clone());
        let owner = Pubkey::new_unique();
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        engine.mark_atas_known(owner, &[mint_a, mint_b]);
        assert!(cache.is_known(&owner, &mint_a));
        assert!(cache.is_known(&owner, &mint_b));
    }

    #[test]
    fn stats_starts_zeroed() {
        let engine = test_engine(1_000_000_000);
        let s = engine.stats();
        assert_eq!(s.total_seen, 0);
        assert_eq!(s.submitted, 0);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.budget_remaining_lamports, 1_000_000_000);
    }

    // ─── pumpfun build path (no network) ───────────────────────────
    //
    // We can't exercise `handle_one` end-to-end without a Jupiter +
    // RPC + relay mock, but the pure build helper on BotConfig is
    // exactly the math we want to guard against drift.

    #[test]
    fn pumpfun_buy_params_passes_through_curve_reserves() {
        let cfg = BotConfig {
            per_trade_lamports: 500_000_000,
            slippage_bps: 250,
            tip_lamports: 0,
            submit_mode: "falcon".into(),
            rpc_url: "http://localhost".into(),
            skip_simulate: true,
            skip_allowlist: true,
            input_mint: "So11111111111111111111111111111111111111112".into(),
            pumpfun_cu_limit: 150_000,
            pumpfun_cu_price_micro_lamports: 75_000,
            bot_session_id: None,
            preset_id: None,
        };
        use solana_sdk::pubkey;
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let curve = crate::dex::pumpfun::BondingCurveAccount {
            virtual_token_reserves: 900_000_000_000_000,
            virtual_sol_reserves: 35_000_000_000,
            real_token_reserves: 700_000_000_000_000,
            real_sol_reserves: 5_000_000_000,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: Pubkey::new_unique(),
        };
        let fee = FeeParams {
            cu_limit: cfg.pumpfun_cu_limit,
            cu_price_micro_lamports: cfg.pumpfun_cu_price_micro_lamports,
        };
        let params = cfg.pumpfun_buy_params(
            user,
            mint,
            &curve,
            crate::dex::ata::TOKEN_PROGRAM_ID,
            solana_sdk::hash::Hash::default(),
            fee,
        );
        assert_eq!(params.user, user);
        assert_eq!(params.mint, mint);
        assert_eq!(params.sol_in_lamports, 500_000_000);
        assert_eq!(params.slippage_bps, 250);
        assert_eq!(params.virtual_sol_reserves, 35_000_000_000);
        assert_eq!(params.virtual_token_reserves, 900_000_000_000_000);
        assert_eq!(params.compute_unit_limit, 150_000);
        assert_eq!(params.compute_unit_price_micro_lamports, 75_000);
    }

    #[test]
    fn pumpfun_buy_params_produces_buildable_tx() {
        // Sanity: the params we build out of a session config + live
        // curve actually feed `build_buy_tx` without errors.
        let cfg = BotConfig {
            per_trade_lamports: 100_000_000,
            slippage_bps: 100,
            tip_lamports: 0,
            submit_mode: "falcon".into(),
            rpc_url: "http://localhost".into(),
            skip_simulate: true,
            skip_allowlist: true,
            input_mint: "So11111111111111111111111111111111111111112".into(),
            pumpfun_cu_limit: 120_000,
            pumpfun_cu_price_micro_lamports: 50_000,
            bot_session_id: None,
            preset_id: None,
        };
        use solana_sdk::pubkey;
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let curve = crate::dex::pumpfun::BondingCurveAccount {
            virtual_token_reserves: crate::dex::pumpfun::INITIAL_VIRTUAL_TOKEN_RESERVES,
            virtual_sol_reserves: crate::dex::pumpfun::INITIAL_VIRTUAL_SOL_RESERVES,
            real_token_reserves: crate::dex::pumpfun::INITIAL_REAL_TOKEN_RESERVES,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: Pubkey::new_unique(),
        };
        let fee = FeeParams {
            cu_limit: cfg.pumpfun_cu_limit,
            cu_price_micro_lamports: cfg.pumpfun_cu_price_micro_lamports,
        };
        let params = cfg.pumpfun_buy_params(
            user,
            mint,
            &curve,
            crate::dex::ata::TOKEN_PROGRAM_ID,
            solana_sdk::hash::Hash::default(),
            fee,
        );
        let bytes = crate::dex::pumpfun::build_buy_tx(&params)
            .expect("build_buy_tx with session-derived params");
        // Round-trip back to the v0 message + assert ix count.
        let tx: solana_sdk::transaction::VersionedTransaction =
            bincode::deserialize(&bytes).unwrap();
        match &tx.message {
            solana_sdk::message::VersionedMessage::V0(msg) => {
                // cu-limit + cu-price + ATA + buy + Falcon tip = 5.
                // submit_mode="falcon" with tip_lamports=0 → the tip is
                // raised to the Falcon minimum and injected.
                assert_eq!(msg.instructions.len(), 5);
            }
            _ => panic!("expected v0"),
        }
    }

    // ── pumpfun sell path ────────────────────────────────────────

    #[test]
    fn pumpfun_sell_params_passes_through_curve_reserves() {
        let cfg = BotConfig {
            per_trade_lamports: 0, // not used for sells
            slippage_bps: 250,
            tip_lamports: 0,
            submit_mode: "falcon".into(),
            rpc_url: "http://localhost".into(),
            skip_simulate: true,
            skip_allowlist: true,
            input_mint: "So11111111111111111111111111111111111111112".into(),
            pumpfun_cu_limit: 100_000,
            pumpfun_cu_price_micro_lamports: 75_000,
            bot_session_id: None,
            preset_id: None,
        };
        use solana_sdk::pubkey;
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let curve = crate::dex::pumpfun::BondingCurveAccount {
            virtual_token_reserves: 900_000_000_000_000,
            virtual_sol_reserves: 35_000_000_000,
            real_token_reserves: 700_000_000_000_000,
            real_sol_reserves: 5_000_000_000,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: Pubkey::new_unique(),
        };
        let token_in = 12_345_678_900_u64;
        let fee = FeeParams {
            cu_limit: cfg.pumpfun_cu_limit,
            cu_price_micro_lamports: cfg.pumpfun_cu_price_micro_lamports,
        };
        let params = cfg.pumpfun_sell_params(
            user,
            mint,
            token_in,
            &curve,
            crate::dex::ata::TOKEN_PROGRAM_ID,
            solana_sdk::hash::Hash::default(),
            fee,
        );
        assert_eq!(params.token_in_amount, token_in);
        assert_eq!(params.slippage_bps, 250);
        assert_eq!(params.virtual_sol_reserves, 35_000_000_000);
        assert_eq!(params.virtual_token_reserves, 900_000_000_000_000);
        assert_eq!(params.compute_unit_limit, 100_000);
        assert_eq!(params.compute_unit_price_micro_lamports, 75_000);
    }

    #[test]
    fn pumpfun_sell_params_produces_buildable_tx_with_tip() {
        let cfg = BotConfig {
            per_trade_lamports: 0,
            slippage_bps: 100,
            tip_lamports: 0,
            submit_mode: "falcon".into(),
            rpc_url: "http://localhost".into(),
            skip_simulate: true,
            skip_allowlist: true,
            input_mint: "So11111111111111111111111111111111111111112".into(),
            pumpfun_cu_limit: 100_000,
            pumpfun_cu_price_micro_lamports: 50_000,
            bot_session_id: None,
            preset_id: None,
        };
        use solana_sdk::pubkey;
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let curve = crate::dex::pumpfun::BondingCurveAccount {
            virtual_token_reserves: crate::dex::pumpfun::INITIAL_VIRTUAL_TOKEN_RESERVES,
            virtual_sol_reserves: crate::dex::pumpfun::INITIAL_VIRTUAL_SOL_RESERVES,
            real_token_reserves: crate::dex::pumpfun::INITIAL_REAL_TOKEN_RESERVES,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: Pubkey::new_unique(),
        };
        let fee = FeeParams {
            cu_limit: cfg.pumpfun_cu_limit,
            cu_price_micro_lamports: cfg.pumpfun_cu_price_micro_lamports,
        };
        let params = cfg.pumpfun_sell_params(
            user,
            mint,
            1_000_000_000,
            &curve,
            crate::dex::ata::TOKEN_PROGRAM_ID,
            solana_sdk::hash::Hash::default(),
            fee,
        );
        let bytes = crate::dex::pumpfun::build_sell_tx(&params)
            .expect("build_sell_tx with session-derived params");
        let tx: solana_sdk::transaction::VersionedTransaction =
            bincode::deserialize(&bytes).unwrap();
        match &tx.message {
            solana_sdk::message::VersionedMessage::V0(msg) => {
                // Sell tx: cu-limit + cu-price + sell + Falcon tip = 4.
                // No ATA-create (seller owns the ATA); submit_mode=
                // "falcon" injects the tip (raised to minimum).
                assert_eq!(msg.instructions.len(), 4);
            }
            _ => panic!("expected v0"),
        }
    }

    #[test]
    fn pumpfun_sell_params_separate_cu_budget_from_buy() {
        // Sanity: both buy + sell helpers read the same CU fields off
        // BotConfig — change one, both reflect it. Guards against
        // accidental param drift if a future refactor splits them.
        let cfg = BotConfig {
            per_trade_lamports: 1,
            slippage_bps: 0,
            tip_lamports: 0,
            submit_mode: "x".into(),
            rpc_url: "http://localhost".into(),
            skip_simulate: true,
            skip_allowlist: true,
            input_mint: "So11111111111111111111111111111111111111112".into(),
            pumpfun_cu_limit: 222_222,
            pumpfun_cu_price_micro_lamports: 333_333,
            bot_session_id: None,
            preset_id: None,
        };
        use solana_sdk::pubkey;
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let curve = crate::dex::pumpfun::BondingCurveAccount {
            virtual_token_reserves: crate::dex::pumpfun::INITIAL_VIRTUAL_TOKEN_RESERVES,
            virtual_sol_reserves: crate::dex::pumpfun::INITIAL_VIRTUAL_SOL_RESERVES,
            real_token_reserves: 0,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: Pubkey::new_unique(),
        };
        let fee = FeeParams {
            cu_limit: cfg.pumpfun_cu_limit,
            cu_price_micro_lamports: cfg.pumpfun_cu_price_micro_lamports,
        };
        let buy = cfg.pumpfun_buy_params(
            user,
            mint,
            &curve,
            crate::dex::ata::TOKEN_PROGRAM_ID,
            solana_sdk::hash::Hash::default(),
            fee,
        );
        let sell = cfg.pumpfun_sell_params(
            user,
            mint,
            42,
            &curve,
            crate::dex::ata::TOKEN_PROGRAM_ID,
            solana_sdk::hash::Hash::default(),
            fee,
        );
        assert_eq!(buy.compute_unit_limit, sell.compute_unit_limit);
        assert_eq!(
            buy.compute_unit_price_micro_lamports,
            sell.compute_unit_price_micro_lamports
        );
        assert_eq!(buy.slippage_bps, sell.slippage_bps);
    }
}
