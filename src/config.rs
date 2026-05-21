use std::{
    fmt,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{anyhow, Result};
use solana_program::pubkey::Pubkey;

use crate::bank_registry::BankRegistry;

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
    /// Optional assertion; refuses to start if loaded keypair's pubkey
    /// doesn't match. Catches swap-ups between keypair sources.
    pub fee_payer_expected_pubkey: Option<Pubkey>,

    /// Live-mutable so a background task can pick up newly-created
    /// markets without a restart.
    pub banks: Arc<RwLock<BankRegistry>>,

    pub handlers: HandlersConfig,
    pub thresholds: ThresholdsConfig,
    pub priority_fee_micro_lamports: u64,
    pub min_signer_balance_lamports: u64,
    pub banks_refresh_interval: Duration,
    pub curator_keypairs_base58: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct HandlersConfig {
    pub promoter_enabled: bool,
    pub claimer_enabled: bool,
    pub liquidator_enabled: bool,
    pub curator_fee_claimer_enabled: bool,

    pub promoter_interval: Duration,
    pub claimer_interval: Duration,
    pub liquidator_interval: Duration,
    pub curator_fee_claimer_interval: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct ThresholdsConfig {
    pub min_liquidation_profit_atoms: u64,
    pub maturity_extra_buffer: Duration,
    pub min_curator_fee_claim_atoms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
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
        let _ = dotenvy::dotenv();

        let network: Network = require_var("NETWORK")?.parse()?;
        let rpc_url = require_var("RPC_URL")?;

        let program_id = parse_pubkey(
            &optional_var("YDELTA_PROGRAM_ID").unwrap_or_else(|| ydelta::id().to_string()),
        )?;
        let marginfi_program_id = parse_pubkey(&require_var("MARGINFI_PROGRAM_ID")?)?;
        let marginfi_group = parse_pubkey(&require_var("MARGINFI_GROUP")?)?;

        // Reject ambiguous config (both sources set) rather than
        // silently picking one.
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

        let banks = Arc::new(RwLock::new(BankRegistry::default()));

        let handlers = HandlersConfig {
            promoter_enabled: bool_var("PROMOTER_ENABLED", true)?,
            claimer_enabled: bool_var("CLAIMER_ENABLED", true)?,
            liquidator_enabled: bool_var("LIQUIDATOR_ENABLED", true)?,
            curator_fee_claimer_enabled: bool_var("CURATOR_FEE_CLAIMER_ENABLED", false)?,

            promoter_interval: secs_var("PROMOTER_INTERVAL_SEC", 5)?,
            claimer_interval: secs_var("CLAIMER_INTERVAL_SEC", 30)?,
            liquidator_interval: secs_var("LIQUIDATOR_INTERVAL_SEC", 10)?,
            curator_fee_claimer_interval: secs_var("CURATOR_FEE_CLAIMER_INTERVAL_SEC", 3_600)?,
        };

        let thresholds = ThresholdsConfig {
            min_liquidation_profit_atoms: u64_var("MIN_LIQUIDATION_PROFIT_ATOMS", 1_000_000)?,
            maturity_extra_buffer: secs_var("MATURITY_EXTRA_BUFFER_SEC", 30)?,
            min_curator_fee_claim_atoms: u64_var("MIN_CURATOR_FEE_CLAIM_ATOMS", 100_000)?,
        };

        let priority_fee_micro_lamports = u64_var("PRIORITY_FEE_MICRO_LAMPORTS", 1_000)?;
        let min_signer_balance_lamports = u64_var("MIN_SIGNER_BALANCE_LAMPORTS", 50_000_000)?;
        let banks_refresh_interval = secs_var("BANKS_REFRESH_SEC", 300)?;

        let curator_keypairs_base58 = match optional_var("CURATOR_KEYPAIRS_JSON") {
            None => Vec::new(),
            Some(raw) => parse_curator_keypairs_json(&raw)?,
        };

        Ok(Self {
            network,
            rpc_url,
            program_id,
            marginfi_program_id,
            marginfi_group,
            fee_payer_keypair,
            fee_payer_expected_pubkey,
            banks,
            handlers,
            thresholds,
            priority_fee_micro_lamports,
            min_signer_balance_lamports,
            banks_refresh_interval,
            curator_keypairs_base58,
        })
    }

    pub async fn discover_banks_from_markets(
        &self,
        rpc: &crate::rpc::Rpc,
        chain: &crate::chain_reader::ChainReader,
    ) -> Result<()> {
        let fresh =
            BankRegistry::discover_from_markets(rpc, chain, &self.marginfi_program_id).await?;
        let mut guard = self
            .banks
            .write()
            .map_err(|_| anyhow!("banks RwLock poisoned"))?;
        *guard = fresh;
        Ok(())
    }

    /// Merge-only refresh: new mints get added, existing entries stay.
    /// A transient empty fetch never wipes the cache.
    pub async fn refresh_banks(
        &self,
        rpc: &crate::rpc::Rpc,
        chain: &crate::chain_reader::ChainReader,
    ) -> Result<()> {
        let fresh =
            BankRegistry::discover_from_markets(rpc, chain, &self.marginfi_program_id).await?;
        if fresh.is_empty() {
            return Ok(());
        }
        let mut guard = self
            .banks
            .write()
            .map_err(|_| anyhow!("banks RwLock poisoned"))?;
        guard.merge_from(fresh);
        Ok(())
    }

    pub fn banks_snapshot(&self) -> BankRegistry {
        self.banks
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

/// Accepts a JSON array of base58 strings or a single bare base58
/// string (single-curator case).
fn parse_curator_keypairs_json(raw: &str) -> Result<Vec<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        let arr: Vec<String> = serde_json::from_str(trimmed)
            .map_err(|e| anyhow!("CURATOR_KEYPAIRS_JSON: invalid JSON array: {e}"))?;
        Ok(arr.into_iter().map(|s| s.trim().to_string()).collect())
    } else {
        Ok(vec![trimmed.to_string()])
    }
}

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
            .field(
                "banks_count",
                &self.banks.read().map(|g| g.len()).unwrap_or(0),
            )
            .field("handlers", &self.handlers)
            .field("thresholds", &self.thresholds)
            .field(
                "priority_fee_micro_lamports",
                &self.priority_fee_micro_lamports,
            )
            .field("banks_refresh_interval", &self.banks_refresh_interval)
            .field(
                "curator_keypairs_count",
                &self.curator_keypairs_base58.len(),
            )
            .finish()
    }
}

/// Strip query string + userinfo before logging. Provider URLs put API
/// keys in `?api-key=…`; basic-auth URLs put creds in `user:pass@host`.
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
