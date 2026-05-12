//! Environment-driven configuration.
//!
//! All knobs flow through `Config::from_env()`. Required vars panic at
//! boot rather than at first use, so a misconfigured deploy fails fast.
//! Optional vars have documented defaults.
//!
//! Secrets (keypairs) are referenced by file path here; the actual
//! signers are loaded by `signer.rs` from those paths. Keep paths in env
//! vars; mount the key files via Railway secret files or a tmpfs.

use std::{fmt, path::PathBuf, str::FromStr, time::Duration};

use anyhow::{anyhow, Context, Result};
use solana_program::pubkey::Pubkey;

use crate::bank_registry::BankRegistry;

/// Where a keypair's secret bytes come from. Either a file path on the
/// container's filesystem (mounted via a volume / secret-file feature)
/// or an inline base58 string from an env var. Both work; pick whichever
/// fits your PaaS. Inline is simpler on Railway (no entrypoint script);
/// path is preferred when you have a real secrets-file mechanism.
#[derive(Clone)]
pub enum KeypairSource {
    Path(PathBuf),
    Base58(String),
}

impl fmt::Debug for KeypairSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeypairSource::Path(p) => write!(f, "Path({})", p.display()),
            KeypairSource::Base58(_) => write!(f, "Base58(<redacted>)"),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub network: Network,
    pub rpc_url: String,

    pub program_id: Pubkey,
    pub marginfi_program_id: Pubkey,
    pub marginfi_group: Pubkey,

    pub fee_payer_keypair: KeypairSource,
    /// Optional assertion that the loaded fee-payer matches this
    /// pubkey. Catches misconfiguration where the configured keypair
    /// source resolves to a curator key (or any other unintended
    /// keypair). When unset, the signer loads whatever the source
    /// resolves to.
    pub fee_payer_expected_pubkey: Option<Pubkey>,

    /// Per-mint marginfi bank metadata. Empty at construction time
    /// (env doesn't carry it) — populated at boot by
    /// `discover_banks_from_markets` reading the indexer's market list
    /// and each market's `MarketFixed` account.
    pub banks: BankRegistry,

    /// Per-profile curator signers. Key = `(global_vault, profile_id)`.
    pub curator_signers: Vec<CuratorSignerConfig>,

    pub handlers: HandlersConfig,
    pub thresholds: ThresholdsConfig,
    pub priority_fee_micro_lamports: u64,
    /// Below this fee-payer balance (in lamports), the supervisor pauses
    /// every handler that signs with the fee payer (promoter, claimer,
    /// liquidator, policy_sync). curator_keeper is unaffected — it uses
    /// per-curator keypairs, which have their own balance concerns.
    /// Default 50_000_000 lamports = 0.05 SOL — enough headroom for
    /// ~250 typical txs at 0.0002 SOL each before the bot truly starves.
    pub min_signer_balance_lamports: u64,
}

#[derive(Debug, Clone)]
pub struct CuratorSignerConfig {
    pub global_vault: Pubkey,
    pub profile_id: u8,
    pub keypair: KeypairSource,
    /// How the keeper picks the target ask rate.
    pub rate_target: RateTarget,
    /// Static order term in seconds. Must be ≤ `RiskProfile.max_term_seconds`.
    pub target_term_seconds: u32,
    /// Markets this curator manages. The keeper iterates over these and
    /// keeps each order in sync.
    pub managed_markets: Vec<Pubkey>,
    /// Bootstrap baseline used in the risk-weighted exposure formula
    /// when a profile has zero deposits. The keeper computes
    /// `target = pool × risk_score_bps / 10_000` per (profile, market),
    /// where `pool = max(profile.total_principal_atoms, baseline)`.
    /// `risk_score_bps` blends `max_ltv_bps`, `max_term_seconds`, and
    /// `allowed_market_max` into a [0, 10_000] score. Default 500 USDC
    /// (= 500_000_000 atoms at 6-decimal mints) — sized to keep the
    /// single-vault bootstrap quote bounded until real deposits land.
    pub exposure_baseline_atoms: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum RateTarget {
    /// Quote at a fixed rate. Useful for bootstrapping or when marginfi
    /// is the wrong benchmark.
    Static { rate_bps: u16 },
    /// Quote at `marginfi_supply_apr + α × (marginfi_borrow_apr - marginfi_supply_apr)`.
    /// `alpha_bps ∈ [0, 10_000]`. `fallback_bps` is used if the bank
    /// read fails.
    Dynamic { alpha_bps: u16, fallback_bps: u16 },
}

#[derive(Debug, Clone, Copy)]
pub struct HandlersConfig {
    pub promoter_enabled: bool,
    pub claimer_enabled: bool,
    pub liquidator_enabled: bool,
    pub policy_sync_enabled: bool,
    pub curator_keeper_enabled: bool,

