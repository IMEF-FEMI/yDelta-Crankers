//! Cranker-side marginfi `Bank` decoder.
//!
//! `marginfi-mocks::Bank` (vendored by the ydelta program) opaque-tails
//! the InterestRateConfig + total_shares fields. The curator rate keeper
//! needs them to compute live borrow/supply APR, so we read at hardcoded
//! offsets here.
//!
//! Source of truth: the v0.1.8 IDL at
//! `ydelta/programs/ydelta/tests/fixtures/marginfi.idl.json`. Offsets
//! derived directly from the layout comment in
//! `marginfi-mocks/src/state.rs`.

use anyhow::{anyhow, Result};
use solana_program::pubkey::Pubkey;

/// Anchor's 8-byte discriminator prefix on every account.
const DISCRIMINATOR_LEN: usize = 8;
/// Marginfi v0.1.8 Bank account total size.
pub const BANK_ACCOUNT_SIZE: usize = 1864;
/// Bank body size (post-discriminator).
pub const BANK_BODY_SIZE: usize = 1856;

// ─── Bank body offsets (post-discriminator) ─────────────────────────
//   mint(32)+mint_decimals(1)+group(32)+_pad0(7)            =  72
//   asset_share_value(16)+liability_share_value(16)         = 104
//   liquidity_vault(32)+bump(1)+lva_bump(1)                 = 138
//   insurance_vault(32)+bump(1)+bump(1)+_pad1(4)            = 176
//   collected_insurance_fees_outstanding(16)                = 192
//   fee_vault(32)+bump(1)+bump(1)+_pad2(6)                  = 232
//   collected_group_fees_outstanding(16)                    = 248
//   total_liability_shares(16)                              = 264
//   total_asset_shares(16)                                  = 280
//   last_update(8)                                          = 288
//   config (BankConfig, 544 bytes)                          = 832
//
// Within `BankConfig` (start = body offset 288):
//   asset_weight_init(16)+maint(16)+liab_init(16)+maint(16) =  64
//   deposit_limit(8)                                        =  72
//   interest_rate_config (240 bytes)                        = 312
//   operational_state(1)                                    = 313
//   oracle_setup(1)                                         = 314
//   oracle_keys[5 × 32 = 160]                               = 474
//   ... (rest unused by the cranker)
//
// Within `InterestRateConfig` (start = body offset 360):
//   optimal_utilization_rate     (16) → 360..376
//   plateau_interest_rate        (16) → 376..392
//   max_interest_rate            (16) → 392..408
//   insurance_fee_fixed_apr      (16) → 408..424
//   insurance_ir_fee             (16) → 424..440
//   protocol_fixed_fee_apr       (16) → 440..456
//   protocol_ir_fee              (16) → 456..472
//   (padding to 600                  ) → 472..600
const OFF_MINT: usize = 0;
const OFF_ASSET_SHARE_VALUE: usize = 72;
const OFF_LIABILITY_SHARE_VALUE: usize = 88;
const OFF_LIQUIDITY_VAULT: usize = 104;
// `liquidity_vault(32) + liquidity_vault_bump(1)` = body offset 137.
const OFF_LVA_BUMP: usize = 137;
const OFF_TOTAL_LIABILITY_SHARES: usize = 248;
const OFF_TOTAL_ASSET_SHARES: usize = 264;

const BANK_CONFIG_OFFSET: usize = 288;
const OFF_INTEREST_RATE_CONFIG: usize = BANK_CONFIG_OFFSET + 72; // 360
const OFF_OPTIMAL_UTIL: usize = OFF_INTEREST_RATE_CONFIG;
const OFF_PLATEAU: usize = OFF_INTEREST_RATE_CONFIG + 16;
const OFF_MAX_IR: usize = OFF_INTEREST_RATE_CONFIG + 32;
const OFF_PROTOCOL_IR_FEE: usize = OFF_INTEREST_RATE_CONFIG + 96;

const OFF_ORACLE_SETUP: usize = BANK_CONFIG_OFFSET + 313;
const OFF_ORACLE_KEYS: usize = BANK_CONFIG_OFFSET + 314;
const MAX_ORACLE_KEYS: usize = 5;

/// Anchor discriminator for marginfi Bank accounts (v0.1.8).
/// First 8 bytes of `sha256("account:Bank")`.
const BANK_DISCRIMINATOR: [u8; 8] = [142, 49, 166, 242, 50, 66, 97, 188];

