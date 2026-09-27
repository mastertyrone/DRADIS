// DRADIS — Polymarket US sports catalog
//
// This file is part of DRADIS and is licensed under the terms in LICENSE.

//! Polymarket US's implementation of [`SportsCatalog`].
//!
//! ## The venue that suits this design best
//!
//! Of the three venues, this is where a resting bid has the most going for it:
//! Polymarket US pays the maker a **rebate at trade** (−0.0125 against a 0.06 taker
//! coefficient), so Bookline's break-even sits around −0.81 points against −0.69 on
//! Polymarket International and only −0.56 on a two-cent Kalshi book. The maker
//! leg is not merely free here, it earns.
//!
//! ## Why the market records make this easier than Kalshi
//!
//! Kalshi forced two awkward inferences: kick-off had to be parsed out of the
//! ticker (and NFL tickers carry no time at all, which silently dropped the whole
//! league until it was found), and team names arrive truncated to things like
//! `"Chicago C"`, which prefix-match equally well against Cubs and White Sox.
//!
//! Polymarket US sends the facts outright. Captured live from
//! `aec-nfl-lac-ten-2025-11-02`, each `marketSides` entry carries:
//!
//! ```text
//! long: true   description "Chargers"  team.name "Los Angeles Chargers"  ordering "away"
//! long: false  description "Titans"    team.name "Tennessee Titans"      ordering "home"
//! ```
//!
//! So three things are read rather than inferred. **Which side pays on which team**
//! comes from the side's own `team` object, not from slug order or a polarity
//! convention — get that wrong and every line on the venue is inverted, with the
//! board pricing the favorite's consensus against the underdog's book.
//! **`team.name` is the full name**, which is what The Odds API uses, so matching
//! is more reliable here than on Kalshi rather than less. And **`gameStartTime` is
//! a real timestamp**, so there is no ticker to parse and no expiry-minus-three-hours
//! estimate.
//!
//! `team.league` also arrives as the shared lowercase code (`"nfl"`), so unlike
//! Kalshi's `KXNCAAFGAME` there is no alias table to keep in step.
//!
//! ## What is signed
//!
//! Every read on this venue is signed — discovery and settlement both. There is no
//! public endpoint to fall back on, which is why this catalog holds the
//! authenticated venue rather than a bare HTTP client.

use super::UsRetailVenue;
use crate::raptors::sports_ledger::{
    match_games, MatchedGame, OddsEvent, PmGame, PmOutcome, SportsCatalog,
};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

/// One side of a moneyline, as the gateway describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct MoneylineSide {
    /// The venue's leg symbol, `{slug}#long` or `{slug}#short`.
    pub leg_id: String,
    /// Full team name, e.g. "Los Angeles Chargers" — what The Odds API uses.
    pub team: String,
    /// `"home"` or `"away"` when the gateway says; carried for diagnostics only.
    pub ordering: Option<String>,
}

/// Read both sides of a moneyline out of a raw gateway market record.
///
/// Pure, so the one fact the adapter cannot afford to get wrong is testable against
/// the captured record. Returns `None` unless exactly two tradable instrument sides
/// carry a team — a three-way market, a prop, or a record missing team objects is
/// not a moneyline this viper can price, and inventing a side would invert the line.
pub fn moneyline_sides(market: &serde_json::Value) -> Option<[MoneylineSide; 2]> {
    let slug = market.get("slug")?.as_str()?;
    let sides = market.get("marketSides")?.as_array()?;
    let mut out: Vec<MoneylineSide> = Vec::new();
    for s in sides {
        // Instrument sides only: the gateway also sends non-instrument rows.
        if let Some(t) = s.get("marketSideType").and_then(|v| v.as_str()) {
            if t != "MARKET_SIDE_TYPE_INSTRUMENT" { continue; }
        }
        let team = s.get("team").and_then(|t| t.get("name")).and_then(|v| v.as_str())
            // `description` is the short form ("Chargers") and the fallback when a
            // record carries no structured team. Never the slug: two teams share it.
            .or_else(|| s.get("description").and_then(|v| v.as_str()))?;
        let long = s.get("long").and_then(|v| v.as_bool())?;
        out.push(MoneylineSide {
            leg_id: format!("{slug}#{}", if long { "long" } else { "short" }),
            team: team.to_string(),
            ordering: s.get("team").and_then(|t| t.get("ordering"))
                .and_then(|v| v.as_str()).map(str::to_string),
        });
    }
    // Exactly two, and on opposite legs. Two sides both claiming `long` is a record
    // shape this code does not understand, and guessing which is which is the error
    // that inverts a venue.
    if out.len() != 2 { return None; }
    let longs = out.iter().filter(|s| s.leg_id.ends_with("#long")).count();
    if longs != 1 { return None; }
    let [a, b]: [MoneylineSide; 2] = out.try_into().ok()?;
    Some([a, b])
}