    pub promoter_interval: Duration,
    pub claimer_interval: Duration,
    pub liquidator_interval: Duration,
    pub policy_sync_interval: Duration,
    pub curator_keeper_interval: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct ThresholdsConfig {
    /// Liquidator skips loans where the static principal × keeper bps
    /// is below this floor (atoms). Guards against tx-fee-negative work.
    pub min_liquidation_profit_atoms: u64,
    /// Curator keeper skips updates where `|target - current| < this`.
    pub curator_min_delta_bps: u16,
    /// Curator keeper skips a `SetSeatMaxExposureForRiskProfile` when
    /// `|target - on_chain| / max(target, on_chain) < this`. Guards
    /// against per-tick jitter from accrued deposits / repayments
    /// nudging the computed cap by single atoms.
    pub curator_min_exposure_delta_bps: u16,
    /// Curator keeper enforces ≥ this between updates to a given
    /// (profile, market). Avoids back-of-queue churn.
    pub curator_min_update_interval: Duration,
    /// Maturity grace buffer the bot adds on top of the on-chain grace
    /// period. Lets the indexer settle before we race.
    pub maturity_extra_buffer: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// `solana-test-validator` / local fork. Used for dev + integration tests.
    Localhost,
    Mainnet,
}

impl Network {
    pub fn as_str(&self) -> &'static str {
        match self {
            Network::Localhost => "localhost",
            Network::Mainnet => "mainnet",
        }
    }
}

impl FromStr for Network {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "localhost" | "local" => Ok(Network::Localhost),
            "mainnet" | "mainnet-beta" => Ok(Network::Mainnet),
            other => Err(anyhow!("NETWORK must be localhost or mainnet, got {other}")),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // `.env` is optional — Railway / GitHub Actions inject vars
        // directly. Local dev uses a gitignored `.env` next to the binary.
        let _ = dotenvy::dotenv();

        let network: Network = require_var("NETWORK")?.parse()?;
        let rpc_url = require_var("RPC_URL")?;

        let program_id = parse_pubkey(
            &optional_var("YDELTA_PROGRAM_ID").unwrap_or_else(|| ydelta::id().to_string()),
        )?;
        let marginfi_program_id = parse_pubkey(&require_var("MARGINFI_PROGRAM_ID")?)?;
        let marginfi_group = parse_pubkey(&require_var("MARGINFI_GROUP")?)?;

