//! Per-mint marginfi-bank metadata.
//!
//! No env input. The cranker discovers every bank it cares about by:
//!   1. Listing markets on chain via `ChainReader::list_markets`
//!      (a single `getProgramAccounts` against the ydelta program id
//!      filtered by the `MarketFixed` discriminator).
//!   2. Reading each `MarketFixed` to pull the four pubkeys it stores
//!      natively: `debt_mint`, `debt_lending_pool`, `collateral_mint`,
//!      `collateral_lending_pool` (the two `*_lending_pool` fields are
//!      the marginfi `Bank` pubkeys).
//!   3. De-duplicating across markets and reading each unique `Bank`
//!      account to extract `liquidity_vault`, the LVA bump, and the
//!      oracle list. The LVA pubkey is derived from the bump via the
//!      canonical PDA seed (`"liquidity_vault_auth"`).
//!   4. Fetching each mint to determine its SPL token program
//!      (legacy vs token-2022).
//!
//! ATAs (liquidator's debt + collateral token accounts) are deterministic
//! PDAs of `(owner, token_program, mint)` — derived on demand via
//! `BankInfo::ata_for`. No env config for those either.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    signature::{Keypair, Signer as _},
    system_program,
};

use crate::chain_reader::ChainReader;
use crate::marginfi_bank::BankView;
use crate::rpc::Rpc;

/// SPL Associated Token Account program id. Constant across mainnet +
/// localnet, no env override.
fn ata_program_id() -> Pubkey {
    Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()
}

/// Legacy spl-token program id.
pub fn spl_token_legacy_id() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}

/// Token-2022 program id.
pub fn spl_token_2022_id() -> Pubkey {
    Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap()
}

#[derive(Debug, Clone)]
pub struct BankInfo {
    pub mint: Pubkey,
    pub bank: Pubkey,
    pub liquidity_vault: Pubkey,
    pub liquidity_vault_authority: Pubkey,
    /// 1-5 oracle pubkeys. The vault-settle CPI path uses oracles[0] only
    /// (per the on-chain loader); the settle/liquidate paths pass the
    /// full slice through as the bank's active-balance health-check tail.
    pub oracles: Vec<Pubkey>,
    /// `spl_token_legacy_id()` or `spl_token_2022_id()`. Defaults to
    /// legacy when not specified in env.
    pub token_program: Pubkey,
}

impl BankInfo {
    /// Convenience for builders that take a single primary oracle.
    pub fn primary_oracle(&self) -> Pubkey {
        self.oracles
            .first()
            .copied()
            .unwrap_or_else(Pubkey::default)
    }

    /// SPL Associated Token Account for `owner` holding `self.mint`
    /// under `self.token_program`. The ATA program ID is constant; the
    /// derivation is deterministic, so the operator doesn't need to
    /// configure these — they only need to *fund* the debt-side ones
    /// for the liquidator before the bot runs.
    pub fn ata_for(&self, owner: &Pubkey) -> Pubkey {
        let (ata, _bump) = Pubkey::find_program_address(
            &[
                owner.as_ref(),
                self.token_program.as_ref(),
                self.mint.as_ref(),
            ],
            &ata_program_id(),
        );
        ata
    }
}

#[derive(Default, Clone, Debug)]
pub struct BankRegistry {
    by_mint: HashMap<Pubkey, BankInfo>,
}

/// PDA seed for the marginfi liquidity-vault authority. Mirrors the
/// `LIQUIDITY_VAULT_AUTHORITY_SEED` constant in marginfi-v2's type
/// crate. Don't change this — must match the on-chain program byte-for-byte.
const LIQUIDITY_VAULT_AUTHORITY_SEED: &[u8] = b"liquidity_vault_auth";

