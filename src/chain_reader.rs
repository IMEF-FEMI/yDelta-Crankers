//! `getProgramAccounts` + in-place hypertree walks against the yDelta
//! program. No indexer.

#![allow(dead_code)]

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
    BooksideReadOnly, MarketFixed, MatchedLoan, MatchedLoanTreeReadOnly,
    MATCHED_LOAN_FLAG_VAULT_LENDER, MATCHED_LOAN_FLAG_VAULT_PRESETTLED,
};
use ydelta::state::resting_order::RestingOrder;
use ydelta::state::vault::{GlobalVaultFixed, RiskProfile, RiskProfileTreeReadOnly};
use ydelta::state::{
    GLOBAL_VAULT_FIXED_DISCRIMINANT, GLOBAL_VAULT_FIXED_SIZE, MARKET_FIXED_DISCRIMINANT,
    MARKET_FIXED_SIZE, OWNER_KIND_RISK_PROFILE,
};

use crate::rpc::Rpc;

const MARKET_CACHE_TTL: Duration = Duration::from_secs(30);

/// `LoanFixed` memcmp offsets. Centralised so a layout change is one diff.
mod loan_offsets {
    pub const MARKET: usize = 8;
    pub const STATE: usize = 196;
    pub const LOAN_TYPE: usize = 197;
    pub const LENDER_KIND: usize = 201;
    pub const LENDER_PROFILE_ID: usize = 202;
    pub const LENDER_GLOBAL_VAULT: usize = 208;
}

#[derive(Debug, Clone)]
pub struct MarketView {
    pub address: Pubkey,
    pub debt_mint: Pubkey,
    pub collateral_mint: Pubkey,
    pub debt_bank: Pubkey,
    pub collateral_bank: Pubkey,
    pub is_paused: bool,
    pub admin: Pubkey,
    pub grace_period_seconds: u32,
    pub liquidation_keeper_bps: u16,
    pub liquidation_protocol_bps: u16,
    pub curator_fee_bps: u16,
}

#[derive(Debug, Clone)]
pub struct LoanView {
    pub address: Pubkey,
    pub market: Pubkey,
    pub state: u8,
    pub loan_type: u8,
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

    pub fn is_p2pool(&self) -> bool {
        self.loan_type == ydelta::state::loan::LoanType::P2Pool as u8
    }
}

#[derive(Debug, Clone)]
pub struct OrderView {
    pub sequence: u64,
    pub side: u8,
    pub rate_bps: u16,
    pub term_seconds: u32,
    pub principal_atoms: u64,
    pub owner: Pubkey,
    pub owner_kind: u8,
    pub risk_profile_id: u8,
}

#[derive(Debug, Clone)]
pub struct PendingMatchedLoan {
    pub market: Pubkey,
    pub sequence: u64,
    pub flags: u8,
    pub loan_type: u8,
    pub lender_seat_index: hypertree::DataIndex,
    pub borrower_seat_index: hypertree::DataIndex,
    pub principal_atoms: u64,
    pub origination_atoms: u64,
}

impl PendingMatchedLoan {
    pub fn has_vault_lender(&self) -> bool {
        (self.flags & MATCHED_LOAN_FLAG_VAULT_LENDER) != 0
    }

    /// Set by `convert_p2pool_to_fixed`. Vault principal already moved;
    /// promoter skips the vault-settle bundle when set.
    pub fn is_vault_presettled(&self) -> bool {
        (self.flags & MATCHED_LOAN_FLAG_VAULT_PRESETTLED) != 0
    }

    pub fn is_p2pool(&self) -> bool {
        self.loan_type == ydelta::state::loan::LoanType::P2Pool as u8
    }
}

#[derive(Debug, Clone)]
pub struct SeatView {
    pub owner: Pubkey,
    pub owner_kind: u8,
    pub risk_profile_id: u8,
}

