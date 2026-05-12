//! One-shot helper: place a primary `PlaceOrder` ix on the SOL/USDC
//! market, signed by the cranker's fee payer. Reuses the cranker's
//! env-driven Config / Signers / Rpc / ChainReader so it picks up the
//! same RPC, program id, marginfi group, and keypair as the main bot.
//!
//! Usage (from the `crankers/` dir):
//!
//!     cargo run --bin place_order
//!
//! Env overrides (all optional, sensible Ask defaults — matches the
//! shape of the original failing tx that prompted this script):
//!
//!     PLACE_ORDER_SIDE              "ask" | "bid"   (default: ask)
//!     PLACE_ORDER_PRINCIPAL_ATOMS   u64             (default: 10_000_000 = $10 USDC)
//!     PLACE_ORDER_COLLATERAL_ATOMS  u64             (default: 0   — required only for Bids)
//!     PLACE_ORDER_RATE_BPS          u16             (default: 720 = 7.20%)
//!     PLACE_ORDER_TERM_SECONDS      u32             (default: 1_209_600 = 14 days)
//!     PLACE_ORDER_LAST_VALID_TS     i64 unix        (default: 0 = no expiry)
//!     PLACE_ORDER_FLAGS             u8              (default: 0)
//!
//! Identifies the SOL/USDC market by `(debt_mint = USDC, collateral_mint = wSOL)`.
//! Pre-flight checks (cluster reachable, signer == fee payer, mints exist)
//! fail loudly before the tx ever lands.
//!
//! Note: Limit Asks require pre-deposited USDC shares on the signer's
//! `ClaimedSeat`. If you haven't run `Deposit` first this tx will land
//! on chain and revert with a balance-check error from the program.

use std::sync::{atomic::AtomicBool, Arc};

use anyhow::{bail, Context, Result};
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;

use ydelta_crankers::{
    chain_reader::ChainReader,
    config::Config,
    rpc::Rpc,
    signer::Signers,
    swb_crank,
};
use ydelta::program::instruction_builders::place_order_instruction;
use ydelta::state::{OrderType, Side};

/// Marginfi `OracleSetup::SwitchboardPull` tag. Mirrored from
/// `marginfi-mocks::state::OracleSetup` so we don't pull in that whole
/// crate just for a one-byte compare.
const ORACLE_SETUP_SWITCHBOARD_PULL: u8 = 4;

/// Bank-config oracle accessors. Same byte offsets as
/// `marginfi-mocks::state::BankConfigView` (post-8-byte-disc Bank body +
/// 288-byte `BankConfig` offset). Re-derived inline so this binary can
/// peek without importing the slim view.
const BANK_BODY_OFFSET: usize = 8;
const BANK_CONFIG_OFFSET: usize = 288;
const BC_ORACLE_SETUP: usize = 313;
const BC_ORACLE_MAX_AGE: usize = 504;

