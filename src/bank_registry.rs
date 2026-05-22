//! Per-mint marginfi bank metadata, chain-discovered at boot from
//! `MarketFixed.{debt,collateral}_lending_pool`.

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

fn ata_program_id() -> Pubkey {
    Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()
}

pub fn spl_token_legacy_id() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}

pub fn spl_token_2022_id() -> Pubkey {
    Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap()
}

#[derive(Debug, Clone)]
pub struct BankInfo {
    pub mint: Pubkey,
    pub bank: Pubkey,
    pub liquidity_vault: Pubkey,
    pub liquidity_vault_authority: Pubkey,
    /// 1-5 oracle pubkeys. The vault-settle CPI path uses `oracles[0]`
    /// only; settle/liquidate paths pass the full slice through.
    pub oracles: Vec<Pubkey>,
    pub token_program: Pubkey,
    /// marginfi `OracleSetup` discriminant. `4 == SwitchboardPull` — the
    /// only pull-based (must-crank) setup our markets use.
    pub oracle_setup: u8,
}

/// marginfi `OracleSetup::SwitchboardPull`.
const ORACLE_SETUP_SWITCHBOARD_PULL: u8 = 4;

impl BankInfo {
    pub fn primary_oracle(&self) -> Pubkey {
        self.oracles
            .first()
            .copied()
            .unwrap_or_else(Pubkey::default)
    }

    /// True for Switchboard On-Demand pull feeds, which the cranker must
    /// refresh itself (Pyth-Push feeds are kept fresh by Pyth-DA).
    pub fn is_switchboard_pull(&self) -> bool {
        self.oracle_setup == ORACLE_SETUP_SWITCHBOARD_PULL
    }

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

/// Mirrors marginfi-v2's `LIQUIDITY_VAULT_AUTHORITY_SEED` byte-for-byte.
const LIQUIDITY_VAULT_AUTHORITY_SEED: &[u8] = b"liquidity_vault_auth";

impl BankRegistry {
    pub async fn discover_from_markets(
        rpc: &Rpc,
        chain: &ChainReader,
        marginfi_program: &Pubkey,
    ) -> Result<Self> {
        let markets = chain
            .refresh_markets()
            .await
            .context("BANKS discover: ChainReader::refresh_markets failed")?;
        if markets.is_empty() {
            tracing::warn!("BANKS discover: chain returned 0 markets; bot will idle");
            return Ok(Self::default());
        }

        // Refuse to start if two markets bind the same mint to
        // different banks — different oracles + rates would silently
        // corrupt liquidator math.
        let mut mint_to_bank: HashMap<Pubkey, Pubkey> = HashMap::new();
        for m in &markets {
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

    async fn resolve_from_chain(
        rpc: &RpcClient,
        marginfi_program: &Pubkey,
        mint_to_bank: &HashMap<Pubkey, Pubkey>,
    ) -> Result<Self> {
        if mint_to_bank.is_empty() {
            return Ok(Self::default());
        }

        let mut entries: Vec<(Pubkey, Pubkey)> =
            mint_to_bank.iter().map(|(m, b)| (*m, *b)).collect();
        entries.sort_by_key(|(m, _)| m.to_bytes());

        let bank_pubkeys: Vec<Pubkey> = entries.iter().map(|(_, b)| *b).collect();
        let bank_accounts = rpc
            .get_multiple_accounts(&bank_pubkeys)
            .await
            .context("BANKS resolve: get_multiple_accounts(banks) failed")?;

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
                    oracle_setup: view.oracle_setup,
                },
            );
        }
        Ok(Self { by_mint })
    }

    pub fn get(&self, mint: &Pubkey) -> Option<&BankInfo> {
        self.by_mint.get(mint)
    }

    /// True if any discovered bank uses a Switchboard On-Demand pull feed —
    /// i.e. the swb cranker is worth booting.
    pub fn has_switchboard_pull(&self) -> bool {
        self.by_mint.values().any(|b| b.is_switchboard_pull())
    }

    /// Primary oracle of every SwitchboardPull bank — the feeds the periodic
    /// cranker must keep fresh on-chain (deduped).
    pub fn switchboard_pull_oracles(&self) -> Vec<Pubkey> {
        let mut out: Vec<Pubkey> = self
            .by_mint
            .values()
            .filter(|b| b.is_switchboard_pull())
            .map(|b| b.primary_oracle())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn mints(&self) -> impl Iterator<Item = &Pubkey> {
        self.by_mint.keys()
    }

    pub fn len(&self) -> usize {
        self.by_mint.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_mint.is_empty()
    }

    /// Add new mints from `other` without overwriting cached entries.
    pub fn merge_from(&mut self, other: BankRegistry) {
        for (mint, info) in other.by_mint {
            self.by_mint.entry(mint).or_insert(info);
        }
    }

    /// Create any missing ATAs for `owner` across every known mint.
    /// Uses the `CreateIdempotent` variant so re-runs are no-ops.
    /// Only creates empty accounts — the operator must fund the
    /// debt-side ATAs for the liquidator to actually settle.
    pub async fn ensure_atas_for(&self, rpc: &Rpc, fee_payer: &Keypair) -> Result<()> {
        if self.by_mint.is_empty() {
            return Ok(());
        }
        let owner = fee_payer.pubkey();

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

/// `CreateIdempotent` (tag 1) — open-coded so we don't pull in the
/// full `spl-associated-token-account` crate for a single instruction.
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
        data: vec![1u8],
    }
}
