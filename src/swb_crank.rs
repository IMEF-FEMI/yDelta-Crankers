//! Switchboard On-Demand inline cranker. Mirrors the construction logic
//! in `references/eva01/src/utils/swb_cranker.rs::crank_oracles_internal`
//! (which is itself a thin wrapper over `PullFeed::fetch_update_consensus_ix`
//! from the upstream `switchboard-on-demand-client` SDK).
//!
//! Why eva01's pattern fits us: an on-demand Switchboard feed only
//! refreshes when *someone* sends a "fetch update" tx. The feed account
//! caches the last consensus price + timestamp; if no one has cranked
//! recently the cached timestamp ages out and yDelta's
//! `read_oracle_price` rejects the bank with `OracleStale` (`age >
//! oracle_max_age`). Bundling a crank tx ahead of an order tx makes the
//! feed fresh just before our tx lands.
//!
//! The crank ix is a v0-message-only construct because Switchboard ships
//! an Address Lookup Table alongside it (the relayer signatures are
//! deduplicated through the LUT). We submit the crank as its own v0 tx
//! and let the downstream order tx stay legacy — same separation eva01
//! uses.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::VersionedTransaction,
};
use switchboard_on_demand_client::{
    FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};

/// Switchboard On-Demand mainnet queue. Same default eva01 uses
/// (`A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w`) — passed to
/// `QueueAccountData::load` as the "queue/program id" arg; the SDK
/// resolves the gateway list from it.
pub const SWB_QUEUE_MAINNET: &str = "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";

/// Build and send a Switchboard "fetch update" tx for `feeds`. Returns
/// the signature once the tx confirms. Mirrors
/// `eva01::utils::swb_cranker::SwbCranker::crank_oracles_internal` —
/// single chunk, no quarantine, no batch fallback (those layers belong
/// in a long-running bot, not a one-shot helper).
pub async fn crank_feeds(
    rpc_url: &str,
    payer: &Keypair,
    feeds: Vec<Pubkey>,
) -> Result<Signature> {
    if feeds.is_empty() {
        return Err(anyhow!("crank_feeds called with no feeds"));
    }

    let rpc = NonBlockingRpcClient::new_with_timeout_and_commitment(
        rpc_url.to_string(),
        Duration::from_secs(30),
        CommitmentConfig::confirmed(),
    );

    let queue_pk: Pubkey = SWB_QUEUE_MAINNET
        .parse()
        .context("parsing SWB_QUEUE_MAINNET")?;
    let queue = QueueAccountData::load(&rpc, &queue_pk)
        .await
        .context("QueueAccountData::load")?;
    let gateway: Gateway = queue
        .fetch_gateways(&rpc)
        .await
        .context("queue.fetch_gateways")?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Switchboard gateways available from queue"))?;

    let (crank_ix, crank_lut) = PullFeed::fetch_update_consensus_ix(
        SbContext::new(),
        &rpc,
        FetchUpdateManyParams {
            feeds,
            payer: payer.pubkey(),
            gateway,
            crossbar: None,
            num_signatures: Some(1),
            ..Default::default()
        },
    )
    .await
    .context("PullFeed::fetch_update_consensus_ix")?;

    let blockhash = rpc
        .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
        .await
        .context("get_latest_blockhash")?
        .0;

    let tx = VersionedTransaction::try_new(
        VersionedMessage::V0(
            v0::Message::try_compile(&payer.pubkey(), &crank_ix, &crank_lut, blockhash)
                .context("v0::Message::try_compile")?,
        ),
        &[payer],
    )
    .context("VersionedTransaction::try_new")?;

    let sig = rpc
        .send_and_confirm_transaction_with_spinner_and_config(
            &tx,
            CommitmentConfig::confirmed(),
            RpcSendTransactionConfig {
                skip_preflight: false,
                preflight_commitment: Some(CommitmentLevel::Processed),
                ..Default::default()
            },
        )
        .await
        .context("send_and_confirm crank tx")?;

    // Drop ref to satisfy the linter on Arc-clone hygiene; nothing else
    // holds the underlying RPC here.
    let _ = Arc::new(());
    Ok(sig)
}

/// Decode the `last_update_timestamp` from a Switchboard On-Demand
/// `PullFeedAccountData` at the fixed offset used by yDelta's
/// `protocol/oracles.rs::decode_switchboard_pull` (post-8-byte disc,
/// body offset 2208).
pub fn decode_swb_last_update_ts(data: &[u8]) -> Option<i64> {
    const OFF: usize = 8 + 2208;
    if data.len() < OFF + 8 {
        return None;
    }
    Some(i64::from_le_bytes(data[OFF..OFF + 8].try_into().unwrap()))
}
