//! Direct on-chain reads against the yDelta program.
//!
//! Replaces the previous `indexer_client` dependency entirely. Every
//! piece of state handlers need — markets, loans, resting orders, risk
//! profiles, matched-loan queue nodes — is fetched straight from chain
//! via `getProgramAccounts` (with discriminator + memcmp filters) or
//! account-data deserialisation.
//!
//! Pattern follows references/eva01: filter `getProgramAccounts` by
//! discriminator bytes encoded in base58, then bytemuck-deserialize the
//! account's first N bytes into the fixed header. Dynamic / tree-backed
//! regions get walked with the `hypertree` iterators from the program
//! crate.
//!
//! View structs intentionally surface every field the indexer DTOs
//! used to expose, even when no current handler consumes them — keeps
//! this module a stable client-facing surface for future handlers (and
//! a 1:1 swap-in for any indexer-consuming code that gets ported).

#![allow(dead_code)]

use std::mem::size_of;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hypertree::{HyperTreeValueIteratorTrait, NIL};
use solana_client::{
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::pubkey::Pubkey;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;

use ydelta::state::loan::{LoanFixed, LoanState, LOAN_FIXED_DISCRIMINANT, LOAN_FIXED_SIZE};
use ydelta::state::market::{
    BooksideReadOnly, ClaimedSeatTreeReadOnly, MarketFixed, MatchedLoan, MatchedLoanTreeReadOnly,
};
use ydelta::state::resting_order::RestingOrder;
use ydelta::state::vault::{
    global_vault_pda, GlobalVaultFixed, RiskProfile, RiskProfileTreeReadOnly,
};
use ydelta::state::{
    ClaimedSeat, GLOBAL_VAULT_FIXED_DISCRIMINANT, MARKET_FIXED_DISCRIMINANT, MARKET_FIXED_SIZE,
    OWNER_KIND_RISK_PROFILE,
};

use crate::rpc::Rpc;

/// How long a cached market-list snapshot stays valid before the next
/// `list_markets()` re-fetches. yDelta markets are created rarely;
/// 30s strikes a balance between staleness and RPC traffic — every
/// handler tick reaches into this cache.
const MARKET_CACHE_TTL: Duration = Duration::from_secs(30);

/// Single high-level view of a `MarketFixed`. Carries the fields every
/// handler needs without exposing the raw account layout.
#[derive(Debug, Clone)]
pub struct MarketView {
    pub address: Pubkey,
    pub debt_mint: Pubkey,
    pub collateral_mint: Pubkey,
    pub debt_bank: Pubkey,
    pub collateral_bank: Pubkey,
    pub is_paused: bool,
    pub matched_loan_sequence: u64,
}

/// View of a single `LoanFixed` PDA.
#[derive(Debug, Clone)]
pub struct LoanView {
    pub address: Pubkey,
    pub market: Pubkey,
    pub state: u8,
    pub principal_debt_atoms: u64,
    pub outstanding_debt_atoms: u64,
    pub matures_at_unix: i64,
    pub matched_loan_sequence: u64,
    pub lender_kind: u8,
    pub lender_global_vault: Pubkey,
    pub lender_profile_id: u8,
    pub borrower_rate_bps: u16,
    pub lender_rate_bps: u16,
}

impl LoanView {
    pub fn is_active(&self) -> bool {
        self.state == LoanState::Active as u8
    }

    pub fn is_repaid(&self) -> bool {
        self.state == LoanState::Repaid as u8
    }
}

/// View of one `RestingOrder` joined with its `ClaimedSeat`.
#[derive(Debug, Clone)]
pub struct OrderView {
    pub sequence: u64,
    pub side: u8,
    pub rate_bps: u16,
    pub term_seconds: u32,
    pub principal_atoms: u64,
    /// From the seat: pubkey of the wallet/vault that owns this order.
    pub owner: Pubkey,
    pub owner_kind: u8,
    pub risk_profile_id: u8,
}

/// View of a `RiskProfile` lifted out of its parent `GlobalVaultFixed`.
#[derive(Debug, Clone)]
pub struct RiskProfileView {
    pub profile_id: u8,
    pub curator: Pubkey,
    pub max_ltv_bps: u16,
    pub max_term_seconds: u32,
    pub allowed_market_count: u8,
    pub allowed_market_max: u8,
    pub deployed_principal_atoms: u64,
    pub total_principal_atoms: u64,
    pub encumbered_in_orders_atoms: u64,
    /// `active_markets` is stored fixed-size on chain (`[Pubkey; 8]`);
    /// trailing default slots are stripped here so callers don't have
    /// to filter `Pubkey::default()` themselves.
    pub active_markets: Vec<Pubkey>,
}

/// Snapshot of one pending entry in a market's MatchedLoan queue.
#[derive(Debug, Clone)]
pub struct PendingMatchedLoan {
    pub market: Pubkey,
    pub sequence: u64,
    pub flags: u8,
    pub lender_seat_index: hypertree::DataIndex,
    pub borrower_seat_index: hypertree::DataIndex,
    pub principal_atoms: u64,
    pub origination_atoms: u64,
    pub referenced_loan_sequence: u64,
    pub new_lender_seat_index: hypertree::DataIndex,
    pub cash_paid_atoms: u64,
}

impl PendingMatchedLoan {
    pub fn is_secondary(&self) -> bool {
        use ydelta::state::market::MATCHED_LOAN_FLAG_SECONDARY;
        (self.flags & MATCHED_LOAN_FLAG_SECONDARY) != 0
    }

    pub fn is_split(&self) -> bool {
        use ydelta::state::market::MATCHED_LOAN_FLAG_SECONDARY_SPLIT;
        (self.flags & MATCHED_LOAN_FLAG_SECONDARY_SPLIT) != 0
    }

    pub fn has_vault_lender(&self) -> bool {
        use ydelta::state::market::MATCHED_LOAN_FLAG_VAULT_LENDER;
        (self.flags & MATCHED_LOAN_FLAG_VAULT_LENDER) != 0
    }
}

#[derive(Debug, Clone)]
pub struct SeatView {
    pub owner: Pubkey,
    pub owner_kind: u8,
    pub risk_profile_id: u8,
    /// Live `max_exposure_atoms` cap on this seat. For risk-profile
    /// seats only — user seats carry `0` here. Cranker reads this to
    /// detect drift vs. the computed target before issuing a
    /// `SetSeatMaxExposureForRiskProfile`.
    pub max_exposure_atoms: u64,
    /// Live `deployed_atoms` (Σ open loan principal). Floor for any
    /// cap shrink: the program rejects new caps below this value.
    pub deployed_atoms: u64,
}

/// On-chain reader. Holds the RPC handle and the ydelta program id, and
/// caches the markets list with a short TTL so per-tick callers don't
/// keep re-issuing `getProgramAccounts` for state that barely changes.
#[derive(Clone)]
pub struct ChainReader {
    rpc: Rpc,
    program_id: Pubkey,
    markets_cache: std::sync::Arc<Mutex<Option<MarketCache>>>,
}

struct MarketCache {
    fetched_at: Instant,
    markets: Vec<MarketView>,
}

impl ChainReader {
    pub fn new(rpc: Rpc, program_id: Pubkey) -> Self {
        Self {
            rpc,
            program_id,
            markets_cache: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    pub fn program_id(&self) -> Pubkey {
        self.program_id
    }

    pub fn rpc(&self) -> &Rpc {
        &self.rpc
    }

    /// Boot-time connectivity check — mirrors what `indexer.health()`
    /// did. Issues a cheap `getSlot` so the bot fails fast on a bad RPC
    /// URL instead of crashing inside the first handler tick.
    pub async fn health(&self) -> Result<()> {
        let _slot = self.rpc.client().get_slot().await.context("rpc.get_slot")?;
        Ok(())
    }

    // ──────────────────────────── Markets ────────────────────────────

    /// Every market the ydelta program owns. Cached for `MARKET_CACHE_TTL`.
    ///
    /// Defensive caching: an empty fetch result is treated as a likely
    /// transient RPC blip (Helius / Triton occasionally return zero
    /// `getProgramAccounts` rows under load) and does NOT replace the
    /// previous good snapshot. Without this, an empty mid-tick fetch
    /// would poison the cache for `MARKET_CACHE_TTL` and every
    /// downstream handler would log "market not found" until the cache
    /// expired again.
    pub async fn list_markets(&self) -> Result<Vec<MarketView>> {
        if let Some(cached) = self.markets_cache_get() {
            return Ok(cached);
        }
        let fresh = self.fetch_markets().await?;
        if fresh.is_empty() {
            // Don't cache empty. If we have a previous (now-expired)
            // snapshot, keep serving it rather than churning into a
            // bad state.
            if let Some(stale) = self.markets_cache_stale() {
                tracing::warn!(
                    "list_markets: fresh fetch returned 0 markets; serving stale snapshot of {}",
                    stale.len()
                );
                return Ok(stale);
            }
            tracing::warn!("list_markets: fresh fetch returned 0 markets and no prior cache");
            return Ok(fresh);
        }
        self.markets_cache_put(fresh.clone());
        Ok(fresh)
    }

    /// Bypass the TTL cache and refetch. Used at boot for bank
    /// discovery, where an empty result is a fatal config error
    /// (bot should idle with a warning, not silently retry forever).
    pub async fn refresh_markets(&self) -> Result<Vec<MarketView>> {
        let fresh = self.fetch_markets().await?;
        if !fresh.is_empty() {
            self.markets_cache_put(fresh.clone());
        }
        Ok(fresh)
    }

    fn markets_cache_get(&self) -> Option<Vec<MarketView>> {
        let guard = self.markets_cache.lock().ok()?;
        let cache = guard.as_ref()?;
        if cache.fetched_at.elapsed() < MARKET_CACHE_TTL {
            Some(cache.markets.clone())
        } else {
            None
        }
    }

    /// Cached markets ignoring TTL. Only used as a fallback when a
    /// fresh fetch returns an empty list — better to serve a
    /// minute-old snapshot than nothing at all while the RPC
    /// recovers.
    fn markets_cache_stale(&self) -> Option<Vec<MarketView>> {
        let guard = self.markets_cache.lock().ok()?;
        guard.as_ref().map(|c| c.markets.clone())
    }

    fn markets_cache_put(&self, markets: Vec<MarketView>) {
        if let Ok(mut guard) = self.markets_cache.lock() {
            *guard = Some(MarketCache {
                fetched_at: Instant::now(),
                markets,
            });
        }
    }

    async fn fetch_markets(&self) -> Result<Vec<MarketView>> {
        // Market accounts grow with their dynamic region (orderbook +
        // matched-loan + claimed-seat trees), so we can't pin
        // `DataSize` — only the leading 8-byte discriminator is fixed.
        // The decoder below validates the header length and rejects
        // anything too small to be a real `MarketFixed`.
        let filters = vec![RpcFilterType::Memcmp(Memcmp::new(
            0,
            MemcmpEncodedBytes::Base58(
                bs58::encode(MARKET_FIXED_DISCRIMINANT.to_le_bytes()).into_string(),
            ),
        ))];
        let accounts = self.get_program_accounts(filters).await?;
        let mut out = Vec::with_capacity(accounts.len());
        for (pk, acct) in accounts {
            // Markets carry a `MarketFixed` header followed by the dynamic
            // region. We only need the header; `MarketFixed` is a `Pod`
            // type with a fixed size, so the borrow on the first N bytes
            // is enough.
            let Ok(fixed) = decode_market_fixed(&acct.data) else {
                tracing::warn!(market = %pk, "skipping malformed market account");
                continue;
            };
            out.push(MarketView {
                address: pk,
                debt_mint: fixed.debt_mint,
                collateral_mint: fixed.collateral_mint,
                debt_bank: fixed.debt_lending_pool,
                collateral_bank: fixed.collateral_lending_pool,
                is_paused: fixed.is_paused != 0,
                matched_loan_sequence: fixed.matched_loan_sequence,
            });
        }
        // Stable ordering so retries / logs are deterministic.
        out.sort_by_key(|m| m.address.to_bytes());
        Ok(out)
    }

    /// Read a single `MarketFixed` directly (uncached). Returns the
    /// canonical pubkeys used to derive marginfi-bank wiring.
    pub async fn read_market(&self, market: &Pubkey) -> Result<MarketView> {
        let data = self
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} not found"))?;
        let fixed = decode_market_fixed(&data)?;
        Ok(MarketView {
            address: *market,
            debt_mint: fixed.debt_mint,
            collateral_mint: fixed.collateral_mint,
            debt_bank: fixed.debt_lending_pool,
            collateral_bank: fixed.collateral_lending_pool,
            is_paused: fixed.is_paused != 0,
            matched_loan_sequence: fixed.matched_loan_sequence,
        })
    }

    // ──────────────────────────── Loans ─────────────────────────────

    /// All loans on a market. Filter follows eva01's pattern: discriminator
    /// + market pubkey memcmp.
    pub async fn list_loans_for_market(&self, market: &Pubkey) -> Result<Vec<LoanView>> {
        let filters = vec![
            RpcFilterType::DataSize(LOAN_FIXED_SIZE as u64),
            RpcFilterType::Memcmp(Memcmp::new(
                0,
                MemcmpEncodedBytes::Base58(
                    bs58::encode(LOAN_FIXED_DISCRIMINANT.to_le_bytes()).into_string(),
                ),
            )),
            // `LoanFixed.market` lives at offset 8 (immediately after the
            // 8-byte discriminator).
            RpcFilterType::Memcmp(Memcmp::new(
                8,
                MemcmpEncodedBytes::Base58(market.to_string()),
            )),
        ];
        self.decode_loans(self.get_program_accounts(filters).await?)
    }

    /// All loans funded by a given `(global_vault, profile_id)`. Same
    /// discriminator filter as `list_loans_for_market`, but memcmp
    /// against `lender_global_vault` at offset 208 plus `lender_profile_id`
    /// at offset 202.
    pub async fn list_loans_for_profile(
        &self,
        vault: &Pubkey,
        profile_id: u8,
    ) -> Result<Vec<LoanView>> {
        let filters = vec![
            RpcFilterType::DataSize(LOAN_FIXED_SIZE as u64),
            RpcFilterType::Memcmp(Memcmp::new(
                0,
                MemcmpEncodedBytes::Base58(
                    bs58::encode(LOAN_FIXED_DISCRIMINANT.to_le_bytes()).into_string(),
                ),
            )),
            // `LoanFixed.lender_global_vault` offset is 208.
            RpcFilterType::Memcmp(Memcmp::new(
                208,
                MemcmpEncodedBytes::Base58(vault.to_string()),
            )),
            // `LoanFixed.lender_profile_id` is a single u8 at offset 202.
            RpcFilterType::Memcmp(Memcmp::new(
                202,
                MemcmpEncodedBytes::Bytes(vec![profile_id]),
            )),
        ];
        self.decode_loans(self.get_program_accounts(filters).await?)
    }

    fn decode_loans(&self, accts: Vec<(Pubkey, Account)>) -> Result<Vec<LoanView>> {
        let mut out = Vec::with_capacity(accts.len());
        for (pk, acct) in accts {
            let Ok(loan) = decode_loan_fixed(&acct.data) else {
                tracing::warn!(loan = %pk, "skipping malformed loan account");
                continue;
            };
            out.push(LoanView {
                address: pk,
                market: loan.market,
                state: loan.state,
                principal_debt_atoms: loan.principal_debt_atoms,
                outstanding_debt_atoms: loan.outstanding_debt_atoms,
                matures_at_unix: loan.matures_at_unix,
                matched_loan_sequence: loan.matched_loan_sequence,
                lender_kind: loan.lender_kind,
                lender_global_vault: loan.lender_global_vault,
                lender_profile_id: loan.lender_profile_id,
                borrower_rate_bps: loan.borrower_rate_bps,
                lender_rate_bps: loan.lender_rate_bps,
            });
        }
        Ok(out)
    }

    // ────────────────────── Matched-loan queue ──────────────────────

    /// All pending matched-loan queue nodes in a market. Walked
    /// in-place over the market's dynamic region.
    pub async fn read_pending_matched_loans(
        &self,
        market: &Pubkey,
    ) -> Result<Vec<PendingMatchedLoan>> {
        let data = self
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} not found"))?;
        let (fixed, dynamic) = split_market(&data)?;
        let root = fixed.matched_loans_root_index;
        if root == NIL {
            return Ok(vec![]);
        }
        let tree = MatchedLoanTreeReadOnly::new(dynamic, root, NIL);
        let mut out = Vec::new();
        for (_index, node) in tree.iter::<MatchedLoan>() {
            out.push(PendingMatchedLoan {
                market: *market,
                sequence: node.sequence,
                flags: node.flags,
                lender_seat_index: node.lender_seat_index,
                borrower_seat_index: node.borrower_seat_index,
                principal_atoms: node.principal_atoms,
                origination_atoms: node.origination_atoms,
                referenced_loan_sequence: node.referenced_loan_sequence,
                new_lender_seat_index: node.new_lender_seat_index,
                cash_paid_atoms: node.cash_paid_atoms,
            });
        }
        Ok(out)
    }

    /// Look up a single seat in a market's dynamic region by data index.
    /// Used by the promoter when it needs to peek at a lender / new-lender
    /// seat referenced from a `MatchedLoan` queue node.
    pub fn read_seat_at(&self, market_data: &[u8], index: hypertree::DataIndex) -> Result<SeatView> {
        if index == NIL {
            return Err(anyhow!("seat index is NIL"));
        }
        let dynamic = &market_data[MARKET_FIXED_SIZE..];
        let seat = ydelta::state::market::get_helper_seat(dynamic, index).get_value();
        // Vault-cap accessors are only meaningful on risk-profile seats;
        // for user seats they alias unrelated u128 slots, so report 0.
        let (max_exposure_atoms, deployed_atoms) = if seat.owner_kind == OWNER_KIND_RISK_PROFILE {
            (seat.max_exposure_atoms(), seat.deployed_atoms())
        } else {
            (0, 0)
        };
        Ok(SeatView {
            owner: seat.owner,
            owner_kind: seat.owner_kind,
            risk_profile_id: seat.risk_profile_id,
            max_exposure_atoms,
            deployed_atoms,
        })
    }

    /// Look up a vault-owned seat by `(market, vault, profile_id)`.
    /// Returns `None` if the seat doesn't exist yet (e.g. the vault has
    /// never placed an order in this market).
    pub async fn read_vault_seat(
        &self,
        market: &Pubkey,
        vault: &Pubkey,
        profile_id: u8,
    ) -> Result<Option<SeatView>> {
        let data = self
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} not found"))?;
        let (fixed, dynamic) = split_market(&data)?;
        let probe = ClaimedSeat::new_empty(*vault, OWNER_KIND_RISK_PROFILE, profile_id);
        let tree = ClaimedSeatTreeReadOnly::new(dynamic, fixed.claimed_seats_root_index, NIL);
        use hypertree::HyperTreeReadOperations;
        let idx = tree.lookup_index(&probe);
        if idx == NIL {
            return Ok(None);
        }
        Ok(Some(self.read_seat_at(&data, idx)?))
    }

    // ─────────────────────────── Orders ─────────────────────────────

    /// Every resting order on a market, with its `ClaimedSeat` owner
    /// joined in. Walks both the bids and asks trees in the market's
    /// dynamic region.
    pub async fn list_market_orders(&self, market: &Pubkey) -> Result<Vec<OrderView>> {
        let data = self
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} not found"))?;
        let (fixed, dynamic) = split_market(&data)?;
        let mut out = Vec::new();
        // Bids tree.
        if fixed.bids_root_index != NIL {
            let tree =
                BooksideReadOnly::new(dynamic, fixed.bids_root_index, fixed.bids_best_index);
            for (_idx, order) in tree.iter::<RestingOrder>() {
                if let Some(view) = self.join_order_with_seat(dynamic, order) {
                    out.push(view);
                }
            }
        }
        // Asks tree.
        if fixed.asks_root_index != NIL {
            let tree =
                BooksideReadOnly::new(dynamic, fixed.asks_root_index, fixed.asks_best_index);
            for (_idx, order) in tree.iter::<RestingOrder>() {
                if let Some(view) = self.join_order_with_seat(dynamic, order) {
                    out.push(view);
                }
            }
        }
        Ok(out)
    }

    fn join_order_with_seat(&self, dynamic: &[u8], order: &RestingOrder) -> Option<OrderView> {
        if order.trader_seat_index == NIL {
            return None;
        }
        let seat = ydelta::state::market::get_helper_seat(dynamic, order.trader_seat_index)
            .get_value();
        Some(OrderView {
            sequence: order.sequence_number,
            side: order.side as u8,
            rate_bps: order.rate_bps,
            term_seconds: order.term_seconds,
            principal_atoms: order.principal_atoms,
            owner: seat.owner,
            owner_kind: seat.owner_kind,
            risk_profile_id: seat.risk_profile_id,
        })
    }

    // ─────────────────────── Vault / RiskProfile ─────────────────────

    /// Read a `RiskProfile` from a vault by walking its `risk_profiles`
    /// tree. Returns `None` if no such profile exists. The vault PDA
    /// itself doesn't have to be derived — caller passes its pubkey.
    pub async fn read_risk_profile(
        &self,
        vault: &Pubkey,
        profile_id: u8,
    ) -> Result<Option<RiskProfileView>> {
        let data = self
            .rpc
            .get_account_data(vault)
            .await?
            .ok_or_else(|| anyhow!("vault {vault} not found"))?;
        let (fixed, dynamic) = split_vault(&data)?;
        if fixed.risk_profiles_root_index == NIL {
            return Ok(None);
        }
        let tree =
            RiskProfileTreeReadOnly::new(dynamic, fixed.risk_profiles_root_index, NIL);
        for (_idx, profile) in tree.iter::<RiskProfile>() {
            if profile.profile_id == profile_id {
                let count = profile.allowed_market_count as usize;
                let active_markets = profile.active_markets[..count]
                    .iter()
                    .copied()
                    .filter(|m| *m != Pubkey::default())
                    .collect();
                return Ok(Some(RiskProfileView {
                    profile_id: profile.profile_id,
                    curator: profile.curator,
                    max_ltv_bps: profile.max_ltv_bps,
                    max_term_seconds: profile.max_term_seconds,
                    allowed_market_count: profile.allowed_market_count,
                    allowed_market_max: profile.allowed_market_max,
                    deployed_principal_atoms: profile.deployed_principal_atoms,
                    total_principal_atoms: profile.total_principal_atoms,
                    encumbered_in_orders_atoms: profile.encumbered_in_orders_atoms,
                    active_markets,
                }));
            }
        }
        Ok(None)
    }

    /// Convenience: derive the per-mint global-vault PDA. Hands off to
    /// `ydelta::state::vault::global_vault_pda` so the seed convention
    /// can't drift from the program crate.
    pub fn global_vault_pda(&self, mint: &Pubkey) -> Pubkey {
        global_vault_pda(mint).0
    }

    // ─────────────────────── getProgramAccounts ──────────────────────

    async fn get_program_accounts(
        &self,
        filters: Vec<RpcFilterType>,
    ) -> Result<Vec<(Pubkey, Account)>> {
        let cfg = RpcProgramAccountsConfig {
            account_config: RpcAccountInfoConfig {
                encoding: Some(solana_account_decoder_client_types::UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
            filters: Some(filters),
            with_context: Some(false),
            sort_results: None,
        };
        self.rpc
            .client()
            .get_program_accounts_with_config(&self.program_id, cfg)
            .await
            .with_context(|| format!("getProgramAccounts({})", self.program_id))
    }
}

// ──────────────────────────── decoders ────────────────────────────

fn decode_market_fixed(data: &[u8]) -> Result<&MarketFixed> {
    if data.len() < MARKET_FIXED_SIZE {
        return Err(anyhow!(
            "market account too small: {} < {}",
            data.len(),
            MARKET_FIXED_SIZE
        ));
    }
    let fixed: &MarketFixed = bytemuck::from_bytes(&data[..MARKET_FIXED_SIZE]);
    if fixed.discriminator != MARKET_FIXED_DISCRIMINANT {
        return Err(anyhow!(
            "market discriminator mismatch: got {:#x}",
            fixed.discriminator
        ));
    }
    Ok(fixed)
}

fn decode_loan_fixed(data: &[u8]) -> Result<&LoanFixed> {
    if data.len() < LOAN_FIXED_SIZE {
        return Err(anyhow!(
            "loan account too small: {} < {}",
            data.len(),
            LOAN_FIXED_SIZE
        ));
    }
    let loan: &LoanFixed = bytemuck::from_bytes(&data[..LOAN_FIXED_SIZE]);
    if loan.discriminator != LOAN_FIXED_DISCRIMINANT {
        return Err(anyhow!(
            "loan discriminator mismatch: got {:#x}",
            loan.discriminator
        ));
    }
    Ok(loan)
}

fn split_market(data: &[u8]) -> Result<(&MarketFixed, &[u8])> {
    let fixed = decode_market_fixed(data)?;
    Ok((fixed, &data[MARKET_FIXED_SIZE..]))
}

fn split_vault(data: &[u8]) -> Result<(&GlobalVaultFixed, &[u8])> {
    let fixed_size = size_of::<GlobalVaultFixed>();
    if data.len() < fixed_size {
        return Err(anyhow!(
            "vault account too small: {} < {}",
            data.len(),
            fixed_size
        ));
    }
    let fixed: &GlobalVaultFixed = bytemuck::from_bytes(&data[..fixed_size]);
    if fixed.discriminator != GLOBAL_VAULT_FIXED_DISCRIMINANT {
        return Err(anyhow!(
            "vault discriminator mismatch: got {:#x}",
            fixed.discriminator
        ));
    }
    Ok((fixed, &data[fixed_size..]))
}
