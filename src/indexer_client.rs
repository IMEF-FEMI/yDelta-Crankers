//! Typed client over the yDelta indexer's REST surface.
//!
//! The indexer is the system of record for candidate discovery — we
//! never poll the chain directly for loans. Endpoints used:
//!
//!   GET /v1/health
//!   GET /v1/loans?vault=&profile_id=&state=&market=&...   (filtered list)
//!   GET /v1/loans/:address                                  (single, full)
//!   GET /v1/markets                                         (list)
//!   GET /v1/markets/:address                                (single)
//!   GET /v1/markets/:address/orders                         (resting orders)
//!   GET /v1/vaults/:address/profiles/:profile_id            (risk profile)
//!   GET /v1/events?kinds=&market=&global_vault=&from_slot=  (event tape)
//!
//! Missing endpoints the cranker would benefit from (filed as TODOs;
//! the cranker has fallbacks for each):
//!   - GET /v1/markets/:address/matched-loans  →  promoter reads
//!     `MarketFixed` directly via RPC for the queue.
//!   - GET /v1/loans?repaid_unclaimed=true     →  claimer client-side
//!     filters `state == Repaid` + `now >= matures_at`.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use solana_program::pubkey::Pubkey;

#[derive(Clone)]
pub struct IndexerClient {
    base_url: String,
    http: Client,
}

impl IndexerClient {
    pub fn new(base_url: String) -> Self {
        // Strip embedded basic-auth credentials before storing. If the
        // operator ever sets `INDEXER_BASE_URL=https://user:pw@host`,
        // reqwest will still send the auth header (constructed from the
        // URL each request), but the stored URL — which lands in
        // tracing logs and `anyhow::Context` chains on every error —
        // won't expose the password. If your indexer needs auth,
        // prefer a bearer token via a header rather than userinfo.
        let stripped = strip_userinfo(&base_url);
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("ydelta-crankers/0.1")
            .build()
            .expect("reqwest client builds");
        Self {
            base_url: stripped,
            http,
        }
    }

    pub async fn health(&self) -> Result<()> {
        let url = format!("{}/v1/health", self.base_url);
        let res = self.http.get(&url).send().await.context("indexer health")?;
        if !res.status().is_success() {
            return Err(anyhow!("indexer health: HTTP {}", res.status()));
        }
        Ok(())
    }

    /// All loans the indexer knows about, filtered.
    /// Pass at least one of `lender`/`borrower`/`market`/`vault` per the
    /// indexer's contract (returns 400 otherwise).
    pub async fn loans(&self, q: LoansQuery<'_>) -> Result<Vec<LoanSummary>> {
        let mut url = reqwest::Url::parse(&format!("{}/v1/loans", self.base_url))?;
        {
            let mut p = url.query_pairs_mut();
            if let Some(s) = q.lender {
                p.append_pair("lender", &s.to_string());
            }
            if let Some(s) = q.borrower {
                p.append_pair("borrower", &s.to_string());
            }
            if let Some(s) = q.market {
                p.append_pair("market", &s.to_string());
            }
            if let Some(s) = q.vault {
                p.append_pair("vault", &s.to_string());
            }
            if let Some(id) = q.profile_id {
                p.append_pair("profile_id", &id.to_string());
            }
            if let Some(state) = q.state {
                p.append_pair("state", state);
            }
            if let Some(limit) = q.limit {
                p.append_pair("limit", &limit.to_string());
            }
        }
        let res: ListResponse<LoanSummary> = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(res.items)
    }

    /// Full single-loan view (includes `created_by`, `lender_kind`,
    /// `lender_global_vault`, share-price snapshots, etc.).
    pub async fn loan(&self, address: &Pubkey) -> Result<LoanFull> {
        let url = format!("{}/v1/loans/{}", self.base_url, address);
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn markets(&self) -> Result<Vec<MarketSummary>> {
        let url = format!("{}/v1/markets", self.base_url);
        let res: ListResponse<MarketSummary> = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(res.items)
    }

    /// All resting orders on a market, optionally filtered to a single owner.
    pub async fn market_orders(
        &self,
        market: &Pubkey,
        owner: Option<&Pubkey>,
    ) -> Result<Vec<OrderSummary>> {
        let mut url =
            reqwest::Url::parse(&format!("{}/v1/markets/{}/orders", self.base_url, market))?;
        if let Some(o) = owner {
            url.query_pairs_mut().append_pair("owner", &o.to_string());
        }
        let res: ListResponse<OrderSummary> = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(res.items)
    }

    pub async fn risk_profile(&self, vault: &Pubkey, profile_id: u8) -> Result<RiskProfileView> {
        let url = format!(
            "{}/v1/vaults/{}/profiles/{}",
            self.base_url, vault, profile_id
        );
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Event tape filtered by kind(s) and slot horizon. Used by the
    /// policy-sync handler to detect `risk_profile_updated` events.
    pub async fn events(&self, q: EventsQuery<'_>) -> Result<Vec<EventRecord>> {
        let mut url = reqwest::Url::parse(&format!("{}/v1/events", self.base_url))?;
        {
            let mut p = url.query_pairs_mut();
            if !q.kinds.is_empty() {
                p.append_pair("kinds", &q.kinds.join(","));
            }
            if let Some(s) = q.market {
                p.append_pair("market", &s.to_string());
            }
            if let Some(s) = q.global_vault {
                p.append_pair("global_vault", &s.to_string());
            }
            if let Some(id) = q.profile_id {
                p.append_pair("profile_id", &id.to_string());
            }
            if let Some(slot) = q.from_slot {
                p.append_pair("from_slot", &slot.to_string());
            }
            if let Some(limit) = q.limit {
                p.append_pair("limit", &limit.to_string());
            }
        }
        let res: ListResponse<EventRecord> = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(res.items)
    }
}

// ─── Request DTOs ───

#[derive(Default, Clone, Copy)]
pub struct LoansQuery<'a> {
    pub lender: Option<&'a Pubkey>,
    pub borrower: Option<&'a Pubkey>,
    pub market: Option<&'a Pubkey>,
    pub vault: Option<&'a Pubkey>,
    pub profile_id: Option<u8>,
    /// `"active" | "closed" | "all"` per the indexer.
    pub state: Option<&'a str>,
    pub limit: Option<u32>,
}

#[derive(Default, Clone)]
pub struct EventsQuery<'a> {
    pub kinds: Vec<String>,
    pub market: Option<&'a Pubkey>,
    pub global_vault: Option<&'a Pubkey>,
    pub profile_id: Option<u8>,
    pub from_slot: Option<i64>,
    pub limit: Option<u32>,
}