// SPL mints. Hard-coded because they're network-stable and the cranker
// env doesn't carry them — we just need to find the right MarketView.
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

    // Boot-time bank discovery from on-chain markets — same path the
    // main binary uses, so we share the BankRegistry surface.
    cfg.discover_banks_from_markets(&rpc, &chain)
        .await
        .context("discover_banks_from_markets")?;

    // Find SOL/USDC market: debt_mint = USDC, collateral_mint = wSOL.
    let usdc: Pubkey = USDC_MINT.parse().unwrap();
    let wsol: Pubkey = WSOL_MINT.parse().unwrap();
    let markets = chain.list_markets().await.context("list_markets")?;
    let market = markets
        .into_iter()
        .find(|m| m.debt_mint == usdc && m.collateral_mint == wsol)
        .with_context(|| format!("no market with debt={} collateral={}", usdc, wsol))?;
    if market.is_paused {
        bail!("market {} is paused — refusing to place an order", market.address);
    }

    let debt_bank = cfg
        .banks
        .get(&usdc)
        .with_context(|| format!("bank registry missing entry for USDC mint {}", usdc))?
        .clone();
    let collateral_bank = cfg
        .banks
        .get(&wsol)
        .with_context(|| format!("bank registry missing entry for wSOL mint {}", wsol))?
        .clone();

    // Order params (env-overridable).
    let side = parse_side(&env_or("PLACE_ORDER_SIDE", "ask"))?;
    let principal_atoms: u64 = env_parse("PLACE_ORDER_PRINCIPAL_ATOMS", 10_000_000)?;
    let collateral_atoms: u64 = env_parse("PLACE_ORDER_COLLATERAL_ATOMS", 0)?;
    let rate_bps: u16 = env_parse("PLACE_ORDER_RATE_BPS", 720)?;
    let term_seconds: u32 = env_parse("PLACE_ORDER_TERM_SECONDS", 14 * 24 * 60 * 60)?;
    let last_valid_unix_ts: i64 = env_parse("PLACE_ORDER_LAST_VALID_TS", 0)?;
    let flags: u8 = env_parse("PLACE_ORDER_FLAGS", 0)?;

    // For an Ask the signer is the lender — their USDC ATA is the source
    // of any settlement transfers. For a Bid the signer is the borrower —
    // their USDC ATA is the destination of borrowed atoms. Same field,
    // same derivation either way.
    let borrower_debt_token = debt_bank.ata_for(&signers.fee_payer.pubkey());

    let ix = place_order_instruction(
        &market.address,
        &signers.fee_payer.pubkey(),
        &cfg.marginfi_group,
        &debt_bank.bank,
        &collateral_bank.bank,
        &debt_bank.oracles,
        &collateral_bank.oracles,
        &debt_bank.liquidity_vault,
        &debt_bank.liquidity_vault_authority,
        &borrower_debt_token,
        &usdc,
        &debt_bank.token_program,
        &cfg.marginfi_program_id,
        side,
        OrderType::Limit,
        rate_bps,
        term_seconds,
        principal_atoms,
        collateral_atoms,
        last_valid_unix_ts,
        flags,
        /*seat_index_hint=*/ None,
        /*borrower_ltv_bps=*/ None,
    );

    // Pre-flight: if either bank's primary oracle is `SwitchboardPull`
    // and its on-chain `last_update_ts` is older than the bank's
    // `oracle_max_age`, send a Switchboard crank tx first. Matches
    // eva01's pattern of separating the crank tx from the downstream
    // user tx; the freshly-cranked feed gives us a ~max_age window for
    // the PlaceOrder to land.
    let skip_crank: bool = env_parse("PLACE_ORDER_SKIP_SWB_CRANK", false)?;
    if !skip_crank {
        maybe_crank_swb(
            &rpc,
            &cfg.rpc_url,
            &signers.fee_payer,
            "debt",
            &debt_bank.bank,
            debt_bank.primary_oracle(),
        )
        .await?;
        maybe_crank_swb(
            &rpc,
            &cfg.rpc_url,
            &signers.fee_payer,
            "collateral",
            &collateral_bank.bank,
            collateral_bank.primary_oracle(),
        )
        .await?;
    }

    tracing::info!(
        market = %market.address,
        side = ?side,
        principal_atoms,
        collateral_atoms,
        rate_bps,
        term_seconds,
        signer = %signers.fee_payer.pubkey(),
        "submitting PlaceOrder",
    );

    let sig = rpc
        .send_signed_labeled("place_order", vec![ix], &[&signers.fee_payer])
        .await
        .context("send_signed")?;

    tracing::info!(%sig, "PlaceOrder landed");
    println!("{}", sig);
    Ok(())
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(v) => v
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("env {} = {:?}: {}", name, v, e)),
        Err(_) => Ok(default),
    }
}