/// Decoded view of the fields we read from a marginfi Bank account.
#[derive(Debug, Clone)]
pub struct BankView {
    pub mint: Pubkey,
    /// SPL token account holding the bank's deposited liquidity.
    pub liquidity_vault: Pubkey,
    /// Bump for the liquidity-vault authority PDA (seeds:
    /// `["liquidity_vault_auth", bank.key()]` against the marginfi
    /// program). The authority pubkey itself isn't stored on chain;
    /// derive it via this bump.
    pub lva_bump: u8,
    /// fp48 — marginfi's i80f48 packed into i128.
    pub asset_share_value_fp48: i128,
    pub liability_share_value_fp48: i128,
    pub total_asset_shares_fp48: i128,
    pub total_liability_shares_fp48: i128,
    /// All four IR-curve params are fp48 fractions (e.g. 0.8 = `0.8 << 48`).
    pub optimal_utilization_fp48: i128,
    pub plateau_interest_rate_fp48: i128,
    pub max_interest_rate_fp48: i128,
    /// fp48 fraction. APR multiplier deducted from supply yield.
    pub protocol_ir_fee_fp48: i128,
    pub oracle_setup: u8,
    /// Up to 5 oracle pubkeys; trailing default-zero entries are dropped.
    pub oracles: Vec<Pubkey>,
}

impl BankView {
    pub fn try_from_account_data(data: &[u8]) -> Result<Self> {
        if data.len() < BANK_ACCOUNT_SIZE {
            return Err(anyhow!(
                "bank account too small: {} < {}",
                data.len(),
                BANK_ACCOUNT_SIZE
            ));
        }
        if &data[..DISCRIMINATOR_LEN] != BANK_DISCRIMINATOR {
            return Err(anyhow!(
                "bank account discriminator mismatch (not a marginfi Bank?)"
            ));
        }
        let body = &data[DISCRIMINATOR_LEN..DISCRIMINATOR_LEN + BANK_BODY_SIZE];

        let mint = read_pubkey(body, OFF_MINT);
        let liquidity_vault = read_pubkey(body, OFF_LIQUIDITY_VAULT);
        let lva_bump = body[OFF_LVA_BUMP];
        let asset_share_value_fp48 = read_i128(body, OFF_ASSET_SHARE_VALUE);
        let liability_share_value_fp48 = read_i128(body, OFF_LIABILITY_SHARE_VALUE);
        let total_asset_shares_fp48 = read_i128(body, OFF_TOTAL_ASSET_SHARES);
        let total_liability_shares_fp48 = read_i128(body, OFF_TOTAL_LIABILITY_SHARES);
        let optimal_utilization_fp48 = read_i128(body, OFF_OPTIMAL_UTIL);
        let plateau_interest_rate_fp48 = read_i128(body, OFF_PLATEAU);
        let max_interest_rate_fp48 = read_i128(body, OFF_MAX_IR);
        let protocol_ir_fee_fp48 = read_i128(body, OFF_PROTOCOL_IR_FEE);

        let oracle_setup = body[OFF_ORACLE_SETUP];
        let mut oracles = Vec::with_capacity(MAX_ORACLE_KEYS);
        for i in 0..MAX_ORACLE_KEYS {
            let pk = read_pubkey(body, OFF_ORACLE_KEYS + i * 32);
            if pk != Pubkey::default() {
                oracles.push(pk);
            }
        }

        Ok(Self {
            mint,
            liquidity_vault,
            lva_bump,
            asset_share_value_fp48,
            liability_share_value_fp48,
            total_asset_shares_fp48,
            total_liability_shares_fp48,
            optimal_utilization_fp48,
            plateau_interest_rate_fp48,
            max_interest_rate_fp48,
            protocol_ir_fee_fp48,
            oracle_setup,
            oracles,
        })
    }
}

fn read_pubkey(body: &[u8], off: usize) -> Pubkey {
    let bytes: [u8; 32] = body[off..off + 32].try_into().unwrap();
    Pubkey::new_from_array(bytes)
}

fn read_i128(body: &[u8], off: usize) -> i128 {
    let bytes: [u8; 16] = body[off..off + 16].try_into().unwrap();
    i128::from_le_bytes(bytes)
}
