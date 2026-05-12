//! Keypair loading. All signers come from on-disk Solana CLI keypair
//! JSON files (the canonical 64-byte JSON array format).
//!
//! Paths are referenced from env vars (`FEE_PAYER_KEYPAIR`,
//! per-curator entries in `CURATORS`). The actual key bytes live
//! outside the repo — Railway secret files / k8s secrets / tmpfs.

use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{anyhow, bail, Context, Result};
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer as _};

use crate::config::{Config, CuratorSignerConfig, KeypairSource};

#[derive(Clone)]
pub struct Signers {
    pub fee_payer: Arc<Keypair>,
    /// `(global_vault, profile_id) → curator keypair`.
    pub curators: HashMap<(Pubkey, u8), Arc<Keypair>>,
}

impl Signers {
    pub fn load(cfg: &Config) -> Result<Self> {
        let fee_payer = Arc::new(load_keypair(&cfg.fee_payer_keypair)?);

        // If the operator pinned an expected fee-payer pubkey, assert
        // the loaded key matches. Catches the misconfig where the
        // configured source resolves to a curator key (which loads
        // fine but signs as the wrong wallet, draining the curator's
        // SOL on every priority-fee preamble).
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

        tracing::info!(
            pubkey = %fee_payer.pubkey(),
            "loaded fee payer"
        );

        let mut curators = HashMap::new();
        for c in &cfg.curator_signers {
            let kp = Arc::new(load_keypair(&c.keypair)?);
            tracing::info!(
                vault = %c.global_vault,
                profile_id = c.profile_id,
                curator_pubkey = %kp.pubkey(),
                "loaded curator"
            );
            curators.insert((c.global_vault, c.profile_id), kp);
        }

        Ok(Self {
            fee_payer,
            curators,
        })
    }

    pub fn curator_for(&self, c: &CuratorSignerConfig) -> Result<Arc<Keypair>> {
        self.curators
            .get(&(c.global_vault, c.profile_id))
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no loaded curator keypair for (vault={}, profile_id={})",
                    c.global_vault,
                    c.profile_id
                )
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
    // Refuse to load if the file is group- or world-readable. On a
    // shared host (or even a misconfigured container with a leaky
    // /secrets mount) this is the difference between "secret on disk"
    // and "secret anyone with shell can `cat`". Unix-only; on
    // non-Unix targets (Railway is Linux, so this always runs) we
    // skip the check.
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
    // Solana CLI format: JSON array of 64 u8s. `read_keypair` from
    // solana-sdk handles this directly, but it wants a Read impl —
    // simpler to parse the JSON ourselves and feed bytes to `from_bytes`.
    let arr: Vec<u8> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing keypair JSON {}", path.display()))?;
    Keypair::try_from(&arr[..]).map_err(|e| anyhow!("invalid keypair at {}: {e}", path.display()))
}

/// Decode an inline base58-encoded keypair secret. The string is the
/// full 64-byte secret encoded as base58 (NOT the Solana CLI JSON
/// array). Error messages deliberately do NOT include the input —
/// it's a secret, and an invalid-base58 mishap would otherwise dump
/// the secret to logs.
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
