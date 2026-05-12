//! Direct on-chain reads against `MarketFixed` accounts.
//!
//! The indexer doesn't expose the `MatchedLoan` queue, so the promoter
//! handler reads market accounts itself and walks the hypertree. We
//! also expose seat lookups for handlers that need them.
//!
//! Account layout (from `programs/ydelta/src/state/market.rs`):
//!   [MarketFixed | dynamic region (hypertree-backed)]
//!
//! Tree roots in `MarketFixed`:
//!   - `claimed_seats_root_index`
//!   - `matched_loans_root_index`
//!   - (plus orderbook roots, not used here)

use std::mem::size_of;

use anyhow::{anyhow, Result};
use hypertree::{HyperTreeValueIteratorTrait, NIL};
use solana_program::pubkey::Pubkey;
use ydelta::state::market::{
    ClaimedSeatTreeReadOnly, MarketFixed, MatchedLoan, MatchedLoanTreeReadOnly,
};
use ydelta::state::ClaimedSeat;

use crate::rpc::Rpc;

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
    /// Populated only on SECONDARY-flagged nodes; zero for primary.
    pub referenced_loan_sequence: u64,
    /// Buyer's (taker) seat on a secondary cross. NIL for primary.
    pub new_lender_seat_index: hypertree::DataIndex,
    /// Cash the buyer paid for the secondary loan. Zero for primary.
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
}

/// The four marginfi-touching pubkeys we need to discover per market.
/// Both `*_lending_pool` fields are the marginfi `Bank` accounts on the
/// debt and collateral side respectively.
#[derive(Debug, Clone)]
pub struct MarketBankBindings {
    pub debt_mint: Pubkey,
    pub debt_bank: Pubkey,
    pub collateral_mint: Pubkey,
    pub collateral_bank: Pubkey,
}

/// Read `MarketFixed` for `market` and pull out the four pubkeys that
/// pin its marginfi bank wiring. Used at boot to discover every bank
/// the cranker will ever touch without asking the operator to enumerate
/// them by hand.
pub async fn read_market_bank_bindings(rpc: &Rpc, market: &Pubkey) -> Result<MarketBankBindings> {
    let data = rpc
        .get_account_data(market)
        .await?
        .ok_or_else(|| anyhow!("market {} not found / empty", market))?;
    let (fixed, _dynamic) = split_market(&data)?;
    Ok(MarketBankBindings {
        debt_mint: fixed.debt_mint,
        debt_bank: fixed.debt_lending_pool,
        collateral_mint: fixed.collateral_mint,
        collateral_bank: fixed.collateral_lending_pool,
    })
}

/// All pending matched-loan queue nodes in a given market.
pub async fn read_pending_matched_loans(
    rpc: &Rpc,
    market: &Pubkey,
) -> Result<Vec<PendingMatchedLoan>> {
    let data = rpc
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

/// Look up a single seat by its index in the market's dynamic region.
pub fn read_seat_at(data: &[u8], index: hypertree::DataIndex) -> Result<SeatView> {
    if index == NIL {
        return Err(anyhow!("seat index is NIL"));
    }
    let dynamic = &data[size_of::<MarketFixed>()..];
    let seat = ydelta::state::market::get_helper_seat(dynamic, index).get_value();
    Ok(SeatView {
        owner: seat.owner,
        owner_kind: seat.owner_kind,
        risk_profile_id: seat.risk_profile_id,
    })
}

/// Look up a vault-owned seat by (owner=vault, profile_id).
pub async fn read_vault_seat(
    rpc: &Rpc,
    market: &Pubkey,
    vault: &Pubkey,
    profile_id: u8,
) -> Result<Option<SeatView>> {
    let data = rpc
        .get_account_data(market)
        .await?
        .ok_or_else(|| anyhow!("market {market} not found"))?;
    let (fixed, dynamic) = split_market(&data)?;
    let probe = ClaimedSeat::new_empty(*vault, ydelta::state::OWNER_KIND_RISK_PROFILE, profile_id);
    let tree = ClaimedSeatTreeReadOnly::new(dynamic, fixed.claimed_seats_root_index, NIL);
    use hypertree::HyperTreeReadOperations;
    let idx = tree.lookup_index(&probe);
    if idx == NIL {
        return Ok(None);
    }
    Ok(Some(read_seat_at(&data, idx)?))
}

fn split_market(data: &[u8]) -> Result<(&MarketFixed, &[u8])> {
    let fixed_size = size_of::<MarketFixed>();
    if data.len() < fixed_size {
        return Err(anyhow!(
            "market account too small: {} < {}",
            data.len(),
            fixed_size
        ));
    }
    let fixed: &MarketFixed = bytemuck::from_bytes(&data[..fixed_size]);
    let dynamic = &data[fixed_size..];
    Ok((fixed, dynamic))
}