/// The ledger's league code for a market record.
///
/// Straight from `team.league`, which already arrives as the shared lowercase code
/// (`"nfl"`). No alias table: unlike Kalshi, the venue and the knob agree.
pub fn league_of(market: &serde_json::Value) -> Option<String> {
    market.get("marketSides")?.as_array()?.iter()
        .find_map(|s| s.get("team")?.get("league")?.as_str())
        .map(|l| l.to_ascii_lowercase())
}

/// Kick-off, straight from `gameStartTime`.
///
/// A real timestamp, so unlike Kalshi there is nothing to parse out of an
/// identifier and no game-duration estimate. `None` when the gateway omits it, and
/// the game is then dropped rather than dated from `endDate` — that field is the
/// settlement date, days after the game, and using it would put every game outside
/// the matcher's tolerance.
pub fn kickoff_of(market: &serde_json::Value) -> Option<DateTime<Utc>> {
    let raw = market.get("gameStartTime")?.as_str()?;
    DateTime::parse_from_rfc3339(raw).ok().map(|d| d.with_timezone(&Utc))
}

/// Polymarket US's catalog: its signed market listing, its BBO, its settlement record.
///
/// Connects on FIRST USE rather than at construction. The ledger is spawned before
/// the trading venue exists, and `UsRetailVenue::connect` is async while the venue
/// selector is not — so rather than restructure startup, the catalog holds the
/// client and dials in the first time it is asked a question. A failure to connect
/// leaves the catalog answering nothing, which reads as zero coverage in the
/// telemetry rather than as a crash at boot.
pub struct UsSportsCatalog {
    pub http: Arc<reqwest::Client>,
    pub venue: tokio::sync::OnceCell<Arc<UsRetailVenue>>,
}

impl UsSportsCatalog {
    pub fn new(http: Arc<reqwest::Client>) -> Self {
        Self { http, venue: tokio::sync::OnceCell::new() }
    }

    /// The connected venue, or `None` when the gateway cannot be reached or the
    /// credentials are absent.
    async fn venue(&self) -> Option<&Arc<UsRetailVenue>> {
        self.venue.get_or_try_init(|| async {
            UsRetailVenue::connect(Arc::clone(&self.http)).await.map(Arc::new)
        }).await.ok()
    }

    /// Open sports moneylines, as raw gateway records.
    ///
    /// Raw rather than `UsMarketPair` because the pair type drops the two things
    /// this catalog needs: the per-side team objects and `gameStartTime`.
    async fn moneyline_records(&self) -> Vec<serde_json::Value> {
        let Some(venue) = self.venue().await else {
            tracing::warn!("🏈 Polymarket US sports: no venue connection — recording nothing");
            return Vec::new();
        };
        let path = "/v1/markets";
        let url = format!(
            "{}{}?categories=sports&limit=200&page=1&closed=false",
            venue.base_url_for_sports(), path,
        );
        let signed = venue.auth_for_sports().signed_headers("GET", path);
        let Ok(resp) = venue.http_for_sports()
            .get(&url)
            .header(signed[0].0, &signed[0].1)
            .header(signed[1].0, &signed[1].1)
            .header(signed[2].0, &signed[2].1)
            .header("Content-Type", "application/json")
            .send().await
        else {
            tracing::warn!("🏈 Polymarket US sports discovery failed");
            return Vec::new();
        };
        let Ok(text) = resp.text().await else { return Vec::new() };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
        v.get("markets").and_then(|m| m.as_array()).cloned().unwrap_or_default()
            .into_iter()
            .filter(|m| m.get("marketType").and_then(|t| t.as_str())
                .is_some_and(|t| t.eq_ignore_ascii_case("moneyline")))
            .collect()
    }
}

