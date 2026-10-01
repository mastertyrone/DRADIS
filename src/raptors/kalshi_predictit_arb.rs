// SPDX-License-Identifier: AGPL-3.0-only
//
// DRADIS — autonomous trading engine for crypto prediction markets.
// Copyright (C) 2026 Team Takatini
//
// This file is part of DRADIS. DRADIS is free software: you can redistribute it
// and/or modify it under the terms of the GNU Affero General Public License,
// version 3, as published by the Free Software Foundation.
//
// DRADIS is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR
// A PARTICULAR PURPOSE. See the GNU Affero General Public License for details.
//
// You should have received a copy of the GNU Affero General Public License along
// with this program. If not, see <https://www.gnu.org/licenses/>.

/// Kalshi–PredictIt Arb Raptor — cross-venue arbitrage scout (observe-only).
///
/// A venue-neutral Raptor that reads the third-party `kalshi-predictit-arb`
/// scanner: Kalshi markets entity-resolved against PredictIt contracts (state,
/// district, office, party, numeric strike and cycle year gating; a party
/// mismatch is a hard reject), both arbitrage directions priced after each
/// venue's fee drag (Kalshi taker `ceil(7·P·(1−P))`¢ per leg; PredictIt 10% of
/// winning-leg profit + 5% withdrawal drag), and the worst-case net across
/// settlement outcomes. `executable` is true only at ≥ 1¢ net. Gaps are
/// indicative until checked against live depth, current fees and resolution
/// equivalence — DRADIS has no PredictIt venue, so this is a signal only.
///
/// Disclosure: kalshi-predictit-arb is built and operated by Team Takatini.
/// Docs: <https://mastertyrone.github.io/kalshi-predictit-arb/> · client:
/// <https://github.com/mastertyrone/kalshi-predictit-arb>.
///
/// ── Modes ───────────────────────────────────────────────────────────────────
/// │ Mode │ When                          │ Network │ Cost                     │
/// │──────│───────────────────────────────│─────────│──────────────────────────│
/// │ Demo │ `X402_WALLET_KEY` unset/empty │ none    │ free — fictional SAMPLE  │
/// │ Live │ key set AND a signing build   │ feed    │ $0.02 USDC/call on Base  │
///
/// Demo mode publishes one snapshot built from the bundled fictional fixture
/// (`fixtures/kalshi_predictit_arb_sample.json`, NOT real market data) with
/// `sample_data = true` and `connected = false`, then parks — so a consumer that
/// treats a disconnected feed as neutral (the house rule for every Raptor)
/// ignores it. Live signing needs `alloy`, which is gated behind the
/// `intl_clob` venue feature; other venue builds opt in with the `x402` feature
/// (`--features kalshi,x402`). Without either, a set key logs a warning and the
/// raptor stays in demo mode.
///
/// ── Payment (x402 v2) ───────────────────────────────────────────────────────
/// The first GET returns 402 with payment requirements (`PAYMENT-REQUIRED`
/// header, base64 JSON, or the JSON body). A requirement is accepted only if it
/// is Base (`eip155:8453`) / scheme `exact` / Base USDC / `0 < amount ≤`
/// `max_usd_per_call` (default $0.02) with a well-formed `payTo`; an empty
/// `accepts` list is refused. The EIP-3009 `TransferWithAuthorization` is
/// signed locally (the key never leaves the process), sent ONCE as unpadded
/// base64url JSON in `PAYMENT-SIGNATURE` and `X-PAYMENT`, and a second 402 is a
/// failure — never a retry loop. Spend is further capped per UTC day by
/// `daily_budget_usd`; once exhausted the raptor skips the network until the
/// day rolls over.
///
/// ── Budget (honest numbers) ─────────────────────────────────────────────────
/// Every live poll costs $0.02. The default `poll_secs = 3600` is 24 calls =
/// $0.48/day, inside the default `daily_budget_usd = 0.50`. 900s polling would
/// be ~$1.92/day and is refused by the default budget after 25 calls.
///
/// ── Observe-only status ─────────────────────────────────────────────────────
/// Like the Tide, Sports and Tennis Raptors it is NOT consumed by any Viper. It
/// is also not spawned by `main.rs`: an operator who wants it spawns
/// `run_kalshi_predictit_arb_raptor` beside the Tennis Raptor (see
/// `examples/kalshi_predictit_arb_demo.rs` for the offline demo).
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::{json, Value};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::config;

/// Env var carrying the x402 wallet key. Absent/empty ⇒ offline demo mode.
pub const WALLET_KEY_ENV: &str = config::KALSHI_PREDICTIT_ARB_WALLET_KEY_ENV;
/// Live feed endpoint (x402-paid).
pub const DEFAULT_URL: &str =
    "https://x402.bankr.bot/0x69fb671637ed68881f66b9ebf305ec3ef5574f65/kalshi-predictit-arb";
/// Networks accepted as Base mainnet.
pub const BASE_NETWORKS: [&str; 2] = ["eip155:8453", "base"];
pub const BASE_CHAIN_ID: u64 = 8453;
/// Native USDC on Base.
pub const BASE_USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const USDC_UNITS: Decimal = dec!(1_000_000);
/// Fictional SAMPLE fixture (NOT real market data) used by demo mode and tests.
pub const SAMPLE_JSON: &str = include_str!("fixtures/kalshi_predictit_arb_sample.json");

/// Feed filter: only executable pairs, or every evaluated pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FeedMode {
    #[default]
    Opportunities,
    All,
}

impl FeedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FeedMode::Opportunities => "opportunities",
            FeedMode::All => "all",
        }
    }
}

/// Raptor settings. Defaults come from the `KALSHI_PREDICTIT_ARB_*` constants in
/// `src/config.rs` (present in all three profile templates).
#[derive(Clone, Debug)]
pub struct ArbRaptorConfig {
    pub url: String,
    /// Keyword filter (`governor`, `senate`, …); empty = everything.
    pub q: String,
    /// Pairs evaluated per call, clamped to 1–25.
    pub limit: u32,
    pub mode: FeedMode,
    pub poll_secs: u64,
    /// Refuse any single payment above this (USD).
    pub max_usd_per_call: Decimal,
    /// Refuse payments once this much was spent in the current UTC day (USD).
    pub daily_budget_usd: Decimal,
    /// The feed can cold-start slowly; the docs recommend ~30s.
    pub timeout_secs: u64,
}

impl Default for ArbRaptorConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            q: config::KALSHI_PREDICTIT_ARB_Q.to_string(),
            limit: config::KALSHI_PREDICTIT_ARB_LIMIT,
            mode: if config::KALSHI_PREDICTIT_ARB_ALL_PAIRS {
                FeedMode::All
            } else {
                FeedMode::Opportunities
            },
            poll_secs: config::KALSHI_PREDICTIT_ARB_POLL_SECS,
            max_usd_per_call: config::KALSHI_PREDICTIT_ARB_MAX_USD_PER_CALL,
            daily_budget_usd: config::KALSHI_PREDICTIT_ARB_DAILY_BUDGET_USD,
            timeout_secs: config::KALSHI_PREDICTIT_ARB_TIMEOUT_SECS,
        }
    }
}

