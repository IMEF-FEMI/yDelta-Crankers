use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc, RwLock},
};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use ydelta_crankers::chain_reader::ChainReader;
use ydelta_crankers::config::{redact_url, Config};
use ydelta_crankers::handlers::{
    spawn, ClaimerHandler, CuratorFeeClaimerHandler, HandlerContext, LiquidatorHandler,
    MatchCrankerHandler, PromoterHandler,
};
use ydelta_crankers::rpc::Rpc;
use ydelta_crankers::signer::Signers;
use ydelta_crankers::{health_server, metrics, swb_cranker};

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    install_panic_hook();
    tracing::info!("ydelta-crankers starting");

    let cfg = Config::from_env()?;
    tracing::info!(
        network = cfg.network.as_str(),
        program_id = %cfg.program_id,
        marginfi_group = %cfg.marginfi_group,
        rpc = %redact_url(&cfg.rpc_url),
        "config loaded"
    );

    let stop = Arc::new(AtomicBool::new(false));

    let signers = Signers::load(&cfg)?;
    let rpc = Rpc::new(
        cfg.rpc_url.clone(),
        cfg.priority_fee_micro_lamports,
        cfg.compute_unit_limit,
    )
    .with_stop_signal(stop.clone());
    let chain = ChainReader::new(rpc.clone(), cfg.program_id);

    cfg.discover_banks_from_markets(&rpc, &chain)
        .await
        .context("discover_banks_from_markets failed")?;
    tracing::info!(
        banks = cfg.banks_snapshot().len(),
        "discovered marginfi banks from on-chain markets",
    );

    // Bootstrap empty ATAs for every known mint. Operator still has to
    // FUND the debt-side ATAs before the liquidator can settle.
    cfg.banks_snapshot()
        .ensure_atas_for(&rpc, &signers.fee_payer)
        .await
        .context("ensure_atas_for failed")?;
    for (curator_pk, keypair) in signers.curators.iter() {
        if let Err(e) = cfg
            .banks_snapshot()
            .ensure_atas_for(&rpc, keypair.as_ref())
            .await
        {
            tracing::warn!(curator = %curator_pk, error = %e, "ensure_atas_for(curator) failed");
        }
    }
    let cfg = Arc::new(cfg);

    // Bind privately by default — the per-handler ix counters leak
    // liquidation cadence as a trading signal. Expose publicly only
    // behind a private network or auth proxy.
    let metrics_bind: std::net::SocketAddr = std::env::var("METRICS_BIND")
        .unwrap_or_else(|_| "127.0.0.1:9091".to_string())
        .parse()?;
    metrics::install(metrics_bind)?;
    tracing::info!(addr = %metrics_bind, "prometheus exporter listening");

    let health_bind: std::net::SocketAddr = std::env::var("HEALTH_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;
    let ready = Arc::new(AtomicBool::new(false));
    let _health_task = health_server::spawn(health_bind, ready.clone(), stop.clone());
    tracing::info!(addr = %health_bind, "health endpoints listening (/healthz, /readyz)");

    let slot = rpc.client().get_slot().await?;
    tracing::info!(slot, "rpc reachable");

    let stop_signal = stop.clone();
    tokio::spawn(async move {
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

    // Switchboard pull-feed cranker — booted whenever a Switchboard-collateral
    // market exists (it loads the swb queue + gateway from chain at boot).
    // Used BOTH by the periodic crank loop below (keeps the feed fresh for
    // borrow/withdraw/UI) AND the liquidator's pre-crank — so it's NOT gated
    // on the liquidator being enabled. A boot failure is non-fatal: callers
    // skip and log.
    let swb_cranker = if cfg.banks_snapshot().has_switchboard_pull() {
        // Switchboard On-Demand QUEUE account (mainnet default; override with
        // SWITCHBOARD_QUEUE for devnet/custom). NOT the program id — loading
        // the program account as a queue fails with SizeMismatch.
        let swb_queue = std::env::var("SWITCHBOARD_QUEUE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w"
                    .parse()
                    .expect("valid mainnet switchboard queue pubkey")
            });
        match swb_cranker::SwbCranker::new(cfg.rpc_url.clone(), swb_queue, signers.fee_payer.clone())
            .await
        {
            Ok(c) => {
                tracing::info!("switchboard pull-feed cranker initialized");
                Some(Arc::new(c))
            }
            Err(e) => {
                tracing::warn!(error = %e, "switchboard cranker init failed; liquidator will skip Switchboard pre-crank");
                None
            }
        }
    } else {
        None
    };

    // Optional continuous Switchboard warm-keeper — DEFAULT OFF.
    //
    // It posts a Switchboard On-Demand update on a fixed cadence to keep
    // the pull feed warm on-chain. But each consensus update carries a
    // per-update oracle charge (~0.0026 SOL — far more than the tx fee),
    // so on a 20s cadence it drains the fee payer by ~0.5 SOL/day. And it
    // is redundant: every consumer that reads the feed (the UI, the TS
    // scripts, and the liquidator's own pre-crank below) already BUNDLES
    // a fresh `fetchUpdateIx` into its consuming tx, so freshness is paid
    // for per-use by whoever needs it. Leaving this loop on just pre-pays
    // for freshness nobody depends on.
    //
    // Set SWITCHBOARD_WARMKEEPER=true to re-enable (e.g. for readers that
    // can't bundle); it then runs at a fixed 60s cadence.
    let warmkeeper_enabled = std::env::var("SWITCHBOARD_WARMKEEPER")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let _swb_crank_task = swb_cranker
        .as_ref()
        .filter(|_| warmkeeper_enabled)
        .map(|c| {
            // Fixed 60s cadence when explicitly opted in — this is an
            // escape hatch, not a tuning surface, so no env knob. Even at
            // 60s the per-update oracle charge adds up (~0.15 SOL/day),
            // so only enable it for a reader that genuinely can't bundle.
            let interval = std::time::Duration::from_secs(60);
            tracing::info!("switchboard warm-keeper ENABLED (60s cadence)");
            swb_cranker::spawn_swb_crank_loop(Arc::clone(c), cfg.clone(), stop.clone(), interval)
        });

    let fee_payer_low = Arc::new(AtomicBool::new(false));
    let ata_balances = Arc::new(RwLock::new(HashMap::new()));
    let ctx = HandlerContext {
        cfg: cfg.clone(),
        rpc: rpc.clone(),
        chain: chain.clone(),
        signers: signers.clone(),
        stop: stop.clone(),
        fee_payer_low: fee_payer_low.clone(),
        ata_balances: ata_balances.clone(),
        swb_cranker,
    };

    let _sol_balance_task = metrics::spawn_sol_balance_loop(
        rpc.clone(),
        signers.clone(),
        stop.clone(),
        fee_payer_low,
        cfg.min_signer_balance_lamports,
    );

    let _ata_balance_task = metrics::spawn_ata_balance_loop(
        rpc.clone(),
        cfg.clone(),
        signers.clone(),
        stop.clone(),
        ata_balances,
        std::time::Duration::from_secs(60),
    );

    let _bank_refresh_task = metrics::spawn_bank_refresh_loop(
        rpc.clone(),
        chain.clone(),
        cfg.clone(),
        stop.clone(),
        cfg.banks_refresh_interval,
    );

    let mut handles = Vec::new();
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
    if cfg.handlers.curator_fee_claimer_enabled {
        handles.push(spawn(
            Arc::new(CuratorFeeClaimerHandler::new()),
            ctx.clone(),
            cfg.handlers.curator_fee_claimer_interval,
        ));
    }
    if cfg.handlers.match_cranker_enabled {
        handles.push(spawn(
            Arc::new(MatchCrankerHandler::new()),
            ctx.clone(),
            cfg.handlers.match_cranker_interval,
        ));
    }

    tracing::info!(handlers_spawned = handles.len(), "bot running");

    ready.store(true, std::sync::atomic::Ordering::Relaxed);

    for h in handles {
        let _ = h.await;
    }
    tracing::info!("ydelta-crankers stopped");
    Ok(())
}

/// Without this, tokio swallows task panics into a `JoinError` that
/// nobody awaits — the bot keeps running with one dead handler. Force
/// a process exit so the orchestrator restarts cleanly.
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
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }));
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ydelta_crankers=info"));
    if std::env::var("LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
