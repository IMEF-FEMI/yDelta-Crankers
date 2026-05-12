# ydelta-crankers

Off-chain keeper bots for the [yDelta](../ydelta) protocol. Five
cranking services in one binary, each running its own poll loop with
shared RPC state. State discovery is entirely chain-driven —
`getProgramAccounts` against the ydelta program for markets/loans, plus
in-place hypertree walks on `MarketFixed` and `GlobalVaultFixed` for
order books, matched-loan queues, and risk profiles. No indexer
endpoint.

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
   When no live ask exists yet, the keeper bootstraps the
   `(vault, profile, market)` end-to-end: if the vault-owned
   `ClaimedSeat` is also missing, it bundles `ClaimSeatForRiskProfile`
   (tag 15) + `PlaceOrderForRiskProfile` (tag 16) into one atomic tx
   so the order is live in a single round-trip. Both ixs are signed
   by the per-profile curator keypair (the yDelta program gates both
   on `signer == profile.curator`).

| Handler | Instructions | Signer | Permissionless? |
|---|---|---|---|
| `promoter` | tag 7 | fee payer | yes |
| `liquidator` | tag 20, 21 (+ sim 40, 41) | fee payer | yes |
| `claimer` | tag 24 | fee payer | yes |
| `policy_sync` | tag 38 | fee payer | yes |
| `curator_keeper` | tag 18 | per-profile curator key | no — curator-gated |

## Architecture

```
                              ┌── getProgramAccounts(MarketFixed)
                              ├── getProgramAccounts(LoanFixed, market | vault+profile filter)
RPC  ◄──────  cranker  ──────►├── getAccountInfo(MarketFixed)  ── walk hypertree (asks/bids/matched_loans)
                              ├── getAccountInfo(GlobalVaultFixed) ── walk risk_profiles tree
                              └── Tx ── ix submission
```

No Geyser, no WebSocket subscriptions, no indexer. Candidate discovery
is `getProgramAccounts` on the ydelta program filtered by account
discriminator (and `market` / `lender_global_vault` memcmps where it
narrows the set). Dynamic-region data — order books, matched-loan
queues, vault risk profiles — is fetched via `getAccountInfo` and
walked in-place with the hypertree iterators from the program crate.

A short-TTL in-process cache (`ChainReader::list_markets`, 30s)
deduplicates the market list across handler ticks so the bot makes
one `getProgramAccounts(MarketFixed)` per ~30s rather than per
handler-tick.

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
    ├── chain_reader.rs   on-chain state reader (markets, loans, orders,
    │                       risk profiles, matched-loan queues) — full
    │                       replacement for the indexer client
    ├── bank_registry.rs  per-mint marginfi bank metadata, chain-driven
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
into the modern `solana-zk-sdk` graph) and a Solana RPC endpoint.

yDelta runs on **localhost** (solana-test-validator) and **mainnet**
only. Point `RPC_URL` at the appropriate endpoint and seed `BANKS` /
`LIQUIDATOR_*` with addresses for that target.

```sh
# From frontier_2026/crankers
cp .env.example .env
# Fill in:
#   - RPC_URL
#   - FEE_PAYER_KEYPAIR (path to a funded keypair JSON)
#   - MARGINFI_GROUP + MARGINFI_PROGRAM_ID
#   - CURATORS (if running the curator keeper)
#
# Bank metadata and the liquidator's ATAs are derived from chain at
# boot — no env entry needed.

cargo run --release
```

Disable handlers you don't want to run via the `*_ENABLED=false` env vars.

### One-shot helpers

`src/bin/` carries small standalone binaries that reuse the same Config /
Signers / Rpc plumbing the supervisor uses. They read the same `.env`,
so once your dev env works for the main bot it works for these too.

- `place_order` — submit a single primary `PlaceOrder` ix on the
  SOL/USDC market, signed by the fee payer. Defaults to a $10 USDC
  Limit Ask at 7.20% APY / 14d term; override via
  `PLACE_ORDER_SIDE` / `PLACE_ORDER_PRINCIPAL_ATOMS` /
  `PLACE_ORDER_RATE_BPS` / `PLACE_ORDER_TERM_SECONDS` /
  `PLACE_ORDER_COLLATERAL_ATOMS` / `PLACE_ORDER_LAST_VALID_TS` /
  `PLACE_ORDER_FLAGS`. Asks require pre-deposited USDC on the
  signer's seat; Bids require pre-deposited wSOL collateral.

  ```sh
  cargo run --release --bin place_order
  ```

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

## Chain reads in use

Every handler talks to the RPC directly via `ChainReader`:

- `getProgramAccounts(ydelta, filter=MarketFixed-discrim, data_size=512)`
  — used at boot (bank discovery) and on a 30s TTL cache during normal
  operation. Drives the markets list every handler reads.
- `getProgramAccounts(ydelta, filter=LoanFixed-discrim + market memcmp)`
  — liquidator per-market scan.
- `getProgramAccounts(ydelta, filter=LoanFixed-discrim + lender_global_vault +
  lender_profile_id memcmps)` — claimer per-profile scan.
- `getAccountInfo(market)` — promoter (matched-loan queue walk),
  curator keeper (asks tree walk for the managed order), liquidator
  (seat lookups).
- `getAccountInfo(global_vault)` — policy_sync (read live
  `RiskProfile.active_markets`).

Hypertree walks live in-process via the `hypertree` crate that ships
alongside the program, so the on-disk layout can't drift.

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