impl ArbRaptorConfig {
    pub fn max_atomic(&self) -> u64 {
        usd_to_atomic(self.max_usd_per_call)
    }

    fn query(&self) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("limit", self.limit.clamp(1, 25).to_string()),
            ("mode", self.mode.as_str().to_string()),
        ];
        if !self.q.trim().is_empty() {
            params.push(("q", self.q.trim().to_string()));
        }
        params
    }
}

/// Normalized snapshot broadcast over the `watch` channel. `Copy` + all-zero
/// `Default` like the other Raptors, so the channel can be seeded before the
/// first poll and a zero/disconnected snapshot reads as neutral.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ArbSnapshot {
    /// Pairs returned by the last poll.
    pub num_pairs: Decimal,
    /// Pairs with `executable == true` (≥ 1¢ worst-case net after fees).
    pub num_executable: Decimal,
    /// Best worst-case net yield across returned pairs, in cents per contract.
    pub best_net_yield_c: Decimal,
    pub best_net_yield_pct: Decimal,
    /// Annualized return on capital of that best pair (to later settlement).
    pub best_roc_annualized_pct: Decimal,
    /// True only after a successful LIVE poll.
    pub connected: bool,
    /// True when the values come from the fictional sample fixture.
    pub sample_data: bool,
}

/// One matched pair, reduced from the feed (tolerant of missing fields).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArbPair {
    pub event: String,
    pub kalshi_ticker: String,
    pub predictit_market: String,
    pub predictit_contract: String,
    pub direction: String,
    pub net_yield_c: Option<Decimal>,
    pub net_yield_pct: Option<Decimal>,
    pub roc_annualized_pct: Option<Decimal>,
    pub days_to_settlement: Option<i64>,
    pub executable: bool,
}

/// A reduced feed response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArbSample {
    pub pairs: Vec<ArbPair>,
    pub pairs_evaluated: Option<u64>,
    pub fetched_at: String,
    pub sample_data: bool,
}

impl ArbSample {
    /// The pair with the highest worst-case net yield, if any carries one.
    pub fn best(&self) -> Option<&ArbPair> {
        self.pairs
            .iter()
            .filter(|p| p.net_yield_c.is_some())
            .max_by_key(|p| p.net_yield_c)
    }

    pub fn snapshot(&self, connected: bool) -> ArbSnapshot {
        let best = self.best();
        ArbSnapshot {
            num_pairs: Decimal::from(self.pairs.len() as u64),
            num_executable: Decimal::from(self.pairs.iter().filter(|p| p.executable).count() as u64),
            best_net_yield_c: best.and_then(|p| p.net_yield_c).unwrap_or_default(),
            best_net_yield_pct: best.and_then(|p| p.net_yield_pct).unwrap_or_default(),
            best_roc_annualized_pct: best.and_then(|p| p.roc_annualized_pct).unwrap_or_default(),
            connected,
            sample_data: self.sample_data,
        }
    }
}

/// Reduce a feed response body. Pure (no I/O) so parsing is unit-testable.
pub fn reduce_feed(body: &str, sample_data: bool) -> Result<ArbSample, String> {
    let parsed: Value = serde_json::from_str(body).map_err(|e| format!("invalid JSON: {e}"))?;
    if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
        return Err(format!("feed error: {}", truncate(&err.to_string(), 200)));
    }
    let rows = parsed
        .get("opportunities")
        .and_then(|o| o.as_array())
        .ok_or_else(|| {
            format!(
                "expected {{opportunities: [...]}}, got: {}",
                truncate(body, 200)
            )
        })?;
    let pairs = rows
        .iter()
        .filter(|r| r.is_object())
        .filter_map(reduce_pair)
        .collect();
    Ok(ArbSample {
        pairs,
        pairs_evaluated: parsed
            .pointer("/stats/pairs_evaluated")
            .and_then(|v| v.as_u64()),
        fetched_at: str_at(&parsed, "/fetched_at"),
        sample_data,
    })
}

