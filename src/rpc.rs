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
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer as _},
    transaction::{Transaction, VersionedTransaction},
};

/// CU limit for the heavier v0 path (liquidator SWB-bundle + marginfi-CPI
/// settle/liquidate txs). Kept at Solana's per-tx ceiling so a valuable
/// liquidation can never be truncated — these txs are rare, so the larger
/// priority fee they imply is not a meaningful cost.
const V0_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

#[derive(Clone)]
pub struct Rpc {
    client: Arc<RpcClient>,
    priority_fee_micro_lamports: u64,
    /// CU limit requested on the routine (legacy-message) send path —
    /// promoter / claimer txs. The prioritization fee is charged as
    /// `compute_unit_price × compute_unit_limit` (the REQUESTED limit, not
    /// the units actually consumed), so an oversized limit silently
    /// overpays every tx. Sized to comfortably cover observed usage
    /// (~130–170k CU) with headroom; tune via `COMPUTE_UNIT_LIMIT`.
    compute_unit_limit: u32,
    /// When set, `send_with_retry` bails between attempts. We never
    /// cancel an in-flight `send_and_confirm` — the tx might land.
    stop: Option<Arc<AtomicBool>>,
}

impl Rpc {
    pub fn new(url: String, priority_fee_micro_lamports: u64, compute_unit_limit: u32) -> Self {
        let client = Arc::new(RpcClient::new_with_commitment(
            url,
            CommitmentConfig::confirmed(),
        ));
        Self {
            client,
            priority_fee_micro_lamports,
            compute_unit_limit,
            stop: None,
        }
    }

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

