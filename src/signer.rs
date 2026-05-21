use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{anyhow, bail, Context, Result};
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer as _};

use crate::config::{Config, KeypairSource};

#[derive(Clone)]
pub struct Signers {
    pub fee_payer: Arc<Keypair>,
    /// Curator pubkey → keypair. Empty unless `CURATOR_KEYPAIRS_JSON`
    /// is set. The curator-fee-claimer resolves `profile.curator` here
    /// and only submits for profiles whose key we hold.
    pub curators: HashMap<Pubkey, Arc<Keypair>>,
}

impl Signers {
    pub fn load(cfg: &Config) -> Result<Self> {
        let fee_payer = Arc::new(load_keypair(&cfg.fee_payer_keypair)?);

        if let Some(expected) = cfg.fee_payer_expected_pubkey {
            if fee_payer.pubkey() != expected {
                bail!(
                    "fee-payer pubkey mismatch: loaded keypair resolves to {}, but \
                     FEE_PAYER_PUBKEY={}; refusing to start — check the configured \
                     FEE_PAYER_KEYPAIR / FEE_PAYER_KEYPAIR_BASE58",
                    fee_payer.pubkey(),
                    expected,
                );
            }
        }

        tracing::info!(pubkey = %fee_payer.pubkey(), "loaded fee payer");

        let mut curators: HashMap<Pubkey, Arc<Keypair>> = HashMap::new();
        for (i, b58) in cfg.curator_keypairs_base58.iter().enumerate() {
            let kp = Arc::new(load_keypair_from_base58(b58).with_context(|| {
                format!("CURATOR_KEYPAIRS_JSON[{i}]: invalid base58 keypair")
            })?);
            let pk = kp.pubkey();
            if curators.insert(pk, kp).is_some() {
                tracing::warn!(curator = %pk, "duplicate curator keypair in CURATOR_KEYPAIRS_JSON — kept the last entry");
            }
        }
        for pk in curators.keys() {
            tracing::info!(curator = %pk, "loaded curator keypair");
        }
        if cfg.handlers.curator_fee_claimer_enabled && curators.is_empty() {
            bail!(
                "CURATOR_FEE_CLAIMER_ENABLED=true but no curator keypairs in \
                 CURATOR_KEYPAIRS_JSON — refusing to start"
            );
        }

        Ok(Self {
            fee_payer,
            curators,
        })
    }
}

fn load_keypair(src: &KeypairSource) -> Result<Keypair> {
    match src {
        KeypairSource::Path(p) => load_keypair_from_file(p),
        KeypairSource::Base58(s) => load_keypair_from_base58(s),
    }
}

fn load_keypair_from_file(path: &Path) -> Result<Keypair> {
    // Refuse group/world-readable files — secret-leak guard.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .with_context(|| format!("stat keypair file {}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "keypair file {} is too permissive (mode {:#o}); chmod 0600 it before starting",
                path.display(),
                mode,
            );
        }
    }

    let bytes =
        std::fs::read(path).with_context(|| format!("reading keypair file {}", path.display()))?;
    let arr: Vec<u8> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing keypair JSON {}", path.display()))?;
    Keypair::try_from(&arr[..]).map_err(|e| anyhow!("invalid keypair at {}: {e}", path.display()))
}

/// Errors deliberately omit the input — it's a secret and an
/// invalid-base58 mishap would otherwise dump it to logs.
fn load_keypair_from_base58(s: &str) -> Result<Keypair> {
    let bytes = bs58::decode(s.trim())
        .into_vec()
        .map_err(|e| anyhow!("invalid base58 in keypair: {e}"))?;
    if bytes.len() != 64 {
        bail!(
            "base58 keypair must decode to 64 bytes, got {}",
            bytes.len()
        );
    }
    Keypair::try_from(&bytes[..]).map_err(|e| anyhow!("invalid keypair bytes: {e}"))
}