/// Row label: `event` if it is a non-empty string, else `pair` (older client
/// fixtures used `pair`). Rows with neither are skipped (`None`).
fn row_label(row: &Value) -> Option<String> {
    ["event", "pair"].iter().find_map(|key| {
        row.get(*key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn reduce_pair(row: &Value) -> Option<ArbPair> {
    let event = row_label(row)?;
    Some(ArbPair {
        event,
        kalshi_ticker: str_at(row, "/kalshi/ticker"),
        predictit_market: str_at(row, "/predictit/market"),
        predictit_contract: str_at(row, "/predictit/contract"),
        direction: str_at(row, "/best_direction/direction"),
        net_yield_c: dec_at(row, "/best_direction/net_yield_c"),
        net_yield_pct: dec_at(row, "/best_direction/net_yield_pct"),
        roc_annualized_pct: dec_at(row, "/roc_annualized_pct"),
        days_to_settlement: row.get("days_to_settlement").and_then(|v| v.as_i64()),
        executable: row
            .get("executable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// Filter the fictional fixture the way the live endpoint filters real data.
pub fn sample_feed(q: &str, limit: u32, mode: FeedMode) -> ArbSample {
    let mut sample = reduce_feed(SAMPLE_JSON, true).unwrap_or_default();
    sample.sample_data = true;
    let words: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
    sample.pairs.retain(|p| {
        let text = format!(
            "{} {} {} {}",
            p.event, p.kalshi_ticker, p.predictit_market, p.predictit_contract
        )
        .to_lowercase();
        (mode == FeedMode::All || p.executable) && words.iter().all(|w| text.contains(w))
    });
    sample.pairs.truncate(limit.clamp(1, 25) as usize);
    sample
}

// ── x402 helpers ─────────────────────────────────────────────────────────────

/// Why a paid fetch did not produce data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentError {
    /// Requirements rejected before anything was signed.
    Refused(String),
    /// Daily budget would be exceeded; nothing was signed.
    Budget(String),
    /// A signed payment was sent and the server still answered 402.
    Failed(String),
    /// Transport / HTTP / parse error.
    Transport(String),
}

impl std::fmt::Display for PaymentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaymentError::Refused(m) => write!(f, "payment refused: {m}"),
            PaymentError::Budget(m) => write!(f, "budget: {m}"),
            PaymentError::Failed(m) => write!(f, "payment failed: {m}"),
            PaymentError::Transport(m) => write!(f, "transport: {m}"),
        }
    }
}

/// Unpadded base64url JSON — the form the live endpoint was tested with.
pub fn encode_payment_header(payload: &Value) -> String {
    URL_SAFE_NO_PAD.encode(payload.to_string())
}

/// Tolerant decode of an incoming header: standard or url-safe alphabet,
/// padded or not. `None` when it is not base64-encoded JSON.
pub fn decode_b64_json(value: &str) -> Option<Value> {
    let mut text: String = value.trim().replace('-', "+").replace('_', "/");
    if text.is_empty() {
        return None;
    }
    // Restore stripped padding (a remainder of 1 is never valid base64).
    match text.len() % 4 {
        2 => text.push_str("=="),
        3 => text.push('='),
        _ => {}
    }
    let bytes = STANDARD.decode(text).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn usd_to_atomic(usd: Decimal) -> u64 {
    (usd * USDC_UNITS).trunc().to_u64().unwrap_or(0)
}

fn requirement_amount(req: &Value) -> Option<&Value> {
    // v2 uses `amount`; v1 used `maxAmountRequired`.
    req.get("amount").or_else(|| req.get("maxAmountRequired"))
}

fn amount_atomic(v: &Value) -> Option<i128> {
    match v {
        Value::String(s) => s.trim().parse::<i128>().ok(),
        Value::Number(n) => n.as_i64().map(i128::from),
        _ => None,
    }
}

fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// `None` if the requirement is acceptable, else the reason it is refused.
pub fn refusal_reason(req: &Value, max_atomic: u64) -> Option<String> {
    if !req.is_object() {
        return Some("malformed requirement".into());
    }
    let network = req.get("network").and_then(|v| v.as_str()).unwrap_or("");
    if !BASE_NETWORKS.contains(&network) {
        return Some(format!("network {network:?} is not Base"));
    }
    let scheme = req.get("scheme").and_then(|v| v.as_str()).unwrap_or("");
    if scheme != "exact" {
        return Some(format!("scheme {scheme:?} is not exact"));
    }
    let asset = req.get("asset").and_then(|v| v.as_str()).unwrap_or("");
    if !asset.eq_ignore_ascii_case(BASE_USDC) {
        return Some(format!("asset {asset:?} is not Base USDC"));
    }
    let Some(amount) = requirement_amount(req).and_then(amount_atomic) else {
        return Some(format!("invalid amount {:?}", requirement_amount(req)));
    };
    if amount <= 0 {
        return Some(format!("amount {amount} must be positive"));
    }
    if amount > i128::from(max_atomic) {
        return Some(format!("amount {amount} exceeds cap {max_atomic}"));
    }
    let pay_to = req.get("payTo").and_then(|v| v.as_str()).unwrap_or("");
    if !is_address(pay_to) {
        return Some(format!("invalid payTo {pay_to:?}"));
    }
    None
}

/// First acceptable requirement from `accepts`, or why none is.
pub fn select_requirement(accepts: Option<&Value>, max_atomic: u64) -> Result<Value, PaymentError> {
    let list = accepts
        .and_then(|a| a.as_array())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| {
            PaymentError::Refused("402 response offered no payment requirements".into())
        })?;
    let mut reasons = Vec::new();
    for req in list {
        match refusal_reason(req, max_atomic) {
            None => return Ok(req.clone()),
            Some(r) => reasons.push(r),
        }
    }
    Err(PaymentError::Refused(reasons.join("; ")))
}

/// EIP-3009 `TransferWithAuthorization` message values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorization {
    pub from: String,
    pub to: String,
    pub value: u64,
    pub valid_after: u64,
    pub valid_before: u64,
    pub nonce: [u8; 32],
}

/// EIP-712 domain of the token contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenDomain {
    pub name: String,
    pub version: String,
    pub chain_id: u64,
    pub verifying_contract: String,
}

impl TokenDomain {
    pub fn from_requirement(req: &Value) -> Self {
        let extra = |k: &str| {
            req.pointer(&format!("/extra/{k}"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Self {
            name: extra("name").unwrap_or_else(|| "USD Coin".into()),
            version: extra("version").unwrap_or_else(|| "2".into()),
            chain_id: BASE_CHAIN_ID,
            verifying_contract: str_at(req, "/asset"),
        }
    }
}

/// Signs x402 payments. Implemented with alloy in signing builds; tests use a stub.
pub trait PaymentSigner: Send + Sync {
    fn address(&self) -> String;
    /// 32 fresh random bytes for the EIP-3009 nonce.
    fn fresh_nonce(&self) -> Result<[u8; 32], String>;
    /// `0x`-prefixed 65-byte signature over the EIP-712 hash.
    fn sign_transfer_authorization(
        &self,
        auth: &Authorization,
        domain: &TokenDomain,
    ) -> Result<String, String>;
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The x402 v2 payment payload (all authorization fields as strings).
pub fn build_payment_payload(
    req: &Value,
    resource: &Value,
    auth: &Authorization,
    signature: &str,
) -> Value {
    let signature = if signature.starts_with("0x") {
        signature.to_string()
    } else {
        format!("0x{signature}")
    };
    json!({
        "x402Version": 2,
        "resource": resource,
        "accepted": req,
        "payload": {
            "signature": signature,
            "authorization": {
                "from": auth.from,
                "to": auth.to,
                "value": auth.value.to_string(),
                "validAfter": auth.valid_after.to_string(),
                "validBefore": auth.valid_before.to_string(),
                "nonce": to_hex(&auth.nonce),
            }
        }
    })
}

/// Per-UTC-day spend tracker.
#[derive(Clone, Debug)]
pub struct DailyBudget {
    day: chrono::NaiveDate,
    spent_atomic: u64,
    limit_atomic: u64,
}

impl DailyBudget {
    pub fn new(limit_usd: Decimal) -> Self {
        Self {
            day: chrono::Utc::now().date_naive(),
            spent_atomic: 0,
            limit_atomic: usd_to_atomic(limit_usd),
        }
    }

    fn roll(&mut self, today: chrono::NaiveDate) {
        if today != self.day {
            self.day = today;
            self.spent_atomic = 0;
        }
    }

    pub fn spent_atomic(&self) -> u64 {
        self.spent_atomic
    }

    fn allows(&mut self, amount: u64, today: chrono::NaiveDate) -> bool {
        self.roll(today);
        self.spent_atomic.saturating_add(amount) <= self.limit_atomic
    }

    fn record(&mut self, amount: u64) {
        self.spent_atomic = self.spent_atomic.saturating_add(amount);
    }

    /// True when not even the smallest payment fits today.
    fn exhausted(&mut self, today: chrono::NaiveDate) -> bool {
        self.roll(today);
        self.spent_atomic >= self.limit_atomic
    }
}

/// One live poll: GET, and on 402 pay once (after every refusal check) and
/// retry once. Returns the reduced sample and the atomic USDC paid.
pub async fn fetch_live(
    http: &reqwest::Client,
    cfg: &ArbRaptorConfig,
    signer: &dyn PaymentSigner,
    budget: &mut DailyBudget,
) -> Result<(ArbSample, u64), PaymentError> {
    let transport = |e: reqwest::Error| PaymentError::Transport(e.to_string());
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    let query = cfg.query();
    let resp = http
        .get(&cfg.url)
        .query(&query)
        .timeout(timeout)
        .send()
        .await
        .map_err(transport)?;

    let mut paid = 0;
    let resp = if resp.status().as_u16() == 402 {
        let header = resp
            .headers()
            .get("PAYMENT-REQUIRED")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.text().await.unwrap_or_default();
        let required = header
            .as_deref()
            .and_then(decode_b64_json)
            .or_else(|| serde_json::from_str::<Value>(&body).ok())
            .filter(Value::is_object)
            .ok_or_else(|| {
                PaymentError::Refused("402 response without payment requirements".into())
            })?;
        let req = select_requirement(required.get("accepts"), cfg.max_atomic())?;
        let amount = requirement_amount(&req)
            .and_then(amount_atomic)
            .and_then(|a| u64::try_from(a).ok())
            .ok_or_else(|| PaymentError::Refused("invalid amount".into()))?;
        if !budget.allows(amount, chrono::Utc::now().date_naive()) {
            return Err(PaymentError::Budget(format!(
                "daily budget exhausted ({} + {amount} > {} atomic USDC)",
                budget.spent_atomic, budget.limit_atomic
            )));
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let timeout_secs = req
            .get("maxTimeoutSeconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(60);
        let auth = Authorization {
            from: signer.address(),
            to: str_at(&req, "/payTo"),
            value: amount,
            valid_after: now.saturating_sub(600),
            valid_before: now + timeout_secs,
            nonce: signer.fresh_nonce().map_err(PaymentError::Refused)?,
        };
        let signature = signer
            .sign_transfer_authorization(&auth, &TokenDomain::from_requirement(&req))
            .map_err(PaymentError::Refused)?;
        let resource = required
            .get("resource")
            .filter(|r| !r.is_null())
            .cloned()
            .unwrap_or_else(|| json!({ "url": cfg.url }));
        let header =
            encode_payment_header(&build_payment_payload(&req, &resource, &auth, &signature));
        let retry = http
            .get(&cfg.url)
            .query(&query)
            .header("PAYMENT-SIGNATURE", &header)
            .header("X-PAYMENT", &header)
            .timeout(timeout)
            .send()
            .await
            .map_err(transport)?;
        if retry.status().as_u16() == 402 {
            let body = retry.text().await.unwrap_or_default();
            return Err(PaymentError::Failed(truncate(&body, 200)));
        }
        // Counted once the signed payment was not rejected (conservative).
        budget.record(amount);
        paid = amount;
        retry
    } else {
        resp
    };

    let status = resp.status();
    let body = resp.text().await.map_err(transport)?;
    if !status.is_success() {
        return Err(PaymentError::Transport(format!(
            "HTTP {status}: {}",
            truncate(&body, 200)
        )));
    }
    let sample = reduce_feed(&body, false).map_err(PaymentError::Transport)?;
    Ok((sample, paid))
}

/// Publish the fictional demo snapshot (no network, no key). Returns what was sent.
pub fn publish_demo(tx: &watch::Sender<ArbSnapshot>, cfg: &ArbRaptorConfig) -> ArbSample {
    let sample = sample_feed(&cfg.q, cfg.limit, cfg.mode);
    // connected = false: consumers treat it as neutral, like any offline Raptor.
    let _ = tx.send(sample.snapshot(false));
    sample
}

/// Spawnable Raptor loop. Demo (offline, parks) unless `X402_WALLET_KEY` is set
/// and this build can sign; then polls the paid feed every `poll_secs`.
pub async fn run_kalshi_predictit_arb_raptor(
    http: Arc<reqwest::Client>,
    tx: watch::Sender<ArbSnapshot>,
    cfg: ArbRaptorConfig,
) {
    let key = std::env::var(WALLET_KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty());
    let signer: Option<Box<dyn PaymentSigner>> = match key.as_deref().map(signer_from_key) {
        None => None,
        Some(Ok(s)) => Some(s),
        Some(Err(e)) => {
            warn!("🛰️ Kalshi–PredictIt Arb Raptor: {WALLET_KEY_ENV} set but unusable ({e}) — staying in demo mode");
            None
        }
    };
    let Some(signer) = signer else {
        let sample = publish_demo(&tx, &cfg);
        info!(
            "🛰️ Kalshi–PredictIt Arb Raptor demo — {} not set: fictional SAMPLE data ({} pairs), no network, no cost",
            WALLET_KEY_ENV,
            sample.pairs.len()
        );
        std::future::pending::<()>().await;
        return;
    };

    info!(
        "🛰️ Kalshi–PredictIt Arb Raptor LIVE as {} — $0.02/call, cap ${}/call, ${}/day, every {}s",
        signer.address(),
        cfg.max_usd_per_call,
        cfg.daily_budget_usd,
        cfg.poll_secs
    );
    let mut budget = DailyBudget::new(cfg.daily_budget_usd);
    let mut last = ArbSnapshot::default();
    loop {
        if budget.exhausted(chrono::Utc::now().date_naive()) {
            debug!("🛰️ Kalshi–PredictIt Arb Raptor: daily budget spent; skipping poll");
            last.connected = false;
            let _ = tx.send(last);
        } else {
            match fetch_live(&http, &cfg, signer.as_ref(), &mut budget).await {
                Ok((sample, paid)) => {
                    last = sample.snapshot(true);
                    let _ = tx.send(last);
                    match sample.best() {
                        Some(best) => info!(
                            "🛰️ Kalshi–PredictIt Arb: {} pairs, {} executable, best {}¢ [{}] (paid {} atomic USDC)",
                            last.num_pairs, last.num_executable, last.best_net_yield_c, best.kalshi_ticker, paid
                        ),
                        None => debug!("🛰️ Kalshi–PredictIt Arb: no pairs returned (paid {paid})"),
                    }
                }
                Err(e) => {
                    last.connected = false;
                    let _ = tx.send(last);
                    warn!("⚠️ Kalshi–PredictIt Arb Raptor poll failed: {e} (signal treated as neutral)");
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(cfg.poll_secs.max(60))).await;
    }
}

/// Build a signer from a hex private key (signing builds only).
#[cfg(any(feature = "intl_clob", feature = "x402"))]
pub fn signer_from_key(key: &str) -> Result<Box<dyn PaymentSigner>, String> {
    Ok(Box::new(x402_signer::LocalX402Signer::from_key(key)?))
}

/// Without alloy there is no signer: live mode is unavailable in this build.
#[cfg(not(any(feature = "intl_clob", feature = "x402")))]
pub fn signer_from_key(_key: &str) -> Result<Box<dyn PaymentSigner>, String> {
    Err("this build has no x402 signer — rebuild with the `x402` feature".into())
}

#[cfg(any(feature = "intl_clob", feature = "x402"))]
pub mod x402_signer {
    //! EIP-712 signing of EIP-3009 `TransferWithAuthorization` with alloy —
    //! the same `Eip712Domain` + `SolStruct::eip712_signing_hash` pattern as
    //! `venues::intl::orders`.
    use std::borrow::Cow;

    use alloy::dyn_abi::Eip712Domain;
    use alloy::primitives::{keccak256, Address, B256, U256};
    use alloy::signers::local::PrivateKeySigner;
    use alloy::signers::SignerSync;
    use alloy::sol_types::SolStruct;

    use super::{Authorization, PaymentSigner, TokenDomain};

    alloy::sol! {
        struct TransferWithAuthorization {
            address from;
            address to;
            uint256 value;
            uint256 validAfter;
            uint256 validBefore;
            bytes32 nonce;
        }
    }

    /// EIP-712 signing hash for an authorization under a token domain.
    pub fn signing_hash(auth: &Authorization, domain: &TokenDomain) -> Result<B256, String> {
        let addr = |s: &str| {
            s.parse::<Address>()
                .map_err(|e| format!("bad address {s:?}: {e}"))
        };
        let message = TransferWithAuthorization {
            from: addr(&auth.from)?,
            to: addr(&auth.to)?,
            value: U256::from(auth.value),
            validAfter: U256::from(auth.valid_after),
            validBefore: U256::from(auth.valid_before),
            nonce: B256::from(auth.nonce),
        };
        let domain = Eip712Domain {
            name: Some(Cow::Owned(domain.name.clone())),
            version: Some(Cow::Owned(domain.version.clone())),
            chain_id: Some(U256::from(domain.chain_id)),
            verifying_contract: Some(addr(&domain.verifying_contract)?),
            ..Eip712Domain::default()
        };
        Ok(message.eip712_signing_hash(&domain))
    }

    /// Local (in-process) signer. The key is never logged or serialized.
    pub struct LocalX402Signer {
        inner: PrivateKeySigner,
    }

    impl LocalX402Signer {
        pub fn from_key(key: &str) -> Result<Self, String> {
            let inner = key
                .trim()
                .parse::<PrivateKeySigner>()
                .map_err(|_| "invalid private key".to_string())?;
            Ok(Self { inner })
        }
    }

    impl std::fmt::Debug for LocalX402Signer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "LocalX402Signer({})", self.inner.address())
        }
    }

    impl PaymentSigner for LocalX402Signer {
        fn address(&self) -> String {
            self.inner.address().to_checksum(None)
        }

        fn fresh_nonce(&self) -> Result<[u8; 32], String> {
            // `B256::random` needs an alloy-primitives feature this crate does not
            // enable; a throwaway key from `PrivateKeySigner::random` (seeded from
            // `rand::thread_rng`, a CSPRNG) hashed with keccak gives 32 uniform bytes.
            Ok(keccak256(PrivateKeySigner::random().to_bytes()).0)
        }

        fn sign_transfer_authorization(
            &self,
            auth: &Authorization,
            domain: &TokenDomain,
        ) -> Result<String, String> {
            let hash = signing_hash(auth, domain)?;
            let signature = self
                .inner
                .sign_hash_sync(&hash)
                .map_err(|e| format!("signing failed: {e}"))?;
            Ok(super::to_hex(&signature.as_bytes()))
        }
    }
}

// ── small JSON helpers ───────────────────────────────────────────────────────

fn str_at(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

/// Decimal from a JSON number via its textual form (no float rounding).
fn dec_at(v: &Value, pointer: &str) -> Option<Decimal> {
    let n = v.pointer(pointer)?.as_number()?.to_string();
    Decimal::from_str(&n)
        .or_else(|_| Decimal::from_scientific(&n))
        .ok()
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use axum::extract::{RawQuery, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use axum::response::IntoResponse;

    const PAY_TO: &str = "0xabababababababababababababababababababab";

    fn requirement() -> Value {
        json!({
            "scheme": "exact",
            "network": "eip155:8453",
            "amount": "20000",
            "asset": BASE_USDC,
            "payTo": PAY_TO,
            "maxTimeoutSeconds": 60,
            "extra": { "name": "USD Coin", "version": "2" }
        })
    }

    fn with(field: &str, value: Value) -> Value {
        let mut r = requirement();
        r[field] = value;
        r
    }

    fn required(accepts: Value) -> Value {
        json!({ "x402Version": 2, "resource": { "url": DEFAULT_URL }, "accepts": accepts })
    }

    fn live_body() -> Value {
        json!({
            "q": "senate",
            "mode": "opportunities",
            "opportunities": [{
                "event": "LIVE-TEST event",
                "kalshi": { "ticker": "LIVE-TEST-1" },
                "best_direction": { "direction": "YES_KALSHI__NO_PREDICTIT", "net_yield_c": 2.5, "net_yield_pct": 2.6 },
                "roc_annualized_pct": 4.4,
                "executable": true
            }],
            "stats": { "pairs_evaluated": 1, "executable": 1 },
            "fetched_at": "2026-01-01T00:00:00Z"
        })
    }

    fn b64(v: &Value, urlsafe: bool, pad: bool) -> String {
        use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE};
        let s = v.to_string();
        match (urlsafe, pad) {
            (true, true) => URL_SAFE.encode(s),
            (true, false) => URL_SAFE_NO_PAD.encode(s),
            (false, true) => STANDARD.encode(s),
            (false, false) => STANDARD_NO_PAD.encode(s),
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Variant {
        Header,
        UrlSafeHeader,
        Body,
    }

    struct Fake {
        required: Value,
        variant: Variant,
        always_402: bool,
        seen: Mutex<Vec<(HeaderMap, Option<String>)>>,
    }

    async fn fake_handler(
        State(fake): State<Arc<Fake>>,
        RawQuery(q): RawQuery,
        headers: HeaderMap,
    ) -> axum::response::Response {
        let paid = headers.contains_key("payment-signature");
        fake.seen.lock().unwrap().push((headers, q));
        if paid && !fake.always_402 {
            let mut h = HeaderMap::new();
            h.insert(
                "PAYMENT-RESPONSE",
                HeaderValue::from_str(&b64(&json!({"success": true}), false, true)).unwrap(),
            );
            return (StatusCode::OK, h, live_body().to_string()).into_response();
        }
        match fake.variant {
            Variant::Body => {
                (StatusCode::PAYMENT_REQUIRED, fake.required.to_string()).into_response()
            }
            v => {
                let urlsafe = v == Variant::UrlSafeHeader;
                let mut h = HeaderMap::new();
                h.insert(
                    "PAYMENT-REQUIRED",
                    HeaderValue::from_str(&b64(&fake.required, urlsafe, !urlsafe)).unwrap(),
                );
                (StatusCode::PAYMENT_REQUIRED, h, "").into_response()
            }
        }
    }

    /// Local fake x402 server on 127.0.0.1 — no external network.
    async fn serve(required: Value, variant: Variant, always_402: bool) -> (Arc<Fake>, String) {
        let fake = Arc::new(Fake {
            required,
            variant,
            always_402,
            seen: Mutex::new(Vec::new()),
        });
        let app = axum::Router::new()
            .route("/kalshi-predictit-arb", axum::routing::get(fake_handler))
            .with_state(Arc::clone(&fake));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (fake, format!("http://{addr}/kalshi-predictit-arb"))
    }

    /// Stub signer: fixed address/nonce, dummy signature, counts sign calls.
    #[derive(Default)]
    struct StubSigner {
        calls: AtomicUsize,
    }

    impl PaymentSigner for StubSigner {
        fn address(&self) -> String {
            "0x1111111111111111111111111111111111111111".into()
        }
        fn fresh_nonce(&self) -> Result<[u8; 32], String> {
            Ok([7u8; 32])
        }
        fn sign_transfer_authorization(
            &self,
            _: &Authorization,
            _: &TokenDomain,
        ) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!("0x{}", "00".repeat(65)))
        }
    }

    fn cfg(url: &str) -> ArbRaptorConfig {
        ArbRaptorConfig {
            url: url.into(),
            q: "senate".into(),
            timeout_secs: 5,
            ..Default::default()
        }
    }

    fn decode_header(h: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h).unwrap()).unwrap()
    }

    // ── fixture / parsing ────────────────────────────────────────────────────

    #[test]
    fn fixture_is_labelled_fictional() {
        let v: Value = serde_json::from_str(SAMPLE_JSON).unwrap();
        assert!(v["_notice"]
            .as_str()
            .unwrap()
            .contains("NOT real market data"));
        for row in v["opportunities"].as_array().unwrap() {
            assert!(row["event"].as_str().unwrap().starts_with("SAMPLE"));
        }
    }

    #[test]
    fn reduces_the_live_schema_and_tolerates_missing_fields() {
        let s = reduce_feed(SAMPLE_JSON, true).unwrap();
        assert_eq!(s.pairs.len(), 4);
        assert_eq!(s.pairs_evaluated, Some(4));
        let first = &s.pairs[0];
        assert_eq!(first.kalshi_ticker, "SAMPLE-GOVPARTYXX-26-R");
        assert_eq!(first.predictit_contract, "Republican");
        assert_eq!(first.direction, "YES_KALSHI__NO_PREDICTIT");
        assert_eq!(first.net_yield_c, Some(dec!(3.1)));
        assert_eq!(first.net_yield_pct, Some(dec!(3.26)));
        assert_eq!(first.days_to_settlement, Some(120));
        assert!(first.executable);
        let missing = &s.pairs[3];
        assert!(missing.event.starts_with("SAMPLE record"));
        assert_eq!(missing.kalshi_ticker, "");
        assert_eq!(missing.net_yield_c, None);
        assert!(!missing.executable);
        let best = s.best().unwrap();
        assert_eq!(best.kalshi_ticker, "SAMPLE-GOVPARTYXX-26-R");
    }

    #[test]
    fn legacy_pair_key_is_accepted() {
        let s = reduce_feed(
            r#"{"opportunities":[{"pair":"A <-> B","executable":true}]}"#,
            false,
        )
        .unwrap();
        assert_eq!(s.pairs[0].event, "A <-> B");
        assert!(s.pairs[0].executable);
    }

    #[test]
    fn event_is_preferred_over_pair() {
        let s = reduce_feed(
            r#"{"opportunities":[{"event":"E","pair":"P","executable":true}]}"#,
            false,
        )
        .unwrap();
        assert_eq!(s.pairs.len(), 1);
        assert_eq!(s.pairs[0].event, "E");
    }

    #[test]
    fn pair_is_used_when_event_is_missing_null_or_empty() {
        let body = r#"{"opportunities":[
            {"pair":"P1"},
            {"event":null,"pair":"P2"},
            {"event":"","pair":"P3"},
            {"event":7,"pair":"P4"}
        ]}"#;
        let labels: Vec<String> = reduce_feed(body, false)
            .unwrap()
            .pairs
            .into_iter()
            .map(|p| p.event)
            .collect();
        assert_eq!(labels, ["P1", "P2", "P3", "P4"]);
    }

    #[test]
    fn rows_without_event_or_pair_are_skipped() {
        let body = r#"{"opportunities":[
            {"executable":true,"best_direction":{"net_yield_c":9.0}},
            {"event":null,"pair":null,"executable":true},
            {"event":"","pair":"  ","executable":true},
            {"event":"KEEP","executable":true,"best_direction":{"net_yield_c":1.5}}
        ]}"#;
        let s = reduce_feed(body, false).unwrap();
        assert_eq!(s.pairs.len(), 1);
        assert_eq!(s.pairs[0].event, "KEEP");
        let snap = s.snapshot(true);
        assert_eq!(snap.num_pairs, dec!(1));
        assert_eq!(snap.num_executable, dec!(1));
        assert_eq!(snap.best_net_yield_c, dec!(1.5));
    }

    #[test]
    fn malformed_bodies_are_errors_not_panics() {
        assert!(reduce_feed("not json", false).is_err());
        assert!(reduce_feed(r#"{"unexpected": true}"#, false).is_err());
        assert!(reduce_feed("[1,2,3]", false).is_err());
        let err = reduce_feed(r#"{"error":"rate_limited"}"#, false).unwrap_err();
        assert!(err.contains("rate_limited"), "got: {err}");
    }

    #[test]
    fn sample_feed_filters_like_the_endpoint() {
        assert_eq!(sample_feed("", 10, FeedMode::Opportunities).pairs.len(), 2);
        assert_eq!(sample_feed("", 25, FeedMode::All).pairs.len(), 4);
        assert_eq!(sample_feed("Governor", 25, FeedMode::All).pairs.len(), 1);
        assert_eq!(sample_feed("", 1, FeedMode::All).pairs.len(), 1);
        assert_eq!(sample_feed("", 0, FeedMode::All).pairs.len(), 1); // clamped to 1
        assert!(sample_feed("no-such-market", 25, FeedMode::All)
            .pairs
            .is_empty());
    }

    #[test]
    fn demo_publishes_a_neutral_sample_snapshot_without_network() {
        let (tx, rx) = watch::channel(ArbSnapshot::default());
        let sample = publish_demo(&tx, &ArbRaptorConfig::default());
        assert!(sample.sample_data);
        let snap = *rx.borrow();
        assert!(snap.sample_data);
        assert!(!snap.connected, "demo must read as disconnected/neutral");
        assert_eq!(snap.num_pairs, dec!(2));
        assert_eq!(snap.num_executable, dec!(2));
        assert_eq!(snap.best_net_yield_c, dec!(3.1));
        assert_eq!(snap.best_roc_annualized_pct, dec!(9.9));
    }

    #[test]
    fn defaults_are_capped_and_cheap() {
        let c = ArbRaptorConfig::default();
        assert_eq!(c.max_usd_per_call, dec!(0.02));
        assert_eq!(c.max_atomic(), 20_000);
        assert_eq!(c.poll_secs, 3600);
        assert_eq!(usd_to_atomic(c.daily_budget_usd), 500_000);
        assert_eq!(c.url, DEFAULT_URL);
        let q = ArbRaptorConfig {
            limit: 99,
            ..Default::default()
        }
        .query();
        assert!(q.contains(&("limit", "25".to_string())));
        assert!(!q.iter().any(|(k, _)| *k == "q"));
    }

    // ── header codec ─────────────────────────────────────────────────────────

    #[test]
    fn outgoing_header_is_unpadded_base64url() {
        let payload = json!({ "x402Version": 2, "blob": "\u{ff}\u{fe}>>>???".repeat(7) });
        let h = encode_payment_header(&payload);
        assert!(
            !h.contains('+') && !h.contains('/') && !h.contains('='),
            "{h}"
        );
        assert_eq!(decode_b64_json(&h), Some(payload));
    }

    #[test]
    fn incoming_decode_is_tolerant() {
        let v = json!({ "accepts": [{ "x": "\u{ff}\u{fe}>>>???" }], "n": 1 });
        for urlsafe in [false, true] {
            for pad in [false, true] {
                assert_eq!(
                    decode_b64_json(&b64(&v, urlsafe, pad)),
                    Some(v.clone()),
                    "urlsafe={urlsafe} pad={pad}"
                );
            }
        }
        for bad in ["", "   ", "!!!not-base64!!!", "e30@@"] {
            assert_eq!(decode_b64_json(bad), None, "{bad:?}");
        }
    }

    // ── refusals ─────────────────────────────────────────────────────────────

    #[test]
    fn refusal_matrix() {
        let cases: Vec<(Value, &str)> = vec![
            (with("network", json!("eip155:1")), "not Base"),
            (with("network", json!("solana")), "not Base"),
            (with("scheme", json!("upto")), "not exact"),
            (
                with("asset", json!("0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd")),
                "not Base USDC",
            ),
            (with("amount", json!("20001")), "exceeds cap"),
            (with("amount", json!("1000000")), "exceeds cap"),
            (with("amount", json!("0")), "must be positive"),
            (with("amount", json!("-5")), "must be positive"),
            (with("amount", json!("abc")), "invalid amount"),
            (with("payTo", json!("0x123")), "invalid payTo"),
            (
                with("payTo", json!("0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")),
                "invalid payTo",
            ),
            (with("payTo", Value::Null), "invalid payTo"),
            (json!("not-an-object"), "malformed"),
        ];
        for (req, needle) in cases {
            let reason = refusal_reason(&req, 20_000).unwrap_or_default();
            assert!(
                reason.contains(needle),
                "{req} -> {reason:?} (want {needle})"
            );
        }
        assert_eq!(refusal_reason(&requirement(), 20_000), None);
        assert_eq!(
            refusal_reason(&with("network", json!("base")), 20_000),
            None
        );
        assert_eq!(refusal_reason(&with("amount", json!(20000)), 20_000), None);
        assert!(refusal_reason(&requirement(), 10_000)
            .unwrap()
            .contains("exceeds cap 10000"));
    }

    #[test]
    fn empty_or_missing_accepts_is_refused() {
        for accepts in [Some(json!([])), Some(json!({})), None] {
            let err = select_requirement(accepts.as_ref(), 20_000).unwrap_err();
            assert!(
                matches!(err, PaymentError::Refused(ref m) if m.contains("no payment requirements")),
                "{err}"
            );
        }
        let good = with("amount", json!("10000"));
        let picked = select_requirement(
            Some(&json!([with("network", json!("eip155:1")), good.clone()])),
            20_000,
        )
        .unwrap();
        assert_eq!(picked, good);
    }

    // ── 402 → sign → retry (local fake server) ───────────────────────────────

    #[tokio::test]
    async fn pays_once_and_retries_once() {
        for variant in [Variant::Header, Variant::UrlSafeHeader, Variant::Body] {
            let (fake, url) = serve(required(json!([requirement()])), variant, false).await;
            let signer = StubSigner::default();
            let mut budget = DailyBudget::new(dec!(0.50));
            let http = reqwest::Client::new();
            let (sample, paid) = fetch_live(&http, &cfg(&url), &signer, &mut budget)
                .await
                .unwrap();

            assert_eq!(paid, 20_000);
            assert_eq!(budget.spent_atomic(), 20_000);
            assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
            assert!(!sample.sample_data);
            assert_eq!(sample.pairs[0].kalshi_ticker, "LIVE-TEST-1");
            assert!(sample.snapshot(true).connected);

            let seen = fake.seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "exactly one retry");
            assert!(!seen[0].0.contains_key("payment-signature"));
            assert!(seen[1].1.as_deref().unwrap().contains("q=senate"));
            let h = seen[1].0["payment-signature"].to_str().unwrap().to_string();
            assert_eq!(seen[1].0["x-payment"].to_str().unwrap(), h);
            assert!(
                !h.contains('+') && !h.contains('/') && !h.contains('='),
                "{h}"
            );

            let p = decode_header(&h);
            assert_eq!(p["x402Version"], 2);
            assert_eq!(p["accepted"], requirement());
            assert_eq!(p["resource"], json!({ "url": DEFAULT_URL }));
            let a = &p["payload"]["authorization"];
            assert_eq!(a["from"], "0x1111111111111111111111111111111111111111");
            assert_eq!(a["to"], PAY_TO);
            assert_eq!(a["value"], "20000");
            assert_eq!(a["nonce"], format!("0x{}", "07".repeat(32)));
            let after: u64 = a["validAfter"].as_str().unwrap().parse().unwrap();
            let before: u64 = a["validBefore"].as_str().unwrap().parse().unwrap();
            assert_eq!(before - after, 660);
        }
    }

    #[tokio::test]
    async fn second_402_is_a_failure_not_a_loop() {
        let (fake, url) = serve(required(json!([requirement()])), Variant::Header, true).await;
        let mut budget = DailyBudget::new(dec!(0.50));
        let err = fetch_live(
            &reqwest::Client::new(),
            &cfg(&url),
            &StubSigner::default(),
            &mut budget,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PaymentError::Failed(_)), "{err}");
        assert_eq!(fake.seen.lock().unwrap().len(), 2);
        assert_eq!(budget.spent_atomic(), 0);
    }

    #[tokio::test]
    async fn refusals_happen_before_signing() {
        let bad = [
            json!([with("network", json!("eip155:1"))]),
            json!([with("scheme", json!("upto"))]),
            json!([with(
                "asset",
                json!("0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd")
            )]),
            json!([with("amount", json!("20001"))]),
            json!([with("payTo", json!("0x123"))]),
            json!([]),
        ];
        for accepts in bad {
            let (fake, url) = serve(required(accepts.clone()), Variant::Header, false).await;
            let signer = StubSigner::default();
            let err = fetch_live(
                &reqwest::Client::new(),
                &cfg(&url),
                &signer,
                &mut DailyBudget::new(dec!(0.50)),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, PaymentError::Refused(_)),
                "{accepts} -> {err}"
            );
            assert_eq!(signer.calls.load(Ordering::SeqCst), 0, "{accepts}");
            assert_eq!(fake.seen.lock().unwrap().len(), 1, "{accepts}");
        }
    }

    #[tokio::test]
    async fn lower_cap_refuses_the_default_price() {
        let (_fake, url) = serve(required(json!([requirement()])), Variant::Header, false).await;
        let c = ArbRaptorConfig {
            max_usd_per_call: dec!(0.01),
            ..cfg(&url)
        };
        let err = fetch_live(
            &reqwest::Client::new(),
            &c,
            &StubSigner::default(),
            &mut DailyBudget::new(dec!(0.50)),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, PaymentError::Refused(ref m) if m.contains("exceeds cap 10000")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn daily_budget_stops_further_payments() {
        let (_fake, url) = serve(required(json!([requirement()])), Variant::Header, false).await;
        let signer = StubSigner::default();
        let mut budget = DailyBudget::new(dec!(0.03));
        let http = reqwest::Client::new();
        fetch_live(&http, &cfg(&url), &signer, &mut budget)
            .await
            .unwrap();
        let err = fetch_live(&http, &cfg(&url), &signer, &mut budget)
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentError::Budget(_)), "{err}");
        assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
        assert!(!budget.exhausted(chrono::Utc::now().date_naive())); // 0.02 of 0.03 spent
        let tomorrow = chrono::Utc::now().date_naive() + chrono::Duration::days(1);
        assert!(
            budget.allows(20_000, tomorrow),
            "budget resets on a new UTC day"
        );
    }

    #[tokio::test]
    async fn non_402_success_is_free() {
        let app = axum::Router::new().route(
            "/f",
            axum::routing::get(|| async { live_body().to_string() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let signer = StubSigner::default();
        let (sample, paid) = fetch_live(
            &reqwest::Client::new(),
            &cfg(&url),
            &signer,
            &mut DailyBudget::new(dec!(0.5)),
        )
        .await
        .unwrap();
        assert_eq!(paid, 0);
        assert_eq!(sample.pairs.len(), 1);
        assert_eq!(signer.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(not(any(feature = "intl_clob", feature = "x402")))]
    #[test]
    fn builds_without_alloy_have_no_live_signer() {
        assert!(signer_from_key("0x01").is_err());
    }

    // ── real EIP-712 signing (throwaway key; signing builds only) ───────────

    #[cfg(any(feature = "intl_clob", feature = "x402"))]
    #[tokio::test]
    async fn signature_recovers_to_the_throwaway_address() {
        use alloy::primitives::Signature;
        use alloy::signers::local::PrivateKeySigner;

        // Fresh random key per run; never funded, never printed.
        let throwaway = PrivateKeySigner::random();
        let key = format!("0x{}", alloy::hex::encode(throwaway.to_bytes()));
        let signer = x402_signer::LocalX402Signer::from_key(&key).unwrap();
        assert_eq!(signer.address(), throwaway.address().to_checksum(None));
        assert!(!format!("{signer:?}").contains(&key[2..]));

        let (fake, url) = serve(
            required(json!([requirement()])),
            Variant::UrlSafeHeader,
            false,
        )
        .await;
        fetch_live(
            &reqwest::Client::new(),
            &cfg(&url),
            &signer,
            &mut DailyBudget::new(dec!(0.5)),
        )
        .await
        .unwrap();
        let h = fake.seen.lock().unwrap()[1].0["payment-signature"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(!h.contains('+') && !h.contains('/') && !h.contains('='));
        let p = decode_header(&h);
        let a = &p["payload"]["authorization"];
        let num = |k: &str| a[k].as_str().unwrap().parse::<u64>().unwrap();
        let nonce_hex = a["nonce"].as_str().unwrap();
        let mut nonce = [0u8; 32];
        nonce.copy_from_slice(&alloy::hex::decode(nonce_hex).unwrap());
        assert_ne!(nonce, [0u8; 32]);
        let auth = Authorization {
            from: a["from"].as_str().unwrap().into(),
            to: a["to"].as_str().unwrap().into(),
            value: num("value"),
            valid_after: num("validAfter"),
            valid_before: num("validBefore"),
            nonce,
        };
        let hash = x402_signer::signing_hash(&auth, &TokenDomain::from_requirement(&requirement()))
            .unwrap();
        let sig_hex = p["payload"]["signature"].as_str().unwrap();
        let sig_bytes = alloy::hex::decode(sig_hex).unwrap();
        assert_eq!(sig_bytes.len(), 65);
        let sig = Signature::try_from(sig_bytes.as_slice()).unwrap();
        assert_eq!(
            sig.recover_address_from_prehash(&hash).unwrap(),
            throwaway.address()
        );
    }

    #[cfg(any(feature = "intl_clob", feature = "x402"))]
    #[test]
    fn signing_hash_matches_the_eip712_spec_by_hand() {
        use alloy::primitives::{keccak256, Address, B256, U256};

        let auth = Authorization {
            from: "0x1111111111111111111111111111111111111111".into(),
            to: PAY_TO.into(),
            value: 20_000,
            valid_after: 1_700_000_000,
            valid_before: 1_700_000_660,
            nonce: [7u8; 32],
        };
        let domain = TokenDomain::from_requirement(&requirement());
        let word = |u: U256| u.to_be_bytes::<32>();
        let addr = |s: &str| {
            let mut w = [0u8; 32];
            w[12..].copy_from_slice(s.parse::<Address>().unwrap().as_slice());
            w
        };
        let domain_type = keccak256(
            "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let mut d = Vec::new();
        d.extend_from_slice(domain_type.as_slice());
        d.extend_from_slice(keccak256("USD Coin").as_slice());
        d.extend_from_slice(keccak256("2").as_slice());
        d.extend_from_slice(&word(U256::from(8453u64)));
        d.extend_from_slice(&addr(BASE_USDC));
        let type_hash = keccak256(
            "TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
        );
        let mut m = Vec::new();
        m.extend_from_slice(type_hash.as_slice());
        m.extend_from_slice(&addr(&auth.from));
        m.extend_from_slice(&addr(&auth.to));
        m.extend_from_slice(&word(U256::from(auth.value)));
        m.extend_from_slice(&word(U256::from(auth.valid_after)));
        m.extend_from_slice(&word(U256::from(auth.valid_before)));
        m.extend_from_slice(&auth.nonce);
        let mut digest = vec![0x19, 0x01];
        digest.extend_from_slice(keccak256(&d).as_slice());
        digest.extend_from_slice(keccak256(&m).as_slice());
        let expected: B256 = keccak256(&digest);
        assert_eq!(x402_signer::signing_hash(&auth, &domain).unwrap(), expected);
    }
}
