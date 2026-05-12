# ydelta-crankers

Off-chain keeper bots for the [yDelta](../ydelta) protocol. Five
cranking services in one binary, each running its own poll loop with
shared RPC + indexer state.

## Cranking services

This program operates the following crankers against the on-chain yDelta
program. Each runs as an independent handler with its own interval +
enable flag.

1. **`promoter`** — promotes new `MatchedLoan` queue entries into live
   `LoanFixed` PDAs (tag 7 `ProcessMatchedLoan`). Handles primary,
   secondary-full, and secondary-split crosses; assembles vault settlement
   accounts when the lender is a risk profile. Pays loan-PDA rent.
2. **`liquidator`** — settles matured loans (tag 20 `SettleMaturedLoan`)
   and liquidates LTV-breach loans (tag 21 `LiquidateLoan`). Pre-flighted
   via simulation-only gates (tag 40 `CheckLtvLiquidatable`, tag 41
   `CheckMaturityLiquidatable`) so we only submit txs that will land.
3. **`claimer`** — drains fully-repaid risk-profile-funded loans back into
   the GlobalVault (tag 24 `ClaimRepaymentForRiskProfile`). Recovers the
   rent the promoter paid via the `cranker_refund` slot.
4. **`policy_sync`** — re-stamps `risk_profile_max_ltv_bps` on every
   market-side vault seat after a curator runs `UpdateRiskProfile`
   (tag 38 `SyncMarketSeatsForRiskProfile`). Triggered by indexer events,
   batched ≤8 markets per ix.
5. **`curator_keeper`** — keeps each managed vault ask in sync with the
   target rate (tag 18 `UpdateOrderForRiskProfile`). Supports both static
   rates and dynamic marginfi-following (`target = supply + α × (borrow - supply)`).
   The quoted rate is clamped to `[marginfi_supply_apr, marginfi_borrow_apr]`;
   if marginfi's curve is inverted (`borrow ≤ supply`), the keeper quotes
   at `supply` so LPs never earn below the marginfi-supply baseline.

| Handler | Instructions | Signer | Permissionless? |
|---|---|---|---|
| `promoter` | tag 7 | fee payer | yes |
| `liquidator` | tag 20, 21 (+ sim 40, 41) | fee payer | yes |
| `claimer` | tag 24 | fee payer | yes |
| `policy_sync` | tag 38 | fee payer | yes |
| `curator_keeper` | tag 18 | per-profile curator key | no — curator-gated |

## Architecture

```
indexer (REST)  ──HTTP poll──►  cranker (handlers)  ──Tx──►  RPC
                                       │
                                       └── reads MarketFixed via RPC
                                           for matched-loan queue
                                           (indexer doesn't expose it)
```

No Geyser, no WebSocket subscriptions. The indexer is the system of
record for candidate discovery; the cranker is a periodic poller. This
keeps the bot lean (~1500 LOC) and operations free of a separate
streaming provider on day 1.

When the LTV liquidator hits competitive pressure (third-party keepers
racing for the bonus), swap the candidate source for a Geyser stream.
The handler loops won't change.

## Repo layout

```
crankers/
├── Cargo.toml
├── README.md
├── .env.example
└── src/
    ├── main.rs           sigterm/sigint, init handlers, supervise
    ├── config.rs         env → typed Config
    ├── signer.rs         load Keypair JSON files
    ├── rpc.rs            send + sim + retry; priority-fee preamble
    ├── indexer_client.rs typed HTTP client over the indexer REST API
    ├── bank_registry.rs  per-mint marginfi bank metadata, env-driven
    ├── market_reader.rs  direct RPC reads of `MarketFixed` + hypertree
    └── handlers/
        ├── mod.rs            Handler trait + supervisor
        ├── util.rs           shared helpers (now_unix, token program id)
        ├── promoter.rs       tag 7
        ├── claimer.rs        tag 24
        ├── liquidator.rs     tag 20 + 21 (+ sim 40, 41)
        ├── policy_sync.rs    tag 38
        └── curator_keeper.rs tag 18
```

