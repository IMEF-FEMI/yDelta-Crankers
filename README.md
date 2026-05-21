# ydelta-crankers

Off-chain keeper bots for the [yDelta](../ydelta) protocol. Three
permissionless cranking services in one binary, each running its own
poll loop with shared RPC state. State discovery is entirely
chain-driven — `getProgramAccounts` against the ydelta program for
markets and loans, plus in-place hypertree walks on `MarketFixed` for
matched-loan queues. No indexer endpoint.

The protocol moved to a **one-sided** model: only vault risk-profile
asks rest on the book. Borrowers place IOC bids that either cross
immediately, fall back to a `LoanType::P2Pool` marginfi-borrow node, or
drop. Matches still queue async in `market.matched_loans` and need
cranking.

## Cranking services

This binary runs the following crankers against the on-chain yDelta
program. Each handler is independent and gated by a `*_ENABLED` env
flag. The first three are permissionless and sign with the fee payer;
the fourth signs with per-curator keypairs.

1. **`promoter`** — promotes new `MatchedLoan` queue entries into live
   `LoanFixed` PDAs (tag 5 `ProcessMatchedLoan`). One ix path: dispatch
   on the node's `loan_type` and `flags` to decide whether to pass the
   trailing 15-account `VaultSettleAddrs` bundle (required for Fixed
   loans with a vault lender, skipped for `VAULT_PRESETTLED` nodes
   emitted by `ConvertP2PoolToFixed`, and skipped for P2Pool loans).
   Pays loan-PDA rent (refunded at claim time).
2. **`liquidator`** — settles matured loans (tag 16 `SettleMaturedLoan`)
   and liquidates LTV-breach loans (tag 17 `LiquidateLoan`), pre-flighted
   by `CheckLtvLiquidatable` (tag 34) / `CheckMaturityLiquidatable`
   (tag 35) sims. On loans that are BOTH matured and under-water the
   liquidator tries `liquidate_loan` first to capture the keeper bonus,
   falling back to `settle_matured_loan` on sim failure. When the
   fee-payer's debt ATA can't fund a full repay, a partial settle/
   liquidate is submitted (respecting the program's 1% / 1000-atom
   floor) so an underfunded ATA never deadlocks a large loan. Same
   handler covers Fixed and P2Pool loan bodies — the program branches
   inside the processor.
3. **`claimer`** — drains fully-repaid vault-funded loans back to the
   GlobalVault (tag 20 `ClaimRepaymentForRiskProfile`). Recovers the
   rent the promoter paid via the `cranker_refund` slot. Discovery is
   permissionless and curator-config-free: one
   `getProgramAccounts(LoanFixed, state=Repaid, lender_kind=RiskProfile)`
   per tick, client-side filtered by `now >= matures_at_unix`.
4. **`curator_fee_claimer`** *(opt-in)* — drains
   `RiskProfile.accumulated_curator_fee_atoms` to each curator's wallet
   ATA on a configurable cadence (default 1h, tag 15
   `ClaimCuratorFee`). Signs with the per-curator keypair loaded from
   `CURATOR_KEYPAIRS_JSON`. Discovery: one
   `getProgramAccounts(GlobalVaultFixed)` per tick plus an in-place
   walk of each vault's `risk_profiles` tree; skips profiles whose
   curator key we don't hold and profiles below
   `MIN_CURATOR_FEE_CLAIM_ATOMS`.

| Handler | Instructions | Sim gate | Signer | Permissionless? |
|---|---|---|---|---|
| `promoter` | tag 5 | — | fee payer | yes |
| `liquidator` | tag 16, 17 | tag 34, 35 | fee payer | yes |
| `claimer` | tag 20 | — | fee payer | yes |
| `curator_fee_claimer` | tag 15 | — | curator keypair | curator-gated |

**Not cranker territory.** `PlaceOrderForRiskProfile` (tag 12),
`CancelOrderForRiskProfile` (tag 13), and `UpdateOrderForRiskProfile`
(tag 14) require `signer == profile.curator` *and* a live strategy
decision — those are exclusively a UI concern. Same for
`ConvertP2PoolToFixed` (tag 33), which requires the borrower signer.
`ProtocolFeeClaim` (tag 19) requires the per-market admin and is
expected to run as a one-shot operator script.

## Architecture

```
                              ┌── getProgramAccounts(MarketFixed)        (markets, 30s TTL)
                              ├── getProgramAccounts(LoanFixed,
RPC  ◄──────  cranker  ──────►│       market | state+lender_kind filter) (loans)
                              ├── getAccountInfo(MarketFixed)             (matched-loan walk)
                              └── Tx ── ix submission
```

No Geyser, no WebSocket subscriptions, no indexer. Candidate discovery
is `getProgramAccounts` on the ydelta program filtered by account
discriminator plus targeted memcmps:

- Markets — discriminator only (small set; cached 30s).
- Loans for the liquidator — discriminator + `market` memcmp at offset 8.
- Loans for the claimer — discriminator + `state == Repaid` (offset
  196) + `lender_kind == RISK_PROFILE` (offset 201). One query covers
  every vault, every profile, every market.