/// Read `oracle_setup` + `oracle_max_age` from a bank account's
/// BankConfig region, then if `oracle_setup == SwitchboardPull` and the
/// feed is stale enough that the next `read_oracle_price` would reject
/// it, send a one-shot Switchboard crank tx for that feed.
async fn maybe_crank_swb(
    rpc: &Rpc,
    rpc_url: &str,
    payer: &solana_sdk::signature::Keypair,
    label: &'static str,
    bank: &Pubkey,
    oracle: Pubkey,
) -> Result<()> {
    let client = rpc.client();
    let bank_data = client
        .get_account_data(bank)
        .await
        .with_context(|| format!("get_account_data for {} bank {}", label, bank))?;
    let cfg_start = BANK_BODY_OFFSET + BANK_CONFIG_OFFSET;
    if bank_data.len() < cfg_start + BC_ORACLE_MAX_AGE + 2 {
        anyhow::bail!("{} bank {} data too small for BankConfig view", label, bank);
    }
    let oracle_setup = bank_data[cfg_start + BC_ORACLE_SETUP];
    if oracle_setup != ORACLE_SETUP_SWITCHBOARD_PULL {
        return Ok(());
    }
    let max_age = u16::from_le_bytes(
        bank_data[cfg_start + BC_ORACLE_MAX_AGE..cfg_start + BC_ORACLE_MAX_AGE + 2]
            .try_into()
            .unwrap(),
    ) as i64;
    // Match yDelta's `DEFAULT_ORACLE_MAX_AGE_SECS = 60` when the bank
    // leaves `oracle_max_age` at 0 (its sentinel value).
    let effective_max_age = if max_age == 0 { 60 } else { max_age };

    let oracle_data = client
        .get_account_data(&oracle)
        .await
        .with_context(|| format!("get_account_data for {} oracle {}", label, oracle))?;
    let last_ts = swb_crank::decode_swb_last_update_ts(&oracle_data)
        .with_context(|| format!("decode_swb_last_update_ts for {}", oracle))?;
    let now = chrono_now_unix(&client).await?;
    let age = now - last_ts;

    if age <= effective_max_age {
        tracing::info!(
            label,
            %bank,
            %oracle,
            age,
            effective_max_age,
            "swb feed already fresh; skipping crank",
        );
        return Ok(());
    }

    tracing::info!(
        label,
        %bank,
        %oracle,
        age,
        effective_max_age,
        "swb feed stale; cranking before PlaceOrder",
    );
    let sig = swb_crank::crank_feeds(rpc_url, payer, vec![oracle])
        .await
        .with_context(|| format!("swb crank for {}", oracle))?;
    tracing::info!(label, %oracle, %sig, "swb crank landed");
    Ok(())
}

/// Read the on-chain `Clock` sysvar's `unix_timestamp` field via
/// `getAccountInfo`. We can't just call `SystemTime::now()` because
/// Solana's stake-weighted Clock lags wall-clock by tens of minutes on
/// mainnet, and yDelta computes oracle age relative to *that* Clock.
async fn chrono_now_unix(
    client: &solana_client::nonblocking::rpc_client::RpcClient,
) -> Result<i64> {
    // Clock sysvar layout: slot(u64) + epoch_start_timestamp(i64) +
    // epoch(u64) + leader_schedule_epoch(u64) + unix_timestamp(i64).
    let sysvar_clock: Pubkey = "SysvarC1ock11111111111111111111111111111111".parse().unwrap();
    let data = client
        .get_account_data(&sysvar_clock)
        .await
        .context("get_account_data for Clock sysvar")?;
    if data.len() < 8 + 8 + 8 + 8 + 8 {
        anyhow::bail!("Clock sysvar data too small: {} bytes", data.len());
    }
    Ok(i64::from_le_bytes(data[32..40].try_into().unwrap()))
}

fn parse_side(s: &str) -> Result<Side> {
    match s.to_ascii_lowercase().as_str() {
        "ask" | "lender" | "lend" => Ok(Side::Ask),
        "bid" | "borrower" | "borrow" => Ok(Side::Bid),
        other => bail!("PLACE_ORDER_SIDE = {:?}; expected ask|bid", other),
    }
}