#[async_trait::async_trait]
impl SportsCatalog for UsSportsCatalog {
    async fn games(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        leagues: &[(String, String)],
        _now: DateTime<Utc>,
    ) -> (Vec<MatchedGame>, Option<i64>) {
        let records = self.moneyline_records().await;
        let mut by_league: HashMap<String, Vec<PmGame>> = HashMap::new();
        for m in &records {
            let (Some(sides), Some(league), Some(commence)) =
                (moneyline_sides(m), league_of(m), kickoff_of(m)) else { continue };
            let slug = m.get("slug").and_then(|s| s.as_str()).unwrap_or_default().to_string();
            by_league.entry(league.clone()).or_default().push(PmGame {
                league,
                slug: slug.clone(),
                title: m.get("question").and_then(|q| q.as_str()).unwrap_or_default().to_string(),
                start: commence,
                outcomes: sides.iter().map(|s| PmOutcome {
                    label: s.team.clone(),
                    condition_id: slug.clone(),
                    token_id: s.leg_id.clone(),
                }).collect(),
            });
        }

        let mut out = Vec::new();
        let mut remaining = None;
        // Per-league coverage, for the reason both other catalogs log it: a league
        // that MATCHES nothing is indistinguishable from a league with no games once
        // summed into a total, and on Kalshi that hid two bugs for a day.
        let mut coverage: Vec<String> = Vec::new();
        for (code, sport_key) in leagues {
            let Some(games) = by_league.get(code) else {
                coverage.push(format!("{code} n/a (no Polymarket US moneylines)"));
                continue;
            };
            let (events, rem) =
                crate::raptors::sports_ledger::odds_events_for_sport(http, api_key, sport_key).await;
            if rem.is_some() { remaining = rem; }
            let events: Vec<OddsEvent> = events;
            let matched = match_games(games, &events, sport_key);
            coverage.push(format!("{code} {}/{}", matched.len(), games.len()));
            out.extend(matched);
        }
        if !coverage.is_empty() {
            tracing::info!(
                "🏈 Sports ledger coverage (matched/Polymarket US games): {}",
                coverage.join(" · "),
            );
        }
        (out, remaining)
    }

