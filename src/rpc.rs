//! Thin RPC client wrapper. Handles tx building, signing, priority-fee
//! preamble, and a small retry on transient errors.
//!
//! We deliberately keep this lean — no transaction simulation helpers
//! beyond what's needed for `CheckLtvLiquidatable` / `CheckMaturityLiquidatable`.

use std::{
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use solana_client::{
    client_error::ClientErrorKind, nonblocking::rpc_client::RpcClient,
    rpc_config::RpcSimulateTransactionConfig, rpc_request::RpcError,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer as _},
    transaction::Transaction,
};

#[derive(Clone)]
pub struct Rpc {
    client: Arc<RpcClient>,
    priority_fee_micro_lamports: u64,
    /// Shared shutdown flag. When set, `send_with_retry` won't start
    /// any new attempt — better to drop an in-flight retry than to
    /// queue a new tx the supervisor is about to abort. Optional so
    /// tests / one-off helpers can construct an `Rpc` without one.
    stop: Option<Arc<AtomicBool>>,
}

impl Rpc {
    pub fn new(url: String, priority_fee_micro_lamports: u64) -> Self {
        let client = Arc::new(RpcClient::new_with_commitment(
            url,
            CommitmentConfig::confirmed(),
        ));
        Self {
            client,
            priority_fee_micro_lamports,
            stop: None,
        }
    }

    /// Attach a shutdown flag so `send_with_retry` bails between
    /// attempts when SIGTERM has fired. The flag is read with
    /// `Ordering::Relaxed` — we only care that it's eventually
    /// observed, not that it's synchronized with any other write.
    pub fn with_stop_signal(mut self, stop: Arc<AtomicBool>) -> Self {
        self.stop = Some(stop);
        self
    }

    fn stopped(&self) -> bool {
        self.stop
            .as_ref()
            .map(|s| s.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn client(&self) -> Arc<RpcClient> {
        self.client.clone()
    }

    /// Chunked + concurrent `getMultipleAccounts`. Splits `addresses`
    /// into 100-account chunks (Solana RPC limit) and fetches up to 16
    /// chunks concurrently via `tokio::task::JoinSet`. Order of the
    /// returned `Vec` matches `addresses`.
    ///
    /// Replaces N round-trip `get_account_data` loops in handler ticks.
    /// One RPC call per 100 accounts beats N calls of 1 every time.
    pub async fn batch_get_multiple_accounts(
        &self,
        addresses: &[Pubkey],
    ) -> Result<Vec<Option<solana_sdk::account::Account>>> {
        const CHUNK_SIZE: usize = 100;
        const MAX_CONCURRENCY: usize = 16;

        if addresses.is_empty() {
            return Ok(Vec::new());
        }

        // Pre-allocate so per-task results land in deterministic
        // positions in the output, independent of completion order.
        let chunks: Vec<Vec<Pubkey>> = addresses.chunks(CHUNK_SIZE).map(|c| c.to_vec()).collect();
        let mut output: Vec<Option<solana_sdk::account::Account>> = vec![None; addresses.len()];

        // Limit concurrency with a semaphore so we never have more than
        // MAX_CONCURRENCY in-flight RPC requests; otherwise a huge
        // discovery would queue 100+ concurrent requests at boot.
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENCY));
        let mut join_set: tokio::task::JoinSet<
            Result<(usize, Vec<Option<solana_sdk::account::Account>>)>,
        > = tokio::task::JoinSet::new();

        for (idx, chunk) in chunks.into_iter().enumerate() {
            let client = self.client.clone();
            let permit = sem.clone();
            join_set.spawn(async move {
                let _g = permit
                    .acquire_owned()
                    .await
                    .map_err(|e| anyhow!("semaphore closed: {e}"))?;
                let res = client
                    .get_multiple_accounts(&chunk)
                    .await
                    .with_context(|| format!("batch_get_multiple_accounts chunk {idx}"))?;
                Ok((idx, res))
            });
        }

        while let Some(joined) = join_set.join_next().await {
            let (idx, accounts) =
                joined.map_err(|e| anyhow!("batch_get_multiple_accounts task panicked: {e}"))??;
            let base = idx * CHUNK_SIZE;
            for (j, acct) in accounts.into_iter().enumerate() {
                output[base + j] = acct;
            }
        }
        Ok(output)
    }