        // Accept the fee-payer keypair as either a file path
        // (`FEE_PAYER_KEYPAIR`) or an inline base58 string
        // (`FEE_PAYER_KEYPAIR_BASE58`). Exactly one must be set —
        // refuse to start on ambiguous config rather than silently
        // pick one source over the other.
        let fee_payer_keypair = match (
            optional_var("FEE_PAYER_KEYPAIR"),
            optional_var("FEE_PAYER_KEYPAIR_BASE58"),
        ) {
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "both FEE_PAYER_KEYPAIR and FEE_PAYER_KEYPAIR_BASE58 are set; \
                     pick exactly one"
                ));
            }
            (Some(path), None) => KeypairSource::Path(PathBuf::from(path)),
            (None, Some(b58)) => KeypairSource::Base58(b58),
            (None, None) => {
                return Err(anyhow!(
                    "set either FEE_PAYER_KEYPAIR=<file path> or \
                     FEE_PAYER_KEYPAIR_BASE58=<base58 secret>"
                ));
            }
        };
        let fee_payer_expected_pubkey = optional_var("FEE_PAYER_PUBKEY")
            .map(|s| parse_pubkey(&s))
            .transpose()?;

        // Bank metadata is discovered from chain at boot — no env input.
        let banks = BankRegistry::default();

        let curator_signers = parse_curator_signers()?;

        let handlers = HandlersConfig {
            promoter_enabled: bool_var("PROMOTER_ENABLED", true)?,
            claimer_enabled: bool_var("CLAIMER_ENABLED", true)?,
            liquidator_enabled: bool_var("LIQUIDATOR_ENABLED", true)?,
            policy_sync_enabled: bool_var("POLICY_SYNC_ENABLED", true)?,
            curator_keeper_enabled: bool_var("CURATOR_KEEPER_ENABLED", true)?,

            promoter_interval: secs_var("PROMOTER_INTERVAL_SEC", 5)?,
            claimer_interval: secs_var("CLAIMER_INTERVAL_SEC", 30)?,
            liquidator_interval: secs_var("LIQUIDATOR_INTERVAL_SEC", 10)?,
            policy_sync_interval: secs_var("POLICY_SYNC_INTERVAL_SEC", 60)?,
            curator_keeper_interval: secs_var("CURATOR_KEEPER_INTERVAL_SEC", 60)?,
        };

        let thresholds = ThresholdsConfig {
            min_liquidation_profit_atoms: u64_var("MIN_LIQUIDATION_PROFIT_ATOMS", 1_000_000)?,
            curator_min_delta_bps: u64_var("CURATOR_MIN_DELTA_BPS", 25)? as u16,
            curator_min_exposure_delta_bps: u64_var("CURATOR_MIN_EXPOSURE_DELTA_BPS", 500)? as u16,
            curator_min_update_interval: secs_var("CURATOR_MIN_UPDATE_INTERVAL_SEC", 300)?,
            maturity_extra_buffer: secs_var("MATURITY_EXTRA_BUFFER_SEC", 30)?,
        };

        let priority_fee_micro_lamports = u64_var("PRIORITY_FEE_MICRO_LAMPORTS", 1_000)?;
        let min_signer_balance_lamports = u64_var("MIN_SIGNER_BALANCE_LAMPORTS", 50_000_000)?;

        Ok(Self {
            network,
            rpc_url,
            program_id,
            marginfi_program_id,
            marginfi_group,
            fee_payer_keypair,
            fee_payer_expected_pubkey,
            banks, // empty; populated by `discover_banks_from_markets` once RPC + indexer are up
            curator_signers,
            handlers,
            thresholds,
            priority_fee_micro_lamports,
            min_signer_balance_lamports,
        })
    }

    /// Resolve the per-mint bank metadata by reading each bank account
    /// on chain. Must be called once after `from_env` and before any
    /// handler runs, so handler code can rely on `self.banks` being
    /// fully populated.
    pub async fn discover_banks_from_markets(
        &mut self,
        rpc: &crate::rpc::Rpc,
        chain: &crate::chain_reader::ChainReader,
    ) -> Result<()> {
        self.banks =
            BankRegistry::discover_from_markets(rpc, chain, &self.marginfi_program_id).await?;
        Ok(())
    }
}

/// Manual `Debug` impl that redacts secrets. Anywhere `Config` is
/// `{:?}`-printed (panics, `tracing::error!(?cfg)`, future code) would
/// otherwise dump RPC API keys and any basic-auth-bearing indexer URLs
/// to logs.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("network", &self.network)
            .field("rpc_url", &redact_url(&self.rpc_url))
            .field("program_id", &self.program_id)
            .field("marginfi_program_id", &self.marginfi_program_id)
            .field("marginfi_group", &self.marginfi_group)
            .field("fee_payer_keypair", &self.fee_payer_keypair)
            .field("fee_payer_expected_pubkey", &self.fee_payer_expected_pubkey)
            .field("banks", &self.banks)
            .field(
                "curator_signers",
                &format!("<{} entries>", self.curator_signers.len()),
            )
            .field("handlers", &self.handlers)
            .field("thresholds", &self.thresholds)
            .field(
                "priority_fee_micro_lamports",
                &self.priority_fee_micro_lamports,
            )
            .finish()
    }
}

/// Strip query string + userinfo from a URL before logging it.
/// Provider URLs (Helius/Triton/Quicknode) put API keys in
/// `?api-key=…`; basic-auth URLs put creds in `user:pass@host`.
/// Emitting the raw URL would leak those to stdout / log aggregators.
pub fn redact_url(url: &str) -> String {
    let no_query = url.split('?').next().unwrap_or(url);
    if let Some(scheme_end) = no_query.find("://") {
        let after_scheme = &no_query[scheme_end + 3..];
        if let Some(at) = after_scheme.find('@') {
            return format!("{}://{}", &no_query[..scheme_end], &after_scheme[at + 1..]);
        }
    }
    no_query.to_string()
}