The cranker **depends on the `ydelta` program crate as a git dep**
pinned to a specific revision of
[IMEF-FEMI/yDelta](https://github.com/IMEF-FEMI/yDelta), with
`features = ["no-entrypoint"]`. This gives us — for free — every ix
builder, account type, and PDA helper the program defines. We never
duplicate the on-chain layout.

`ydelta` itself is treated as packaged upstream — we consume it from
its own repo at a pinned rev, never modify it from this project.
Bump the rev in `Cargo.toml` whenever ydelta ships a change the
cranker needs.

## Local dev

Prereqs: Rust 1.90 (set in `rust-toolchain.toml` — newer than the
program crate because the cranker builds for the host and gets pulled
into the modern `solana-zk-sdk` graph), a Solana RPC endpoint, the
yDelta indexer running and reachable.

yDelta runs on **localhost** (solana-test-validator) and **mainnet**
only. Point `RPC_URL` at the appropriate endpoint and seed `BANKS` /
`LIQUIDATOR_*` with addresses for that target.

```sh
# From frontier_2026/crankers
cp .env.example .env
# Fill in:
#   - RPC_URL
#   - INDEXER_BASE_URL
#   - FEE_PAYER_KEYPAIR (path to a funded keypair JSON)
#   - MARGINFI_GROUP + MARGINFI_PROGRAM_ID
#   - BANKS (per-mint bank metadata)
#   - LIQUIDATOR_DEBT_ATAS + LIQUIDATOR_COLLATERAL_ATAS
#   - CURATORS (if running the curator keeper)

cargo run --release
```

Disable handlers you don't want to run via the `*_ENABLED=false` env vars.

## Railway deploy

1. **Add the service.** Connect the standalone crankers repo directly;
   the Dockerfile and `railway.toml` live at the repo root, and the
   ydelta program crate is fetched as a git dep at build time (no
   sibling directories required).
   - Builder: Dockerfile (auto-detected via `railway.toml`).
   - Start command: image `ENTRYPOINT` (no override needed).
2. **Set environment variables** in the service's _Variables_ tab. Use
   `.env.example` as the contract. Mount keypair files via Railway's
   "Files" / secret-file mechanism and reference them by absolute path
   (`/secrets/fee-payer.json`, etc.) — never inline base58.
3. **Grafana Cloud metrics.** The bot exposes Prometheus exposition on
   `$METRICS_BIND` (default `0.0.0.0:9091`) at `/metrics`. Point
   Grafana Cloud's hosted Prometheus at this URL via a scrape config
   in your stack; the free tier handles our volume comfortably.
   - Metrics emitted: `ydelta_cranker_ticks_total{handler,outcome}`,
     `ydelta_cranker_tick_duration_seconds{handler}`,
     `ydelta_cranker_ixs_submitted_total{ix,outcome}`,
     `ydelta_cranker_ix_latency_seconds{ix,outcome}`,
     `ydelta_cranker_signer_sol_balance{signer,pubkey}`.
   - Set `LOG_FORMAT=json` for structured stdout logs.

## Indexer dependencies

Endpoints the cranker consumes today (all under `/v1`):

- `GET /health`
- `GET /loans?vault=&profile_id=&state=&market=&limit=` — candidates for
  claimer / liquidator.
- `GET /loans/:address` — full loan view (needed for `matched_loan_sequence`).
- `GET /markets` — list of markets with `debt_mint` / `collateral_mint`.
- `GET /markets/:address/orders[?owner=]` — current resting orders for
  the curator keeper to compare against.
- `GET /vaults/:address/profiles/:profile_id` — `active_markets` list for
  policy sync.
- `GET /events?kinds=risk_profile_updated&from_slot=` — change feed for
  policy sync.

Missing / nice-to-have (the cranker has fallbacks):

- `GET /markets/:address/matched-loans` — would replace the cranker's
  direct `MarketFixed` walk in `market_reader.rs`.
- `state=repaid_unclaimed` filter on `/loans` — would replace the
  client-side filter in `claimer.rs`.
- Per-loan `matured_at` / `liquidatable` precomputes — would let the
  liquidator skip per-loan tag-40/41 sims when the indexer can already
  rule them out.

## Known v1 limitations

All material correctness gaps have been closed. Remaining items are
scope decisions, not bugs:

- **Bank registry is env-driven** via `BANKS`. The cranker reads the
  marginfi Bank for live rates (curator keeper) but still needs to
  know each bank's pubkey + liquidity-vault + LVA. v2 follow-up:
  auto-discover from marginfi `getProgramAccounts` filtered by
  `(mint, group)`.
- **No rent-receivables ledger.** The promoter passes its own fee-payer
  pubkey as `cranker_refund` at claim time. Rent recovery works as long
  as one wallet runs both promoter and claimer (the default). If we
  later split signers, add an explicit ledger of outstanding rent
  obligations keyed by loan-PDA.