- Matched-loan queues — read from the `MarketFixed` dynamic region
  in-place via `hypertree` iterators that ship with the program crate,
  so the on-disk layout can never drift.

When the LTV liquidator faces competitive pressure (third-party keepers
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
    ├── signer.rs         load fee-payer keypair from JSON file or base58
    ├── rpc.rs            send + sim + retry; priority-fee preamble
    ├── chain_reader.rs   on-chain state reader (markets, loans, matched-
    │                       loan queues)
    ├── bank_registry.rs  per-mint marginfi bank metadata, chain-driven
    ├── marginfi_bank.rs  bytemuck decoder for the marginfi Bank account
    ├── metrics.rs        Prometheus exposition + classifier
    ├── health_server.rs  /healthz + /readyz
    └── handlers/
        ├── mod.rs               Handler trait + supervisor
        ├── util.rs              shared helpers (now_unix, P2Pool stage math)
        ├── promoter.rs          tag 5
        ├── liquidator.rs        tag 16 + 17 (sim 34, 35)
        ├── claimer.rs           tag 20
        └── curator_fee_claimer.rs   tag 15 (opt-in, per-curator signer)
```

The cranker **depends on the `ydelta` program crate as a git dep**
pinned to a specific revision of
[IMEF-FEMI/yDelta](https://github.com/IMEF-FEMI/yDelta), with
`features = ["no-entrypoint"]`. This gives us — for free — every ix
builder, account type, and PDA helper the program defines. The on-disk
layout can't drift.

`ydelta` is treated as packaged upstream — we consume it at a pinned
rev, never modify it from this project. Bump the rev in `Cargo.toml`
whenever ydelta ships a change the cranker needs.

## Local dev

Prereqs: Rust 1.90 (set in `rust-toolchain.toml`) and a Solana RPC
endpoint.

yDelta runs on **localhost** (solana-test-validator) and **mainnet**
only. Point `RPC_URL` at the appropriate endpoint.

```sh
# From frontier_2026/crankers
cp .env.example .env
# Fill in:
#   - RPC_URL
#   - FEE_PAYER_KEYPAIR (path to a funded keypair JSON)
#     or FEE_PAYER_KEYPAIR_BASE58 (inline secret)
#   - MARGINFI_PROGRAM_ID + MARGINFI_GROUP
#
# Bank metadata and the liquidator's ATAs are derived from chain at
# boot — no env entry needed. You DO still need to fund the fee
# payer's debt-mint ATA before the liquidator can settle anything.

cargo run --release
```

Disable handlers you don't want to run via the `*_ENABLED=false` env vars.

To iterate on an unpushed `ydelta` change, uncomment the `[patch.…]`
block at the bottom of `Cargo.toml` so the cranker builds against
`../ydelta`. Re-comment + bump the `rev` once the change is pushed.

## Railway deploy

1. **Add the service.** Connect the standalone crankers repo directly;
   the Dockerfile and `railway.toml` live at the repo root, and the
   ydelta program crate is fetched as a git dep at build time (no
   sibling directories required).
   - Builder: Dockerfile (auto-detected via `railway.toml`).
   - Start command: image `ENTRYPOINT` (no override needed).
2. **Set environment variables** in the service's _Variables_ tab. Use
   `.env.example` as the contract. Mount the keypair file via Railway's
   "Files" / secret-file mechanism and reference it by absolute path
   (`/secrets/fee-payer.json`), or use `FEE_PAYER_KEYPAIR_BASE58` for
   an inline secret.
3. **Grafana Cloud metrics.** The bot exposes Prometheus exposition on
   `$METRICS_BIND` (default `0.0.0.0:9091`) at `/metrics`. Point
   Grafana Cloud's hosted Prometheus at this URL via a scrape config
   in your stack.
   - Metrics emitted: `ydelta_cranker_ticks_total{handler,outcome}`,
     `ydelta_cranker_tick_duration_seconds{handler}`,
     `ydelta_cranker_ixs_submitted_total{ix,outcome}`,
     `ydelta_cranker_ix_latency_seconds{ix,outcome}`,
     `ydelta_cranker_signer_sol_balance{signer,pubkey}`.
   - Set `LOG_FORMAT=json` for structured stdout logs.

## Chain reads in use

Every handler talks to the RPC directly via `ChainReader`:

- `getProgramAccounts(ydelta, filter=MarketFixed-discrim)` — boot
  (bank discovery), then on a 30s TTL cache + a 5-minute background
  refresh so newly-created markets surface without a restart.
- `getProgramAccounts(ydelta, filter=LoanFixed-discrim + market memcmp)`
  — liquidator per-market scan.
- `getProgramAccounts(ydelta, filter=LoanFixed-discrim + state=Repaid
  + lender_kind=RiskProfile)` — claimer scan, single query covers
  every vault.
- `getProgramAccounts(ydelta, filter=GlobalVaultFixed-discrim)` —
  curator-fee-claimer scan (in-place walk of each vault's
  `risk_profiles` tree).
- `getAccountInfo(market)` — promoter (matched-loan queue walk +
  defense-in-depth seat-kind check).

Hypertree walks live in-process via the `hypertree` crate that ships
alongside the program, so the on-disk layout can't drift.