// ─── Response DTOs ───

#[derive(Deserialize)]
struct ListResponse<T> {
    items: Vec<T>,
    #[serde(default)]
    #[allow(dead_code)]
    next_cursor: Option<String>,
}

/// Mirrors `loan_row_summary` in the indexer. All pubkeys are base58.
#[derive(Debug, Deserialize, Clone)]
pub struct LoanSummary {
    pub address: String,
    pub market: String,
    /// On-chain `LoanState as i16`. 0 = Active, 1 = Repaid, 3/9 = closed.
    pub state: i16,
    pub principal_debt_atoms: i64,
    pub outstanding_debt_atoms: i64,
    pub borrower_rate_bps: i32,
    pub lender_rate_bps: i32,
    pub matures_at_unix: i64,
    /// 0 = Wallet, 1 = RiskProfile.
    pub lender_kind: i16,
    pub lender_seat_owner: Option<String>,
    pub borrower_seat_owner: Option<String>,
    pub lender_global_vault: Option<String>,
    pub lender_profile_id: Option<i16>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoanFull {
    pub address: String,
    pub market: String,
    pub state: i16,
    pub principal_debt_atoms: i64,
    pub outstanding_debt_atoms: i64,
    pub collateral_atoms: i64,
    pub borrower_rate_bps: i32,
    pub lender_rate_bps: i32,
    pub matures_at_unix: i64,
    pub lender_kind: i16,
    pub lender_seat_owner: Option<String>,
    pub borrower_seat_owner: Option<String>,
    pub lender_global_vault: Option<String>,
    pub lender_profile_id: Option<i16>,
    pub matched_loan_sequence: i64,
    pub created_by: Option<String>,
    pub loan_type: i16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MarketSummary {
    pub address: String,
    pub debt_mint: String,
    pub collateral_mint: String,
    pub is_paused: bool,
    pub matched_loan_sequence: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OrderSummary {
    pub sequence: i64,
    pub side: i16,
    pub rate_bps: i32,
    pub term_seconds: i64,
    pub principal_atoms: i64,
    pub owner: Option<String>,
    pub owner_kind: Option<i16>,
    pub risk_profile_id: Option<i16>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RiskProfileView {
    pub profile_id: i16,
    pub curator: String,
    pub max_ltv_bps: i32,
    pub max_term_seconds: i64,
    pub allowed_market_max: i16,
    pub allowed_market_count: i16,
    pub active_markets: Vec<String>,
    pub deployed_principal_atoms: i64,
    pub total_principal_atoms: i64,
    pub encumbered_in_orders_atoms: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EventRecord {
    pub slot: i64,
    pub event_kind: String,
    pub market: Option<String>,
    pub global_vault: Option<String>,
    pub profile_id: Option<i16>,
    pub payload: serde_json::Value,
}

/// Strip basic-auth userinfo from a URL string (the `user:pass@` bit).
/// Falls back to the input unchanged when the URL doesn't parse or has
/// no userinfo. Used to keep credentials out of stored config and
/// downstream error messages.
fn strip_userinfo(raw: &str) -> String {
    let Some(scheme_end) = raw.find("://") else {
        return raw.to_string();
    };
    let after_scheme = &raw[scheme_end + 3..];
    let Some(at_in_authority) = after_scheme.find('@') else {
        return raw.to_string();
    };
    // Make sure the `@` we found is in the authority section, not the
    // path / query (a stray `@` in a path is legal). Look for the
    // first `/`, `?`, or `#` and check the `@` is before it.
    let authority_end = after_scheme
        .find(|c: char| matches!(c, '/' | '?' | '#'))
        .unwrap_or(after_scheme.len());
    if at_in_authority >= authority_end {
        return raw.to_string();
    }
    format!(
        "{}://{}",
        &raw[..scheme_end],
        &after_scheme[at_in_authority + 1..]
    )
}
