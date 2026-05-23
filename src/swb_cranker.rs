//! Switchboard On-Demand pull-feed cranker.
//!
//! Switchboard On-Demand feeds are pull-based — nobody posts them on-chain
//! unless asked. yDelta's liquidator/settle paths read the collateral bank's
//! Switchboard oracle, and the on-chain `read_oracle_price` staleness gate
//! rejects a feed older than `bank.oracle_max_age`. So before those ixs we
//! post a fresh update in a SEPARATE tx (lands a slot or two ahead, well
//! inside the freshness window). Mirrors `references/eva01`'s `SwbCranker`,
//! using the first on-chain gateway (no crossbar).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::VersionedTransaction,
};
use switchboard_on_demand_client::{
    FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};

pub struct SwbCranker {
    rpc: RpcClient,
    /// ALL on-chain gateways for the queue, tried in order with failover —
    /// one throttled/dead gateway must not stall cranking.
    gateways: Vec<Gateway>,
    payer: Arc<Keypair>,
}

impl SwbCranker {
    /// Load the Switchboard queue + ALL on-chain gateways at boot.
    /// `swb_queue` is the Switchboard On-Demand QUEUE account pubkey (e.g.
    /// mainnet `A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w`) — NOT the
    /// program id (loading the program account parses as a queue and fails
    /// with `SizeMismatch`).
    pub async fn new(rpc_url: String, swb_queue: Pubkey, payer: Arc<Keypair>) -> Result<Self> {
        let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        let queue = QueueAccountData::load(&rpc, &swb_queue)
            .await
            .map_err(|e| anyhow!("swb QueueAccountData::load({swb_queue}): {e}"))?;
        let gateways = queue
            .fetch_gateways(&rpc)
            .await
            .map_err(|e| anyhow!("swb fetch_gateways: {e}"))?;
        if gateways.is_empty() {
            return Err(anyhow!("no on-chain switchboard gateways"));
        }
        tracing::info!(gateways = gateways.len(), "swb cranker loaded gateways");

        Ok(Self {
            rpc,
            gateways,
            payer,
        })
    }

    /// Post a fresh consensus update for `oracles` in one tx, signed by the
    /// fee payer. No-op on an empty slice. Tries each gateway in turn so a
    /// single throttled/stale gateway (HTTP 500 / rate-limit) doesn't fail
    /// the crank — mirrors the UI's `/api/oracle-update` failover.
    pub async fn crank(&self, oracles: Vec<Pubkey>) -> Result<Option<Signature>> {
        if oracles.is_empty() {
            return Ok(None);
        }
        let mut last_err: Option<anyhow::Error> = None;
        for (i, gateway) in self.gateways.iter().enumerate() {
            match self.try_crank(&oracles, gateway).await {
                Ok(sig) => return Ok(Some(sig)),
                Err(e) => {
                    tracing::debug!(gateway_idx = i, error = %e, "swb gateway failed; trying next");
                    last_err = Some(e);
                }
            }
        }
        Err(anyhow!(
            "all {} switchboard gateways failed: {}",
            self.gateways.len(),
            last_err.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

    /// One crank attempt against a single gateway.
    async fn try_crank(&self, oracles: &[Pubkey], gateway: &Gateway) -> Result<Signature> {
        let (ixs, luts) = self.fetch_update_via(oracles, gateway).await?;
        let blockhash = self.rpc.get_latest_blockhash().await?;
        let msg = VersionedMessage::V0(v0::Message::try_compile(
            &self.payer.pubkey(),
            &ixs,
            &luts,
            blockhash,
        )?);
        let tx = VersionedTransaction::try_new(msg, &[self.payer.as_ref()])?;
        let sig = self.rpc.send_and_confirm_transaction(&tx).await?;
        Ok(sig)
    }

    /// Build (without submitting) the fetch-update ix bundle for `oracles`.
    /// Used by callers (the liquidator) that want to BUNDLE the SWB update
    /// with their consuming ix into a single sim/tx — sim pays no SOL, and
    /// the real submission only happens when the consuming ix's gate passes.
    /// Tries each gateway in turn so a flaky one doesn't kill the fetch.
    pub async fn fetch_update_ixs(
        &self,
        oracles: Vec<Pubkey>,
    ) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        if oracles.is_empty() {
            return Ok((vec![], vec![]));
        }
        let mut last_err: Option<anyhow::Error> = None;
        for (i, gateway) in self.gateways.iter().enumerate() {
            match self.fetch_update_via(&oracles, gateway).await {
                Ok(bundle) => return Ok(bundle),
                Err(e) => {
                    tracing::debug!(gateway_idx = i, error = %e, "swb gateway failed; trying next");
                    last_err = Some(e);
                }
            }
        }
        Err(anyhow!(
            "all {} switchboard gateways failed: {}",
            self.gateways.len(),
            last_err.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

    /// Pure HTTP gateway call — no on-chain side effect, no SOL cost. The
    /// returned ixs include a Secp256k1 verify + the on-demand update ix;
    /// LUTs compress the account list to fit the 1232-byte tx limit.
    async fn fetch_update_via(
        &self,
        oracles: &[Pubkey],
        gateway: &Gateway,
    ) -> Result<(Vec<Instruction>, Vec<AddressLookupTableAccount>)> {
        PullFeed::fetch_update_consensus_ix(
            SbContext::new(),
            &self.rpc,
            FetchUpdateManyParams {
                feeds: oracles.to_vec(),
                payer: self.payer.pubkey(),
                gateway: gateway.clone(),
                crossbar: None,
                num_signatures: Some(1),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow!("swb fetch_update_consensus_ix: {e}"))
    }
}

/// Periodically crank every SwitchboardPull collateral oracle so the
/// on-chain feed stays inside marginfi's `oracle_max_age`. Pull feeds are
/// only posted when someone cranks them; this keeps them warm for ALL
/// readers — borrow / withdraw / liquidation, and the UI (which no longer
/// self-cranks) — the same way other protocols on the bank do. Uses the
/// gateway path (no crossbar). Re-reads the bank snapshot each tick so a
/// newly-added Switchboard market is picked up automatically.
pub fn spawn_swb_crank_loop(
    cranker: Arc<SwbCranker>,
    cfg: Arc<crate::config::Config>,
    stop: Arc<AtomicBool>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let oracles = cfg.banks_snapshot().switchboard_pull_oracles();
            if !oracles.is_empty() {
                match cranker.crank(oracles).await {
                    Ok(Some(sig)) => tracing::debug!(%sig, "swb periodic crank posted"),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "swb periodic crank failed; feed may go stale")
                    }
                }
            }
            // Stop-aware sleep so shutdown isn't blocked for a full interval.
            let mut elapsed = Duration::ZERO;
            let step = Duration::from_millis(500);
            while elapsed < interval {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(step).await;
                elapsed += step;
            }
        }
    })
}
