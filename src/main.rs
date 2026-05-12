//! Entrypoint. Loads config, signers, RPC; verifies the chain endpoint
//! is reachable; spawns enabled handlers; awaits sigterm.

use std::sync::{atomic::AtomicBool, Arc};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use ydelta_crankers::chain_reader::ChainReader;
use ydelta_crankers::config::{redact_url, Config};
use ydelta_crankers::handlers::{
    spawn, ClaimerHandler, CuratorKeeperHandler, HandlerContext, LiquidatorHandler,
    PolicySyncHandler, PromoterHandler,
};
use ydelta_crankers::rpc::Rpc;
use ydelta_crankers::signer::Signers;
use ydelta_crankers::{health_server, metrics};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    install_panic_hook();
    tracing::info!("ydelta-crankers starting");

    let mut cfg = Config::from_env()?;
    // The RPC URL goes through `redact_url` so embedded API keys
    // (Helius / Triton / Quicknode all use `?api-key=…`) and any
    // basic-auth userinfo don't end up in Railway log streams.
    tracing::info!(
        network = cfg.network.as_str(),
        program_id = %cfg.program_id,
        marginfi_group = %cfg.marginfi_group,
        rpc = %redact_url(&cfg.rpc_url),
        "config loaded"
    );

    let stop = Arc::new(AtomicBool::new(false));

    let signers = Signers::load(&cfg)?;
    let rpc = Rpc::new(cfg.rpc_url.clone(), cfg.priority_fee_micro_lamports)
        .with_stop_signal(stop.clone());
    let chain = ChainReader::new(rpc.clone(), cfg.program_id);

    // Discover marginfi bank metadata at boot: walk every market the
    // chain reader surfaces, pull `(debt_bank, collateral_bank)` from
    // each `MarketFixed`, then resolve each unique bank's full
    // metadata from chain. No env input — the operator can't typo what
    // they don't enter.
    cfg.discover_banks_from_markets(&rpc, &chain)
        .await
        .context("discover_banks_from_markets failed")?;
    tracing::info!(
        banks = cfg.banks.len(),
        "discovered marginfi banks from on-chain markets",
    );

    // Bootstrap the fee-payer's SPL ATAs for every bank's mint. Uses
    // the idempotent create-ATA ix so this is safe to run on every
    // boot — already-existing accounts are no-ops. Operator still
    // needs to FUND the debt-side ATA before the liquidator can do
    // anything; we just create the empty account.
    cfg.banks
        .ensure_atas_for(&rpc, &signers.fee_payer)
        .await
        .context("ensure_atas_for failed")?;
    let cfg = Arc::new(cfg);

    // Install Prometheus exporter before any metric emission. Listener
    // binds on `METRICS_BIND` (default 127.0.0.1:9091 — local-only).
    // The metrics endpoint exposes per-handler ix-submission counters;
    // exposing those publicly leaks liquidation cadence as a live
    // trading signal. Bind publicly (e.g. 0.0.0.0:9091) ONLY behind a
    // private Railway network or an auth proxy.
    let metrics_bind: std::net::SocketAddr = std::env::var("METRICS_BIND")
        .unwrap_or_else(|_| "127.0.0.1:9091".to_string())
        .parse()?;
    metrics::install(metrics_bind)?;
    tracing::info!(addr = %metrics_bind, "prometheus exporter listening");

    // Health endpoints (/healthz, /readyz) on a separate bind from
    // metrics. `/healthz` is meant to be reachable from Railway's
    // health probe — bind 0.0.0.0:$HEALTH_PORT (default 8080) by
    // default. `/readyz` returns 503 until `ready` flips below.
    let health_bind: std::net::SocketAddr = std::env::var("HEALTH_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;
    let ready = Arc::new(AtomicBool::new(false));
    let _health_task = health_server::spawn(health_bind, ready.clone(), stop.clone());
    tracing::info!(addr = %health_bind, "health endpoints listening (/healthz, /readyz)");

    // Boot-time connectivity checks. Fail fast on misconfig.
    let slot = rpc.client().get_slot().await?;
    tracing::info!(slot, "rpc reachable");

    let stop_signal = stop.clone();
    tokio::spawn(async move {
        // Wait for either sigterm or sigint.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler installs");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("sigint handler installs");
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("SIGTERM received"),
            _ = sigint.recv() => tracing::info!("SIGINT received"),
        }
        stop_signal.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let fee_payer_low = Arc::new(AtomicBool::new(false));
    let ctx = HandlerContext {
        cfg: cfg.clone(),
        rpc: rpc.clone(),
        chain,
        signers: signers.clone(),
        stop: stop.clone(),
        fee_payer_low: fee_payer_low.clone(),
    };

    // Background gauge refresher for signer SOL balances. Also flips
    // `fee_payer_low` on each threshold crossing so the supervisor
    // can pause fee-payer handlers when the wallet drains.
    let _sol_balance_task = metrics::spawn_sol_balance_loop(
        rpc,
        signers,
        stop.clone(),
        fee_payer_low,
        cfg.min_signer_balance_lamports,
    );

    let mut handles = Vec::new();
    if cfg.handlers.policy_sync_enabled {
        handles.push(spawn(
            Arc::new(PolicySyncHandler::new()),
            ctx.clone(),
            cfg.handlers.policy_sync_interval,
        ));
    }
    if cfg.handlers.promoter_enabled {
        handles.push(spawn(
            Arc::new(PromoterHandler::new()),
            ctx.clone(),
            cfg.handlers.promoter_interval,
        ));
    }
    if cfg.handlers.claimer_enabled {
        handles.push(spawn(
            Arc::new(ClaimerHandler::new()),
            ctx.clone(),
            cfg.handlers.claimer_interval,
        ));
    }
    if cfg.handlers.liquidator_enabled {
        handles.push(spawn(
            Arc::new(LiquidatorHandler::new()),
            ctx.clone(),
            cfg.handlers.liquidator_interval,
        ));
    }
    if cfg.handlers.curator_keeper_enabled && !cfg.curator_signers.is_empty() {
        handles.push(spawn(
            Arc::new(CuratorKeeperHandler::new()),
            ctx.clone(),
            cfg.handlers.curator_keeper_interval,
        ));
    }

    tracing::info!(handlers_spawned = handles.len(), "bot running");

    // Bank discovery + ATA bootstrap done, handlers running → mark
    // /readyz as 200. Anything before this is still in cold-boot;
    // probes get a 503 and Railway / k8s won't route traffic yet.
    ready.store(true, std::sync::atomic::Ordering::Relaxed);

    for h in handles {
        let _ = h.await;
    }
    tracing::info!("ydelta-crankers stopped");
    Ok(())
}

/// Replace the default panic handler so a panic inside ANY tokio task
/// (handler tick, gauge loop, ATA bootstrap, etc.) logs payload + file
/// + line + a captured backtrace and then exits the process. Without
/// this, tokio swallows task panics into a `JoinError::is_panic()` that
/// nobody is awaiting, and the bot keeps running with one dead handler.
///
/// Railway / k8s see the non-zero exit and restart cleanly.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_string)
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(unparseable panic payload)".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "(unknown)".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            location = %location,
            panic_msg = %msg,
            backtrace = %backtrace,
            "ydelta-crankers panic — terminating",
        );
        // Flush stdout/stderr where possible before exit so the panic
        // log isn't lost to buffering.
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }));
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ydelta_crankers=info"));
    // Plain text for local; JSON layer kicks in if `LOG_FORMAT=json`.
    if std::env::var("LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