impl BankRegistry {
    /// Discover every bank touched by any market the indexer surfaces,
    /// then resolve each one's full metadata from chain. Order:
    ///
    ///   1. Indexer → list of markets.
    ///   2. Each market → `MarketFixed.{debt,collateral}_lending_pool`
    ///      (the marginfi `Bank` pubkeys, stored natively on the market
    ///      account). Read via `market_reader::read_market_bank_bindings`.
    ///   3. Dedupe (mint, bank) pairs across markets.
    ///   4. Resolve each unique bank's metadata via the existing
    ///      chain-reading path.
    pub async fn discover_from_markets(
        rpc: &Rpc,
        chain: &ChainReader,
        marginfi_program: &Pubkey,
    ) -> Result<Self> {
        // One `getProgramAccounts(MarketFixed)` returns every market on
        // chain, fully decoded, in a single RPC round-trip — no per-
        // market follow-up read needed.
        let markets = chain
            .refresh_markets()
            .await
            .context("BANKS discover: ChainReader::refresh_markets failed")?;
        if markets.is_empty() {
            tracing::warn!("BANKS discover: chain returned 0 markets; bot will idle");
            return Ok(Self::default());
        }

        let mut mint_to_bank: HashMap<Pubkey, Pubkey> = HashMap::new();
        for m in &markets {
            // If two markets bind the same mint to different banks, we
            // refuse to start — that's a misconfiguration we can't
            // silently paper over (different banks = different oracles
            // and rates).
            if let Some(prev) = mint_to_bank.insert(m.debt_mint, m.debt_bank) {
                if prev != m.debt_bank {
                    bail!(
                        "market {} debt_bank {} disagrees with prior bank {} for mint {}",
                        m.address,
                        m.debt_bank,
                        prev,
                        m.debt_mint,
                    );
                }
            }
            if let Some(prev) = mint_to_bank.insert(m.collateral_mint, m.collateral_bank) {
                if prev != m.collateral_bank {
                    bail!(
                        "market {} collateral_bank {} disagrees with prior bank {} for mint {}",
                        m.address,
                        m.collateral_bank,
                        prev,
                        m.collateral_mint,
                    );
                }
            }
        }
        tracing::info!(
            markets = markets.len(),
            unique_banks = mint_to_bank.len(),
            "BANKS discover: extracted bank pubkeys from market accounts",
        );

        Self::resolve_from_chain(rpc.client().as_ref(), marginfi_program, &mint_to_bank).await
    }

    /// Chain-read pass: given a `{mint → bank}` map, fetch each bank
    /// account, validate ownership + mint, derive the LVA pubkey from
    /// the on-chain bump, resolve each mint's SPL token program.
    /// Reused internally by `discover_from_markets`.
    async fn resolve_from_chain(
        rpc: &RpcClient,
        marginfi_program: &Pubkey,
        mint_to_bank: &HashMap<Pubkey, Pubkey>,
    ) -> Result<Self> {
        if mint_to_bank.is_empty() {
            return Ok(Self::default());
        }

        // Stable ordering so error messages reproduce consistently.
        let mut entries: Vec<(Pubkey, Pubkey)> =
            mint_to_bank.iter().map(|(m, b)| (*m, *b)).collect();
        entries.sort_by_key(|(m, _)| m.to_bytes());

        let bank_pubkeys: Vec<Pubkey> = entries.iter().map(|(_, b)| *b).collect();
        let bank_accounts = rpc
            .get_multiple_accounts(&bank_pubkeys)
            .await
            .context("BANKS resolve: get_multiple_accounts(banks) failed")?;

        // First pass: decode each bank, validate ownership + mint match,
        // collect mints for the second pass (token-program resolution).
        let mut decoded: Vec<(Pubkey, Pubkey, BankView, Pubkey)> =
            Vec::with_capacity(entries.len());
        let mut mint_pubkeys: Vec<Pubkey> = Vec::with_capacity(entries.len());
        for (i, (expected_mint, bank_pk)) in entries.iter().enumerate() {
            let acct = bank_accounts[i]
                .as_ref()
                .ok_or_else(|| anyhow!("BANKS: bank {bank_pk} not found on chain"))?;
            if acct.owner != *marginfi_program {
                bail!(
                    "BANKS[{expected_mint}]: bank {bank_pk} is owned by {}, not the configured \
                     MARGINFI_PROGRAM_ID {marginfi_program}",
                    acct.owner
                );
            }
            let view = BankView::try_from_account_data(&acct.data).with_context(|| {
                format!("BANKS[{expected_mint}]: decoding bank {bank_pk} failed")
            })?;
            if view.mint != *expected_mint {
                bail!(
                    "BANKS[{expected_mint}]: bank {bank_pk} has on-chain mint {} — pair the \
                     env key with the correct bank or fix the mint",
                    view.mint
                );
            }
            // Derive the LVA pubkey from the bump stored on the bank.
            let lva = Pubkey::create_program_address(
                &[
                    LIQUIDITY_VAULT_AUTHORITY_SEED,
                    bank_pk.as_ref(),
                    &[view.lva_bump],
                ],
                marginfi_program,
            )
            .with_context(|| {
                format!(
                    "BANKS[{expected_mint}]: deriving LVA failed (bump={})",
                    view.lva_bump
                )
            })?;
            mint_pubkeys.push(view.mint);
            decoded.push((*expected_mint, *bank_pk, view, lva));
        }

        // Second pass: fetch each mint to figure out its SPL token
        // program. Legacy spl-token and token-2022 are owner-discriminated.
        let mint_accounts = rpc
            .get_multiple_accounts(&mint_pubkeys)
            .await
            .context("BANKS resolve: get_multiple_accounts(mints) failed")?;

        let mut by_mint = HashMap::new();
        for (i, (mint, bank, view, lva)) in decoded.iter().enumerate() {
            let m = mint_accounts[i]
                .as_ref()
                .ok_or_else(|| anyhow!("BANKS: mint {mint} not found on chain"))?;
            let token_program = if m.owner == spl_token_legacy_id() {
                spl_token_legacy_id()
            } else if m.owner == spl_token_2022_id() {
                spl_token_2022_id()
            } else {
                bail!(
                    "BANKS[{mint}]: mint owner {} is neither spl-token nor token-2022",
                    m.owner
                );
            };
            if view.oracles.is_empty() || view.oracles.len() > 5 {
                bail!(
                    "BANKS[{mint}]: bank {bank} has {} oracle keys (must be 1-5)",
                    view.oracles.len()
                );
            }
            by_mint.insert(
                *mint,
                BankInfo {
                    mint: *mint,
                    bank: *bank,
                    liquidity_vault: view.liquidity_vault,
                    liquidity_vault_authority: *lva,
                    oracles: view.oracles.clone(),
                    token_program,
                },
            );
        }
        Ok(Self { by_mint })
    }