#[derive(Debug, Clone)]
pub struct RiskProfileView {
    pub vault: Pubkey,
    pub vault_mint: Pubkey,
    pub vault_bank: Pubkey,
    pub profile_id: u8,
    pub curator: Pubkey,
    pub accumulated_curator_fee_atoms: u64,
    pub vault_is_paused: bool,
}

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

    pub async fn health(&self) -> Result<()> {
        let _slot = self.rpc.client().get_slot().await.context("rpc.get_slot")?;
        Ok(())
    }

    /// Cached for `MARKET_CACHE_TTL`. A transient empty fetch never
    /// wipes the prior good snapshot — Helius/Triton occasionally
    /// return zero `getProgramAccounts` rows under load.
    pub async fn list_markets(&self) -> Result<Vec<MarketView>> {
        if let Some(cached) = self.markets_cache_get() {
            return Ok(cached);
        }
        let fresh = self.fetch_markets().await?;
        if fresh.is_empty() {
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

    /// Bypass the TTL cache. Used at boot where an empty result is
    /// fatal config error.
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
        // Market accounts grow with their dynamic region; only the
        // discriminator is at a fixed offset, so no DataSize filter.
        let filters = vec![RpcFilterType::Memcmp(Memcmp::new(
            0,
            MemcmpEncodedBytes::Base58(
                bs58::encode(MARKET_FIXED_DISCRIMINANT.to_le_bytes()).into_string(),
            ),
        ))];
        let accounts = self.get_program_accounts(filters).await?;
        let mut out = Vec::with_capacity(accounts.len());
        for (pk, acct) in accounts {
            let Ok(fixed) = decode_market_fixed(&acct.data) else {
                tracing::warn!(market = %pk, "skipping malformed market account");
                continue;
            };
            out.push(market_view_from_fixed(pk, fixed));
        }
        out.sort_by_key(|m| m.address.to_bytes());
        Ok(out)
    }

    pub async fn read_market(&self, market: &Pubkey) -> Result<MarketView> {
        let data = self
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} not found"))?;
        let fixed = decode_market_fixed(&data)?;
        Ok(market_view_from_fixed(*market, fixed))
    }

    pub async fn list_loans_for_market(&self, market: &Pubkey) -> Result<Vec<LoanView>> {
        let filters = vec![
            RpcFilterType::DataSize(LOAN_FIXED_SIZE as u64),
            self.loan_discriminator_filter(),
            RpcFilterType::Memcmp(Memcmp::new(
                loan_offsets::MARKET,
                MemcmpEncodedBytes::Base58(market.to_string()),
            )),
        ];
        self.decode_loans(self.get_program_accounts(filters).await?)
    }

    pub async fn list_repaid_vault_loans(&self) -> Result<Vec<LoanView>> {
        let filters = vec![
            RpcFilterType::DataSize(LOAN_FIXED_SIZE as u64),
            self.loan_discriminator_filter(),
            RpcFilterType::Memcmp(Memcmp::new(
                loan_offsets::STATE,
                MemcmpEncodedBytes::Bytes(vec![LoanState::Repaid as u8]),
            )),
            RpcFilterType::Memcmp(Memcmp::new(
                loan_offsets::LENDER_KIND,
                MemcmpEncodedBytes::Bytes(vec![OWNER_KIND_RISK_PROFILE]),
            )),
        ];
        self.decode_loans(self.get_program_accounts(filters).await?)
    }

    fn loan_discriminator_filter(&self) -> RpcFilterType {
        RpcFilterType::Memcmp(Memcmp::new(
            0,
            MemcmpEncodedBytes::Base58(
                bs58::encode(LOAN_FIXED_DISCRIMINANT.to_le_bytes()).into_string(),
            ),
        ))
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
                loan_type: loan.loan_type,
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
                loan_type: node.loan_type,
                lender_seat_index: node.lender_seat_index,
                borrower_seat_index: node.borrower_seat_index,
                principal_atoms: node.principal_atoms,
                origination_atoms: node.origination_atoms,
            });
        }
        Ok(out)
    }

    pub fn read_seat_at(
        &self,
        market_data: &[u8],
        index: hypertree::DataIndex,
    ) -> Result<SeatView> {
        if index == NIL {
            return Err(anyhow!("seat index is NIL"));
        }
        let dynamic = &market_data[MARKET_FIXED_SIZE..];
        let seat = ydelta::state::market::get_helper_seat(dynamic, index).get_value();
        Ok(SeatView {
            owner: seat.owner,
            owner_kind: seat.owner_kind,
            risk_profile_id: seat.risk_profile_id,
        })
    }

    pub async fn list_risk_profiles(&self) -> Result<Vec<RiskProfileView>> {
        let filters = vec![RpcFilterType::Memcmp(Memcmp::new(
            0,
            MemcmpEncodedBytes::Base58(
                bs58::encode(GLOBAL_VAULT_FIXED_DISCRIMINANT.to_le_bytes()).into_string(),
            ),
        ))];
        let accts = self.get_program_accounts(filters).await?;
        let mut out = Vec::new();
        for (pk, acct) in accts {
            if acct.data.len() < GLOBAL_VAULT_FIXED_SIZE {
                continue;
            }
            let (fixed_bytes, dynamic) = acct.data.split_at(GLOBAL_VAULT_FIXED_SIZE);
            let header: &GlobalVaultFixed = match bytemuck::try_from_bytes(fixed_bytes) {
                Ok(h) => h,
                Err(_) => continue,
            };
            if header.discriminator != GLOBAL_VAULT_FIXED_DISCRIMINANT {
                continue;
            }
            if header.risk_profiles_root_index == NIL {
                continue;
            }
            let tree =
                RiskProfileTreeReadOnly::new(dynamic, header.risk_profiles_root_index, NIL);
            for (_idx, profile) in tree.iter::<RiskProfile>() {
                out.push(RiskProfileView {
                    vault: pk,
                    vault_mint: header.mint,
                    vault_bank: header.lending_pool,
                    profile_id: profile.profile_id,
                    curator: profile.curator,
                    accumulated_curator_fee_atoms: profile.accumulated_curator_fee_atoms,
                    vault_is_paused: header.is_paused != 0,
                });
            }
        }
        Ok(out)
    }

    pub async fn list_market_orders(&self, market: &Pubkey) -> Result<Vec<OrderView>> {
        let data = self
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} not found"))?;
        let (fixed, dynamic) = split_market(&data)?;
        let mut out = Vec::new();
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
            side: order.side,
            rate_bps: order.rate_bps,
            term_seconds: order.term_seconds,
            principal_atoms: order.principal_atoms,
            owner: seat.owner,
            owner_kind: seat.owner_kind,
            risk_profile_id: seat.risk_profile_id,
        })
    }

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

fn market_view_from_fixed(address: Pubkey, fixed: &MarketFixed) -> MarketView {
    MarketView {
        address,
        debt_mint: fixed.debt_mint,
        collateral_mint: fixed.collateral_mint,
        debt_bank: fixed.debt_lending_pool,
        collateral_bank: fixed.collateral_lending_pool,
        is_paused: fixed.is_paused != 0,
        admin: fixed.admin,
        grace_period_seconds: fixed.fee_config.grace_period_seconds,
        liquidation_keeper_bps: fixed.fee_config.liquidation_keeper_bps,
        liquidation_protocol_bps: fixed.fee_config.liquidation_protocol_bps,
        curator_fee_bps: fixed.fee_config.curator_fee_bps,
    }
}

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