    /// Fetch raw account data. Returns `None` if missing or zero-data.
    pub async fn get_account_data(&self, pk: &Pubkey) -> Result<Option<Vec<u8>>> {
        match self.client.get_account(pk).await {
            Ok(acct) if !acct.data.is_empty() => Ok(Some(acct.data)),
            Ok(_) => Ok(None),
            Err(e) => {
                // Treat "not found" as None; surface everything else.
                if format!("{e}").contains("AccountNotFound") {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Build, sign, send, and confirm a tx with a priority-fee preamble.
    /// `signers[0]` must be the fee payer. `ix_label` is a stable string
    /// used as a metrics label (e.g. `"process_matched_loan"`).
    pub async fn send_signed(
        &self,
        ixs: Vec<Instruction>,
        signers: &[&Keypair],
    ) -> Result<Signature> {
        self.send_signed_labeled("untagged", ixs, signers).await
    }

    pub async fn send_signed_labeled(
        &self,
        ix_label: &'static str,
        ixs: Vec<Instruction>,
        signers: &[&Keypair],
    ) -> Result<Signature> {
        if signers.is_empty() {
            return Err(anyhow!("send_signed: no signers"));
        }
        let fee_payer = signers[0];

        let mut all_ixs = self.priority_fee_preamble();
        all_ixs.extend(ixs);

        let t0 = std::time::Instant::now();
        let result = self.send_with_retry(&all_ixs, signers, fee_payer).await;

        let elapsed = t0.elapsed();
        let outcome = if result.is_ok() { "ok" } else { "err" };
        metrics::counter!(
            crate::metrics::M_IXS_SUBMITTED,
            "ix" => ix_label,
            "outcome" => outcome,
        )
        .increment(1);
        metrics::histogram!(
            crate::metrics::M_IX_LATENCY,
            "ix" => ix_label,
            "outcome" => outcome,
        )
        .record(elapsed.as_secs_f64());

        result
    }

    async fn send_with_retry(
        &self,
        all_ixs: &[Instruction],
        signers: &[&Keypair],
        fee_payer: &Keypair,
    ) -> Result<Signature> {
        let mut last_sig: Option<Signature> = None;

        for attempt in 0..3 {
            // Bail between attempts if SIGTERM has fired. We deliberately
            // don't try to cancel an in-flight `send_and_confirm` — that
            // tx might land on-chain; better to let it finish and pay
            // its fee than to leak a tx whose status we don't know.
            if self.stopped() {
                return Err(anyhow!("shutdown signal received before send attempt"));
            }

            // Before re-signing, check whether the previous attempt's
            // signature actually landed. If it did but `send_and_confirm`
            // returned an error (lost confirmation, RPC blip, etc.), we
            // must not submit a second distinct signature for the same
            // logical action — that's a double-pay of priority fees and,
            // worse, can race the on-chain idempotency guards.
            if let Some(prev) = last_sig {
                match self.client.get_signature_statuses(&[prev]).await {
                    Ok(resp) => {
                        if let Some(Some(_status)) = resp.value.first() {
                            tracing::info!(
                                sig = %prev,
                                "previous tx landed; treating retry-trigger as success"
                            );
                            return Ok(prev);
                        }
                    }
                    Err(e) => {
                        // If we can't even check status, skip the
                        // precheck rather than bail — preserve the
                        // retry path for genuinely transient cases.
                        tracing::warn!(error = %e, "could not check prev sig status; proceeding to re-sign");
                    }
                }
            }

            let blockhash = self
                .client
                .get_latest_blockhash()
                .await
                .context("get_latest_blockhash")?;
            let tx = Transaction::new_signed_with_payer(
                all_ixs,
                Some(&fee_payer.pubkey()),
                signers,
                blockhash,
            );
            let sig = *tx
                .signatures
                .first()
                .ok_or_else(|| anyhow!("tx has no signature"))?;

            match self.client.send_and_confirm_transaction(&tx).await {
                Ok(sig) => return Ok(sig),
                Err(e) => {
                    last_sig = Some(sig);
                    let transient = is_transient(&e);
                    if !transient || attempt == 2 {
                        return Err(anyhow!("send_and_confirm_transaction: {e}"));
                    }
                    tracing::warn!(attempt, error = %e, "transient send error, retrying");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        unreachable!()
    }

    /// Simulate a tx; return `Ok(())` if the program returned Ok,
    /// `Err(...)` otherwise with the program error captured.
    pub async fn simulate(
        &self,
        ixs: Vec<Instruction>,
        fee_payer: &Pubkey,
    ) -> Result<SimulationResult> {
        let blockhash = self.client.get_latest_blockhash().await?;
        let tx = Transaction::new_unsigned(solana_sdk::message::Message::new_with_blockhash(
            &ixs,
            Some(fee_payer),
            &blockhash,
        ));
        // sig_verify=false lets us simulate without a real signer (only
        // the fee_payer key is referenced; no signing keypair needed).
        let result = self
            .client
            .simulate_transaction_with_config(
                &tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(SimulationResult {
            ok: result.value.err.is_none(),
            error: result.value.err.map(|e| format!("{e:?}")),
        })
    }

    /// Compute-budget preamble for every tx the bot sends.
    ///
    /// Sets both the per-CU price (priority fee) and an explicit CU
    /// limit. The runtime defaults to a per-ix 200k allocation which
    /// can both over-reserve fees and undersize a marginfi-CPI-heavy
    /// liquidate tx. 1.4M is Solana's per-tx ceiling and is a *cap*,
    /// not a charge — you only pay for CU actually consumed, so this
    /// is free protection against tx failures that look like a "pay
    /// more priority fee" problem when they're actually CU starvation.
    fn priority_fee_preamble(&self) -> Vec<Instruction> {
        let mut ixs = vec![
            // Always emit the CU-limit ix, even when priority_fee is 0,
            // so handler txs always have headroom for marginfi CPI +
            // oracle reads.
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
        ];
        if self.priority_fee_micro_lamports > 0 {
            ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
                self.priority_fee_micro_lamports,
            ));
        }
        ixs
    }
}

#[derive(Debug)]
pub struct SimulationResult {
    pub ok: bool,
    pub error: Option<String>,
}

/// Classify a `solana-client` error as "safe to retry" vs "definitive".
/// We match on the structured `ClientErrorKind` (and a narrow message
/// substring for the blockhash case) so a program-level error whose
/// `Display` happens to contain "connection" / "timed out" never
/// triggers a retry.
fn is_transient(e: &solana_client::client_error::ClientError) -> bool {
    match e.kind() {
        // Network-level errors: io & reqwest. Always safe to retry.
        ClientErrorKind::Io(_) | ClientErrorKind::Reqwest(_) => true,
        // RPC server errors in the JSON-RPC layer.
        ClientErrorKind::RpcError(rpc_err) => match rpc_err {
            // Stale blockhash is the canonical "leader rejected, try
            // again" case. The textual match here is narrow enough
            // that program errors can't accidentally trigger it
            // (the message comes from the server itself).
            RpcError::ForUser(msg) => msg.contains("Blockhash not found"),
            // Server-side internal errors. Code range -32000..-32099
            // is the JSON-RPC reserved server-error band.
            RpcError::RpcResponseError { code, .. } => (-32099..=-32000).contains(code),
            _ => false,
        },
        // Anything else (TransactionError, SigningError, SerdeJson,
        // Custom) is a definitive failure — retrying won't help and
        // may make things worse.
        _ => false,
    }
}