fn parse_curator_signers() -> Result<Vec<CuratorSignerConfig>> {
    // Curators are declared via a `CURATORS` env var: a JSON array of
    // objects. Example:
    //
    //   CURATORS='[{"global_vault":"Vault…","profile_id":1,
    //              "keypair":"/secrets/curator-1.json",
    //              "target_rate_bps":650,"target_term_seconds":2592000,
    //              "markets":["Market1…","Market2…"]}]'
    //
    // JSON-in-env keeps the per-curator config from sprawling across N
    // numbered env-var families. Railway's UI handles multi-line values.
    let raw = std::env::var("CURATORS").unwrap_or_else(|_| "[]".to_string());
    let parsed: Vec<CuratorSignerJson> =
        serde_json::from_str(&raw).context("CURATORS must be JSON array")?;
    parsed
        .into_iter()
        .map(|c| {
            let rate_target = match (c.target_rate_alpha_bps, c.target_rate_bps) {
                (Some(alpha), fallback) => RateTarget::Dynamic {
                    alpha_bps: alpha,
                    fallback_bps: fallback.or(c.target_rate_fallback_bps).ok_or_else(|| {
                        anyhow!(
                            "curator (vault={}, profile_id={}): dynamic mode needs \
                                 `target_rate_fallback_bps` (or `target_rate_bps` as fallback)",
                            c.global_vault,
                            c.profile_id
                        )
                    })?,
                },
                (None, Some(static_bps)) => RateTarget::Static {
                    rate_bps: static_bps,
                },
                (None, None) => {
                    return Err(anyhow!(
                        "curator (vault={}, profile_id={}): must set either \
                         `target_rate_bps` (static) or `target_rate_alpha_bps` (dynamic)",
                        c.global_vault,
                        c.profile_id
                    ))
                }
            };
            // Each curator entry must specify exactly one of:
            //   "keypair":        "/secrets/curator-N.json"   (file path)
            //   "keypair_base58": "5J8b..."                   (inline secret)
            let keypair = match (&c.keypair, &c.keypair_base58) {
                (Some(_), Some(_)) => {
                    return Err(anyhow!(
                        "curator (vault={}, profile_id={}): both `keypair` and \
                         `keypair_base58` set; pick one",
                        c.global_vault,
                        c.profile_id
                    ));
                }
                (Some(path), None) => KeypairSource::Path(PathBuf::from(path)),
                (None, Some(b58)) => KeypairSource::Base58(b58.clone()),
                (None, None) => {
                    return Err(anyhow!(
                        "curator (vault={}, profile_id={}): set either `keypair` \
                         (file path) or `keypair_base58` (inline)",
                        c.global_vault,
                        c.profile_id
                    ));
                }
            };
            Ok(CuratorSignerConfig {
                global_vault: parse_pubkey(&c.global_vault)?,
                profile_id: c.profile_id,
                keypair,
                rate_target,
                target_term_seconds: c.target_term_seconds,
                managed_markets: c
                    .markets
                    .iter()
                    .map(|m| parse_pubkey(m))
                    .collect::<Result<Vec<_>>>()?,
                exposure_baseline_atoms: c.exposure_baseline_atoms.unwrap_or(500_000_000),
            })
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct CuratorSignerJson {
    global_vault: String,
    profile_id: u8,
    #[serde(default)]
    keypair: Option<String>,
    #[serde(default)]
    keypair_base58: Option<String>,
    /// Static target rate, or fallback for dynamic mode.
    #[serde(default)]
    target_rate_bps: Option<u16>,
    /// Set to enable dynamic marginfi-following.
    #[serde(default)]
    target_rate_alpha_bps: Option<u16>,
    /// Explicit fallback for dynamic mode (otherwise `target_rate_bps` is used).
    #[serde(default)]
    target_rate_fallback_bps: Option<u16>,
    target_term_seconds: u32,
    markets: Vec<String>,
    /// Baseline used when the profile has no deposits. Optional;
    /// defaults to 500 USDC atoms (`500_000_000`) when unset.
    #[serde(default)]
    exposure_baseline_atoms: Option<u64>,
}

fn require_var(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| anyhow!("missing required env var {name}"))
}

fn optional_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn bool_var(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            other => Err(anyhow!("{name}: expected bool, got {other}")),
        },
        Err(_) => Ok(default),
    }
}

fn u64_var(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(v) => v.parse().map_err(|e| anyhow!("{name}: {e}")),
        Err(_) => Ok(default),
    }
}

fn secs_var(name: &str, default_secs: u64) -> Result<Duration> {
    Ok(Duration::from_secs(u64_var(name, default_secs)?))
}

fn parse_pubkey(s: &str) -> Result<Pubkey> {
    Pubkey::from_str(s).map_err(|e| anyhow!("invalid pubkey {s}: {e}"))
}
