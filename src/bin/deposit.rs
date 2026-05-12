//! One-shot helper: deposit `amount_atoms` of USDC (or wSOL) from the
//! fee-payer's ATA into their `ClaimedSeat` on the SOL/USDC market.
//! Atoms hop wallet ATA → market staging vault → marginfi
//! lender/borrower integration account via the program's `Deposit` ix
//! (tag 2). Same env/Config plumbing as `place_order`.
//!
//! Usage (from `crankers/`):
//!
//!     cargo run --release --bin deposit                # 10 USDC (lender side)
//!     DEPOSIT_MINT=wsol cargo run --release --bin deposit
//!
//! Env overrides:
//!
//!     DEPOSIT_MINT          "usdc" | "wsol"   (default: usdc — the market's debt mint)
//!     DEPOSIT_AMOUNT_ATOMS  u64               (default: 10_000_000 = $10 USDC / 0.01 wSOL)
//!
//! `usdc` deposits into the **lender-side** integration account
//! (debt-mint side) — required before placing an Ask.
//! `wsol` deposits into the **borrower-side** integration account
//! (collateral-mint side) — required before placing a Bid.

use std::sync::{atomic::AtomicBool, Arc};

use anyhow::{bail, Context, Result};
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;

use ydelta_crankers::{
    chain_reader::ChainReader,
    config::Config,
    rpc::Rpc,
    signer::Signers,
};
use ydelta::program::instruction_builders::deposit_instruction;

const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut cfg = Config::from_env().context("Config::from_env")?;
    let signers = Signers::load(&cfg).context("Signers::load")?;
    let rpc = Rpc::new(cfg.rpc_url.clone(), cfg.priority_fee_micro_lamports)
        .with_stop_signal(Arc::new(AtomicBool::new(false)));
    let chain = ChainReader::new(rpc.clone(), cfg.program_id);

    cfg.discover_banks_from_markets(&rpc, &chain)
        .await
        .context("discover_banks_from_markets")?;

    let usdc: Pubkey = USDC_MINT.parse().unwrap();
    let wsol: Pubkey = WSOL_MINT.parse().unwrap();
    let markets = chain.list_markets().await.context("list_markets")?;
    let market = markets
        .into_iter()
        .find(|m| m.debt_mint == usdc && m.collateral_mint == wsol)
        .with_context(|| format!("no market with debt={} collateral={}", usdc, wsol))?;
    if market.is_paused {
        bail!("market {} is paused — refusing to deposit", market.address);
    }

    let mint_choice = std::env::var("DEPOSIT_MINT")
        .unwrap_or_else(|_| "usdc".to_string())
        .to_ascii_lowercase();
    let (deposit_mint, default_amount) = match mint_choice.as_str() {
        "usdc" => (usdc, 10_000_000_u64),       // $10 USDC (6 decimals)
        "wsol" | "sol" => (wsol, 10_000_000_u64), // 0.01 wSOL (9 decimals)
        other => bail!("DEPOSIT_MINT = {:?}; expected usdc|wsol", other),
    };
    let amount_atoms: u64 = match std::env::var("DEPOSIT_AMOUNT_ATOMS") {
        Ok(v) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("DEPOSIT_AMOUNT_ATOMS = {:?}: {}", v, e))?,
        Err(_) => default_amount,
    };

    let bank = cfg
        .banks
        .get(&deposit_mint)
        .with_context(|| format!("bank registry missing entry for mint {}", deposit_mint))?
        .clone();
    let trader_token = bank.ata_for(&signers.fee_payer.pubkey());

    let ix = deposit_instruction(
        &market.address,
        &signers.fee_payer.pubkey(),
        &deposit_mint,
        &usdc, // market's debt mint — used by the ix to decide lender vs borrower integration acct
        &trader_token,
        &bank.token_program,
        &cfg.marginfi_group,
        &bank.bank,
        &bank.liquidity_vault,
        &cfg.marginfi_program_id,
        amount_atoms,
        /*trader_index_hint=*/ None,
    );

    tracing::info!(
        market = %market.address,
        mint = %deposit_mint,
        amount_atoms,
        signer = %signers.fee_payer.pubkey(),
        trader_token = %trader_token,
        "submitting Deposit",
    );

    let sig = rpc
        .send_signed_labeled("deposit", vec![ix], &[&signers.fee_payer])
        .await
        .context("send_signed")?;

    tracing::info!(%sig, "Deposit landed");
    println!("{}", sig);
    Ok(())
}