    async fn best_levels(
        &self,
        _http: &reqwest::Client,
        leg_id: &str,
    ) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
        // One BBO call per leg, which is how `best_ask` already reads this venue.
        let Some(venue) = self.venue().await else { return (None, None, None, None) };
        match venue.bbo_for_sports(leg_id).await {
            Some((bid, bid_sz, ask, ask_sz)) => (bid, bid_sz, ask, ask_sz),
            None => (None, None, None, None),
        }
    }

    async fn settled_prices(
        &self,
        _http: &reqwest::Client,
        _market_key: &str,
        legs: &[String],
    ) -> HashMap<String, f64> {
        use crate::venues::core::TokenResolution as R;
        let mut out = HashMap::new();
        let Some(venue) = self.venue().await else { return out };
        for leg in legs {
            // Only a decisive venue answer books. The gateway pins a resolved
            // market's side price to exactly "1" or "0"; anything else is no answer.
            if let R::Resolved(px) = venue.settlement_for_sports(leg).await {
                if let Ok(v) = px.to_string().parse::<f64>() { out.insert(leg.clone(), v); }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The captured live record, verbatim in the fields that matter.
    ///
    /// From `aec-nfl-lac-ten-2025-11-02`, read off the gateway on 2026-09-27. This
    /// is the record the adapter was written against rather than guessed at.
    fn captured_moneyline() -> serde_json::Value {
        serde_json::json!({
            "slug": "aec-nfl-lac-ten-2025-11-02",
            "question": "Chargers vs. Titans",
            "marketType": "moneyline",
            "gameStartTime": "2025-11-02T18:00:00Z",
            "endDate": "2025-11-03T18:00:00Z",
            "category": "sports",
            "marketSides": [
                {
                    "description": "Chargers", "long": true, "id": "1",
                    "marketSideType": "MARKET_SIDE_TYPE_INSTRUMENT",
                    "team": { "name": "Los Angeles Chargers", "alias": "Chargers",
                              "abbreviation": "lac", "league": "nfl", "ordering": "away" }
                },
                {
                    "description": "Titans", "long": false, "id": "2",
                    "marketSideType": "MARKET_SIDE_TYPE_INSTRUMENT",
                    "team": { "name": "Tennessee Titans", "alias": "Titans",
                              "abbreviation": "ten", "league": "nfl", "ordering": "home" }
                }
            ]
        })
    }

    /// The one fact that inverts a venue if wrong: which leg pays on which team.
    ///
    /// Read off each side's own `team` object rather than inferred from slug order.
    /// The slug here is `lac-ten` and `long` pays on the Chargers (lac, away) — so a
    /// "first team in the slug is long" convention would happen to work on this
    /// record and is exactly the kind of coincidence that hides an inversion.
    #[test]
    fn each_leg_is_keyed_to_its_own_team_not_to_slug_order() {
        let sides = moneyline_sides(&captured_moneyline()).expect("a moneyline");
        let find = |leg: &str| sides.iter().find(|s| s.leg_id == leg).map(|s| s.team.clone());

        assert_eq!(find("aec-nfl-lac-ten-2025-11-02#long").as_deref(), Some("Los Angeles Chargers"));
        assert_eq!(find("aec-nfl-lac-ten-2025-11-02#short").as_deref(), Some("Tennessee Titans"));

        // Full names, because that is what The Odds API matches against — the short
        // `description` ("Chargers") is only the fallback.
        assert!(sides.iter().all(|s| s.team.contains(' ')), "full team names: {sides:?}");
        // Ordering is carried but never load-bearing.
        assert_eq!(sides[0].ordering.as_deref(), Some("away"));
    }

    /// A record this code does not understand is refused, never half-read.
    #[test]
    fn a_record_that_is_not_a_two_sided_moneyline_is_refused() {
        // Three sides: a drawable market is not a two-way moneyline.
        let mut three = captured_moneyline();
        let extra = serde_json::json!({
            "description": "Draw", "long": false, "marketSideType": "MARKET_SIDE_TYPE_INSTRUMENT",
            "team": { "name": "Draw", "league": "nfl" }
        });
        three["marketSides"].as_array_mut().unwrap().push(extra);
        assert!(moneyline_sides(&three).is_none(), "three outcomes is not this instrument");

        // Both sides claiming `long` is a shape we do not understand; guessing which
        // is which is the error that inverts a venue.
        let mut both_long = captured_moneyline();
        both_long["marketSides"][1]["long"] = serde_json::json!(true);
        assert!(moneyline_sides(&both_long).is_none());

        // No team and no description: nothing to match on.
        let mut nameless = captured_moneyline();
        nameless["marketSides"][0]["team"] = serde_json::json!({});
        nameless["marketSides"][0].as_object_mut().unwrap().remove("description");
        assert!(moneyline_sides(&nameless).is_none());

        assert!(moneyline_sides(&serde_json::json!({})).is_none());
    }

    /// Kick-off is a real timestamp here, and `endDate` must never stand in for it.
    #[test]
    fn kickoff_comes_from_game_start_time_only() {
        let m = captured_moneyline();
        assert_eq!(kickoff_of(&m).map(|t| t.to_rfc3339()).as_deref(),
                   Some("2025-11-02T18:00:00+00:00"));

        // `endDate` is the settlement date a day later. Falling back to it would put
        // every game outside the matcher's 15-minute tolerance, matching nothing.
        let mut no_start = m.clone();
        no_start.as_object_mut().unwrap().remove("gameStartTime");
        assert_eq!(kickoff_of(&no_start), None, "dropped, not dated from endDate");
    }

    /// The league code arrives already normalized, unlike Kalshi's.
    #[test]
    fn the_league_code_is_the_shared_one() {
        assert_eq!(league_of(&captured_moneyline()).as_deref(), Some("nfl"));
        assert_eq!(league_of(&serde_json::json!({"marketSides": []})), None);
    }
}