    pub fn get(&self, mint: &Pubkey) -> Option<&BankInfo> {
        self.by_mint.get(mint)
    }

    pub fn len(&self) -> usize {
        self.by_mint.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_mint.is_empty()
    }

    /// Ensure the fee-payer has an SPL Associated Token Account for
    /// every mint in the registry. Existing ATAs are left alone; only
    /// missing ones get created in a single batched tx (≤ ~10 ATA
    /// creates fit comfortably in one tx).
    ///
    /// The ATA program's `Idempotent` instruction (variant tag = 1)
    /// no-ops if the account already exists, so this is safe to retry
    /// across restarts without re-burning rent.
    ///
    /// Note: this only CREATES the empty token accounts. For the
    /// liquidator to actually settle loans, the debt-side ATAs must
    /// still be funded with the debt asset (USDC etc.) — that's an
    /// off-chain transfer the bot can't perform on the operator's
    /// behalf.
    pub async fn ensure_atas_for(&self, rpc: &Rpc, fee_payer: &Keypair) -> Result<()> {
        if self.by_mint.is_empty() {
            return Ok(());
        }
        let owner = fee_payer.pubkey();

        // Derive each ATA + remember the per-ATA token program so we
        // build the correct create-ix later.
        let derived: Vec<(Pubkey, Pubkey, Pubkey)> = self
            .by_mint
            .values()
            .map(|info| (info.mint, info.ata_for(&owner), info.token_program))
            .collect();

        let ata_pubkeys: Vec<Pubkey> = derived.iter().map(|(_, a, _)| *a).collect();
        let existing = rpc
            .client()
            .get_multiple_accounts(&ata_pubkeys)
            .await
            .context("ensure_atas_for: get_multiple_accounts failed")?;

        let mut missing: Vec<(Pubkey, Pubkey, Pubkey)> = Vec::new();
        for (i, (mint, ata, token_program)) in derived.iter().enumerate() {
            if existing[i].is_none() {
                missing.push((*mint, *ata, *token_program));
            }
        }

        if missing.is_empty() {
            tracing::info!(
                ata_count = derived.len(),
                "all liquidator ATAs already exist; nothing to create",
            );
            return Ok(());
        }

        tracing::info!(
            missing = missing.len(),
            of = derived.len(),
            "creating missing liquidator ATAs (idempotent)",
        );
        for (mint, ata, token_program) in &missing {
            tracing::info!(%mint, %ata, %token_program, "  → will create");
        }

        let ixs: Vec<Instruction> = missing
            .iter()
            .map(|(mint, _ata, token_program)| {
                create_associated_token_account_idempotent_ix(&owner, &owner, mint, token_program)
            })
            .collect();

        let sig = rpc
            .send_signed_labeled("create_atas", ixs, &[fee_payer])
            .await
            .context("ensure_atas_for: create-ATAs tx failed to send")?;
        tracing::info!(%sig, "created liquidator ATAs");
        Ok(())
    }
}

/// Build the SPL Associated Token Account program's
/// `CreateIdempotent` (tag 1) instruction. Doing this by hand instead
/// of pulling in `spl-associated-token-account` because the entire
/// payload is 1 byte and the account list is fixed.
///
/// Account order matches the upstream layout:
///   0. [signer, writable] funding_account
///   1. [writable]         associated_token_account (the derived PDA)
///   2. [readonly]         wallet (the owner of the ATA)
///   3. [readonly]         token_mint
///   4. [readonly]         system_program
///   5. [readonly]         token_program (legacy spl-token or token-2022)
fn create_associated_token_account_idempotent_ix(
    funding: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let (ata, _bump) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program_id(),
    );
    Instruction {
        program_id: ata_program_id(),
        accounts: vec![
            AccountMeta::new(*funding, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        // CreateIdempotent variant tag = 1 (CreateAssociatedTokenAccount = 0)
        data: vec![1u8],
    }
}