    /// Chunked + concurrent `getMultipleAccounts`. 100-account chunks
    /// (RPC limit), up to 16 chunks in flight.
    pub async fn batch_get_multiple_accounts(
        &self,
        addresses: &[Pubkey],
    ) -> Result<Vec<Option<solana_sdk::account::Account>>> {
        const CHUNK_SIZE: usize = 100;
        const MAX_CONCURRENCY: usize = 16;

        if addresses.is_empty() {
            return Ok(Vec::new());
        }

        let chunks: Vec<Vec<Pubkey>> = addresses.chunks(CHUNK_SIZE).map(|c| c.to_vec()).collect();
        let mut output: Vec<Option<solana_sdk::account::Account>> = vec![None; addresses.len()];

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

    pub async fn get_account_data(&self, pk: &Pubkey) -> Result<Option<Vec<u8>>> {
        match self.client.get_account(pk).await {
            Ok(acct) if !acct.data.is_empty() => Ok(Some(acct.data)),
            Ok(_) => Ok(None),
            Err(e) => {
                if format!("{e}").contains("AccountNotFound") {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

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

        let mut all_ixs = self.priority_fee_preamble(self.compute_unit_limit);
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
            if self.stopped() {
                return Err(anyhow!("shutdown signal received before send attempt"));
            }

            // If the prior attempt's signature actually landed, return
            // it instead of re-signing — re-submission would race the
            // on-chain idempotency guards.
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

    pub async fn simulate(
        &self,
        ixs: Vec<Instruction>,
        fee_payer: &Pubkey,
    ) -> Result<SimulationResult> {
        // Mirror the send path (send_signed_labeled): prepend the compute-budget
        // preamble so the sim runs under the SAME CU limit the real tx will.
        // Without it, a heavy ix like process_matched_loan (marginfi CPIs) blows
        // the default 200k CU in sim and fails ProgramFailedToComplete, making
        // the productive-crank gate skip a send that would have succeeded at the
        // configured limit. simulate_v0 stays bare — its Switchboard callers
        // build their own bundle and manage instruction indices.
        let mut all_ixs = self.priority_fee_preamble(self.compute_unit_limit);
        all_ixs.extend(ixs);
        self.simulate_v0(all_ixs, &[], fee_payer).await
    }

    /// v0-message simulate with optional Address Lookup Tables. Used by
    /// the liquidator to sim a Switchboard fetch ix (which carries LUTs
    /// to fit under the 1232-byte limit) bundled with the consuming
    /// liquidatable-check ix — simulation is free so it pays no SOL even
    /// though the SWB update would normally cost ~0.0018 SOL.
    pub async fn simulate_v0(
        &self,
        ixs: Vec<Instruction>,
        luts: &[AddressLookupTableAccount],
        fee_payer: &Pubkey,
    ) -> Result<SimulationResult> {
        let blockhash = self.client.get_latest_blockhash().await?;
        let msg = VersionedMessage::V0(v0::Message::try_compile(fee_payer, &ixs, luts, blockhash)?);
        // sig_verify:false + replace_recent_blockhash:true lets us simulate
        // an unsigned tx with a fresh blockhash; the RPC fills it in.
        let tx = VersionedTransaction {
            signatures: vec![Default::default(); msg.header().num_required_signatures as usize],
            message: msg,
        };
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
            logs: result.value.logs.unwrap_or_default(),
        })
    }

    /// v0-message submit with optional LUTs. Caller-supplied `ixs` are
    /// PREPENDED with the priority-fee preamble, same as `send_signed_labeled`,
    /// so the metering is consistent regardless of whether the tx uses LUTs.
    pub async fn send_signed_v0_labeled(
        &self,
        ix_label: &'static str,
        ixs: Vec<Instruction>,
        luts: &[AddressLookupTableAccount],
        signers: &[&Keypair],
    ) -> Result<Signature> {
        if signers.is_empty() {
            return Err(anyhow!("send_signed_v0: no signers"));
        }
        let fee_payer = signers[0];

        let mut all_ixs = self.priority_fee_preamble(V0_COMPUTE_UNIT_LIMIT);
        all_ixs.extend(ixs);

        let t0 = std::time::Instant::now();
        let result = self.send_v0_with_retry(&all_ixs, luts, signers, fee_payer).await;

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

    /// v0 sibling of `send_with_retry`. Same retry policy (3 attempts,
    /// idempotency check via signature status) but compiles a v0 message
    /// with LUTs.
    async fn send_v0_with_retry(
        &self,
        all_ixs: &[Instruction],
        luts: &[AddressLookupTableAccount],
        signers: &[&Keypair],
        fee_payer: &Keypair,
    ) -> Result<Signature> {
        let mut last_sig: Option<Signature> = None;
        for attempt in 0..3 {
            if self.stopped() {
                return Err(anyhow!("shutdown signal received before send attempt"));
            }
            if let Some(prev) = last_sig {
                match self.client.get_signature_statuses(&[prev]).await {
                    Ok(resp) => {
                        if let Some(Some(_status)) = resp.value.first() {
                            tracing::info!(sig = %prev, "previous tx landed; treating retry-trigger as success");
                            return Ok(prev);
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "could not check prev sig status; proceeding to re-sign"),
                }
            }
            let blockhash = self.client.get_latest_blockhash().await.context("get_latest_blockhash")?;
            let msg = VersionedMessage::V0(
                v0::Message::try_compile(&fee_payer.pubkey(), all_ixs, luts, blockhash)
                    .context("v0::Message::try_compile")?,
            );
            let tx = VersionedTransaction::try_new(msg, signers).context("VersionedTransaction::try_new")?;
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

    /// Build the compute-budget preamble. NOTE: the prioritization fee is
    /// `compute_unit_price × compute_unit_limit` using the REQUESTED limit,
    /// not the units actually consumed — so `cu_limit` is a direct cost
    /// lever, not just a truncation guard. Callers pass a limit sized to the
    /// path: the routine `compute_unit_limit` for promoter/claimer txs, the
    /// larger `V0_COMPUTE_UNIT_LIMIT` for the liquidator's CPI-heavy bundles.
    fn priority_fee_preamble(&self, cu_limit: u32) -> Vec<Instruction> {
        let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(cu_limit)];
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
    /// Program logs from the sandbox run, including `sol_log_data`
    /// (`Program data: <base64>`) lines. Lets a caller gate on the
    /// *productive* signal a tx emits (e.g. `MatchCrankLog.fills`), not
    /// just whether it reverted — a 0-work tx that still succeeds would
    /// otherwise pass `ok` and land for nothing.
    pub logs: Vec<String>,
}

/// Classify safe-to-retry vs definitive errors. We match on the
/// structured `ClientErrorKind` (with a narrow message match for the
/// blockhash case) so a program error whose `Display` happens to
/// contain "connection" or "timed out" can't accidentally trigger a
/// retry.
fn is_transient(e: &solana_client::client_error::ClientError) -> bool {
    match e.kind() {
        ClientErrorKind::Io(_) | ClientErrorKind::Reqwest(_) => true,
        ClientErrorKind::RpcError(rpc_err) => match rpc_err {
            RpcError::ForUser(msg) => msg.contains("Blockhash not found"),
            RpcError::RpcResponseError { code, .. } => (-32099..=-32000).contains(code),
            _ => false,
        },
        _ => false,
    }
}
