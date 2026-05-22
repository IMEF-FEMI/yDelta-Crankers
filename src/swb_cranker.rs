//! Switchboard On-Demand pull-feed cranker.
//!
//! Switchboard On-Demand feeds are pull-based — nobody posts them on-chain
//! unless asked. yDelta's liquidator/settle paths read the collateral bank's
//! Switchboard oracle, and the on-chain `read_oracle_price` staleness gate
//! rejects a feed older than `bank.oracle_max_age`. So before those ixs we
//! post a fresh update in a SEPARATE tx (lands a slot or two ahead, well
//! inside the freshness window). Mirrors `references/eva01`'s `SwbCranker`,
//! using the first on-chain gateway (no crossbar).

use std::sync::Arc;

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    commitment_config::CommitmentConfig,
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
    gateway: Gateway,
    payer: Arc<Keypair>,
}

impl SwbCranker {
    /// Load the Switchboard queue + the first on-chain gateway at boot.
    /// `swb_queue` is the Switchboard On-Demand QUEUE account pubkey (e.g.
    /// mainnet `A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w`) — NOT the
    /// program id (loading the program account parses as a queue and fails
    /// with `SizeMismatch`).
    pub async fn new(rpc_url: String, swb_queue: Pubkey, payer: Arc<Keypair>) -> Result<Self> {
        let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        let queue = QueueAccountData::load(&rpc, &swb_queue)
            .await
            .map_err(|e| anyhow!("swb QueueAccountData::load({swb_queue}): {e}"))?;
        let gateway = queue
            .fetch_gateways(&rpc)
            .await
            .map_err(|e| anyhow!("swb fetch_gateways: {e}"))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no on-chain switchboard gateways"))?;

        Ok(Self {
            rpc,
            gateway,
            payer,
        })
    }

    /// Post a fresh consensus update for `oracles` in one tx, signed by the
    /// fee payer. No-op on an empty slice.
    pub async fn crank(&self, oracles: Vec<Pubkey>) -> Result<Option<Signature>> {
        if oracles.is_empty() {
            return Ok(None);
        }
        let (ixs, luts) = PullFeed::fetch_update_consensus_ix(
            SbContext::new(),
            &self.rpc,
            FetchUpdateManyParams {
                feeds: oracles,
                payer: self.payer.pubkey(),
                gateway: self.gateway.clone(),
                crossbar: None,
                num_signatures: Some(1),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow!("swb fetch_update_consensus_ix: {e}"))?;

        let blockhash = self.rpc.get_latest_blockhash().await?;
        let msg = VersionedMessage::V0(v0::Message::try_compile(
            &self.payer.pubkey(),
            &ixs,
            &luts,
            blockhash,
        )?);
        let tx = VersionedTransaction::try_new(msg, &[self.payer.as_ref()])?;
        let sig = self.rpc.send_and_confirm_transaction(&tx).await?;
        Ok(Some(sig))
    }
}
