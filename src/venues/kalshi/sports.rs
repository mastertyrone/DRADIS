// DRADIS — Kalshi sports catalog
//
// This file is part of DRADIS and is licensed under the terms in LICENSE.

//! Kalshi's implementation of [`SportsCatalog`]: where its game markets are, what
//! their touch is, and how it says a game ended.
//!
//! ## What is different about Kalshi's game shape
//!
//! Polymarket publishes one market per game with a token per outcome. Kalshi
//! publishes one market per TEAM, grouped under an event: the event
//! `KXMLBGAME-26SEP261915CHCBOS` carries `…-CHC` ("Chicago C wins") and `…-BOS`,
//! each an independent Yes/No binary. So one game is two markets and four legs, and
//! a squadron deployed on either market needs lines for both of its own legs — the
//! team's own `#yes` and, as the mirror, the opponent via `#no`. Without the mirror
//! a squadron that landed on the underdog's market would hold only a low-consensus
//! YES line, which the favorite floor correctly refuses, and Bookline would idle
//! for reasons that look like a bug.
//!
//! Three further quirks, each of which has bitten something:
//!
//! * **Team names are truncated.** `yes_sub_title` is "Chicago C", not "Chicago
//!   Cubs". The ledger's matcher prefix-matches tokens of four characters or more,
//!   so "Chicago C" scores a perfect match against "Chicago Cubs" — and equally
//!   against "Chicago White Sox". Kick-off is what disambiguates same-city games,
//!   which is why it is parsed from the ticker rather than taken from `close_time`.
//! * **`close_time` is not kick-off.** It is the trading close, which on these
//!   series sits hours after the game. The start time is encoded in the ticker
//!   (`26SEP261915` = 2026-09-26 19:15 ET) and nowhere else in the record.
//! * **Cancellation is not a tie.** Polymarket settles a voided game at $0.50;
//!   Kalshi resolves it "to a fair price" and reports neither `yes` nor `no`, so
//!   `settlement_resolution` answers `Unknown` forever. A simulated position on a
//!   voided game therefore needs a give-up bound rather than a resolution.

use super::KalshiVenue;
use crate::raptors::sports_ledger::{
    match_games, MatchedGame, MatchedOutcome, OddsEvent, PmGame, PmOutcome, SportsCatalog,
};
use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashMap;

/// Kick-off from a Kalshi game ticker.
///
/// The event ticker embeds the start as `YYMMMDDHHMM` in US Eastern —
/// `KXMLBGAME-26SEP261915CHCBOS` is 2026-09-26 19:15 ET. Eastern rather than UTC
/// because that is what the exchange's own `rules_primary` text states for the same
/// market ("Sep 26, 2026 at 7:15 PM EDT"), and because reading it as UTC would put
/// every game four or five hours early and break the matcher's 15-minute tolerance
/// against The Odds API.
///
/// Returns `None` on anything that does not parse, because a game with an unknown
/// start cannot be matched and must be dropped rather than guessed at.
pub fn kickoff_from_ticker(ticker: &str) -> Option<DateTime<Utc>> {
    // The stamp is the run of digits-and-letters after the first '-', up to the
    // team suffix: 2 digits year, 3 letters month, 2 digits day, 4 digits time.
    let rest = ticker.split_once('-')?.1;
    let b = rest.as_bytes();
    if b.len() < 11 { return None; }
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    let (yy, mon, dd, hhmm) = (&rest[0..2], &rest[2..5], &rest[5..7], &rest[7..11]);
    if !digits(yy) || !digits(dd) || !digits(hhmm) { return None; }
    let month = match mon.to_ascii_uppercase().as_str() {
        "JAN" => 1, "FEB" => 2, "MAR" => 3, "APR" => 4, "MAY" => 5, "JUN" => 6,
        "JUL" => 7, "AUG" => 8, "SEP" => 9, "OCT" => 10, "NOV" => 11, "DEC" => 12,
        _ => return None,
    };
    let year = 2000 + yy.parse::<i32>().ok()?;
    let day = dd.parse::<u32>().ok()?;
    let hour = hhmm[0..2].parse::<u32>().ok()?;
    let minute = hhmm[2..4].parse::<u32>().ok()?;
    chrono_tz::America::New_York
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .map(|t| t.with_timezone(&Utc))
}

/// The ledger's lowercase league code for a Kalshi game series ticker.
///
/// Normalized to the SAME codes Polymarket uses (`mlb`, `nfl`, …) so the
/// `sports_ledger_leagues` knob, `SportsLine.league` and every consumer stay
/// venue-neutral. A second league vocabulary per venue would mean a second knob and
/// a second place for them to disagree.
pub fn league_from_series(series: &str) -> Option<String> {
    let s = series.to_ascii_uppercase();
    let core = s.strip_prefix("KX")?;
    for suffix in ["GAME", "MATCH", "FIGHT"] {
        if let Some(code) = core.strip_suffix(suffix) {
            if code.is_empty() { return None; }
            let native = code.to_ascii_lowercase();
            // Kalshi's own league abbreviations are not always the ones the shared
            // `sports_ledger_leagues` knob uses, and a mismatch is silent: the
            // league simply never appears in `by_league`, so it reads as "no games"
            // rather than as a naming gap. College football was exactly this —
            // `KXNCAAFGAME` yields `ncaaf` while the knob has always said `cfb`, so
            // it could never have matched a single game.
            return Some(match native.as_str() {
                "ncaaf" => "cfb".to_string(),
                "ncaamb" => "cbb".to_string(),
                other => other.to_string(),
            });
        }
    }
    None
}

/// How long after kick-off a Kalshi game market is expected to expire.
///
/// Used only for tickers that carry no time. Observed on live records: the NFL
/// game `KXNFLGAME-26OCT05ATLNO` expires `2026-10-06T03:15Z`, three hours after a
/// 20:15 ET Sunday kick-off, and MLB's stamped tickers agree with the same offset.
///
/// An estimate, and the coverage line is how a league where it is wrong becomes
/// visible: the matcher tolerates 15 minutes, so a sport whose games do not run
/// about three hours would show as matched 0 of N rather than failing silently.
const EXPIRY_AFTER_KICKOFF_SECS: i64 = 3 * 3600;

/// Kick-off for one Kalshi game market.
///
/// Prefers the ticker's own stamp, which is exact — but only some series carry one.
/// MLB writes `26SEP261915CHCBOS` (date, time, teams) while NFL writes
/// `26OCT05ATLNO` (date, teams). Reading the NFL form with the MLB parser takes
/// `ATLN` as `HHMM`, fails, and drops the ENTIRE league: five NFL games were open
/// on 2026-09-27 and not one reached the board, which read identically to "the NFL
/// has no games today".
pub fn kickoff_for(event_ticker: &str, expected_expiration: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    kickoff_from_ticker(event_ticker).or_else(|| {
        expected_expiration.map(|e| e - chrono::Duration::seconds(EXPIRY_AFTER_KICKOFF_SECS))
    })
}

/// Add each market's mirror leg to a matched game.
///
/// A Kalshi game arrives as two independent team markets, so matching pairs the two
/// `#yes` legs and stops there. Every squadron, though, is deployed on ONE of those
/// markets and reads both of its own legs — so the opponent's line has to exist
/// under this market's `#no` or half of every game is unpriceable. A squadron on the
/// underdog's market would otherwise hold only a low-consensus YES line that the
/// favorite floor refuses, and the viper would idle for a reason that looks like a
/// bug rather than a missing row.
pub fn with_mirror_legs(mut game: MatchedGame) -> MatchedGame {
    let originals = game.outcomes.clone();
    for (i, o) in originals.iter().enumerate() {
        // The other team of a two-outcome game. Draw markets are not game
        // moneylines on these series, so there is exactly one opponent.
        let Some(opponent) = originals.iter().enumerate().find(|(j, _)| *j != i).map(|(_, x)| x) else {
            continue;
        };
        let Some(ticker) = o.pm.token_id.rsplit_once('#').map(|(t, _)| t) else { continue };
        game.outcomes.push(MatchedOutcome {
            pm: PmOutcome {
                label: opponent.pm.label.clone(),
                condition_id: o.pm.condition_id.clone(),
                token_id: super::leg_id(ticker, false),
            },
            odds_name: opponent.odds_name.clone(),
        });
    }
    game
}

/// Kalshi's catalog: its own game series for the slate, its order book for the
/// touch, and the exchange's settlement record for results.
pub struct KalshiSportsCatalog {
    pub venue: std::sync::Arc<KalshiVenue>,
}

#[async_trait::async_trait]
impl SportsCatalog for KalshiSportsCatalog {
    async fn games(
        &self,
        http: &reqwest::Client,
        api_key: &str,
        leagues: &[(String, String)],
        // Kalshi's own catalog carries each game's start in its ticker, so the
        // ledger's clock is not needed to decide what is in the slate.
        _now: DateTime<Utc>,
    ) -> (Vec<MatchedGame>, Option<i64>) {
        let markets = super::super::kalshi::trader::open_sports_game_markets(&self.venue).await;
        // Group the per-team markets back into games by their event ticker.
        let mut by_event: HashMap<String, Vec<&crate::venues::kalshi::types::KalshiMarket>> = HashMap::new();
        for m in &markets {
            if m.event_ticker.is_empty() || m.yes_sub_title.is_empty() { continue; }
            by_event.entry(m.event_ticker.clone()).or_default().push(m);
        }

        let mut by_league: HashMap<String, Vec<PmGame>> = HashMap::new();
        for (event, legs) in by_event {
            // Exactly two team markets is a moneyline. Anything else on a game
            // series is a prop or a three-way and not this instrument.
            if legs.len() != 2 { continue; }
            let expiry = legs.iter().find_map(|m| {
                chrono::DateTime::parse_from_rfc3339(&m.expected_expiration_time)
                    .ok().map(|d| d.with_timezone(&Utc))
            });
            let Some(commence) = kickoff_for(&event, expiry) else { continue };
            let Some(league) = league_from_series(event.split_once('-').map(|(s, _)| s).unwrap_or(&event))
            else { continue };
            let outcomes: Vec<PmOutcome> = legs.iter().map(|m| PmOutcome {
                label: m.yes_sub_title.clone(),
                condition_id: event.clone(),
                token_id: super::leg_id(&m.ticker, true),
            }).collect();
            by_league.entry(league.clone()).or_default().push(PmGame {
                league,
                slug: event.clone(),
                title: legs[0].title.clone(),
                start: commence,
                outcomes,
            });
        }

        // The Odds API half and the matcher are venue-neutral and shared.
        let mut out = Vec::new();
        let mut remaining = None;
        // Per-league coverage, for the reason the intl catalog logs it: a league
        // that MATCHES nothing is indistinguishable from a league with no games
        // once both are summed into a total. Without this line, "only MLB is on
        // the board" could mean the matcher is broken on college football or that
        // no college football was in the window, and the operator cannot tell
        // which. Observed 2026-09-27: exactly that question, unanswerable.
        let mut coverage: Vec<String> = Vec::new();
        for (code, sport_key) in leagues {
            let Some(games) = by_league.get(code) else {
                coverage.push(format!("{code} n/a (no Kalshi game series)"));
                continue;
            };
            let (events, rem) =
                crate::raptors::sports_ledger::odds_events_for_sport(http, api_key, sport_key).await;
            if rem.is_some() { remaining = rem; }
            let events: Vec<OddsEvent> = events;
            let matched = match_games(games, &events, sport_key);
            coverage.push(format!("{code} {}/{}", matched.len(), games.len()));
            for g in matched {
                out.push(with_mirror_legs(g));
            }
        }
        if !coverage.is_empty() {
            tracing::info!(
                "🏈 Sports ledger coverage (matched/Kalshi games): {}",
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
        let (ticker, yes) = super::split_market_id(leg_id);
        match self.venue.orderbook(&ticker).await {
            Ok(b) => {
                // The exchange quotes each side as a BID; the other side's ask is
                // one minus it, which is what `best_yes_ask` already encodes.
                let f = |o: Option<(rust_decimal::Decimal, rust_decimal::Decimal)>| {
                    use rust_decimal::prelude::ToPrimitive;
                    o.map(|(p, q)| (p.to_f64().unwrap_or(0.0), q.to_f64().unwrap_or(0.0)))
                };
                let (bid, ask) = if yes {
                    (f(b.best_yes_bid()), f(b.best_yes_ask()))
                } else {
                    // A NO leg's ask is one minus the best YES bid, the mirror of
                    // `best_yes_ask`.
                    let no_ask = b.best_yes_bid()
                        .map(|(p, q)| (rust_decimal::Decimal::ONE - p, q));
                    (f(b.best_no_bid()), f(no_ask))
                };
                (bid.map(|x| x.0), bid.map(|x| x.1), ask.map(|x| x.0), ask.map(|x| x.1))
            }
            Err(_) => (None, None, None, None),
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
        for leg in legs {
            // Only a decisive exchange answer books. A voided game answers neither
            // yes nor no forever, which is why a filled simulated position needs a
            // give-up bound rather than waiting on this.
            if let R::Resolved(px) = self.venue.settlement_resolution(leg).await {
                if let Some(v) = px.to_string().parse::<f64>().ok() { out.insert(leg.clone(), v); }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kick-off comes from the ticker, in Eastern, because it is nowhere else in
    /// the record and `close_time` is the trading close hours later.
    #[test]
    fn kickoff_is_parsed_from_the_ticker_in_eastern() {
        // The live event Fable captured: Cubs at Red Sox, 7:15 PM EDT.
        let t = kickoff_from_ticker("KXMLBGAME-26SEP261915CHCBOS").expect("parses");
        assert_eq!(t.to_rfc3339(), "2026-09-26T23:15:00+00:00", "19:15 EDT is 23:15Z");

        // Winter reads as EST, an hour further from UTC. Getting this wrong by an
        // hour would break the matcher's 15-minute tolerance on every game.
        let w = kickoff_from_ticker("KXNBAGAME-26JAN151930BOSNYK").expect("parses");
        assert_eq!(w.to_rfc3339(), "2026-01-16T00:30:00+00:00", "19:30 EST is 00:30Z next day");

        // Anything unparseable is dropped, never guessed: a game with an unknown
        // start cannot be matched at all.
        assert!(kickoff_from_ticker("KXMLBGAME").is_none(), "no stamp");
        assert!(kickoff_from_ticker("KXMLBGAME-26XXX261915CHCBOS").is_none(), "bad month");
        assert!(kickoff_from_ticker("KXMLBGAME-XX SEP261915").is_none(), "bad year");
        assert!(kickoff_from_ticker("").is_none());
    }

    /// A ticker with no time must still yield a kick-off, or the whole league drops.
    ///
    /// MLB stamps `26SEP261915CHCBOS` (date, time, teams); NFL stamps
    /// `26OCT05ATLNO` (date, teams). Reading the NFL form with the MLB parser takes
    /// `ATLN` as `HHMM` and fails, and because a game with no start cannot be
    /// matched, the league never enters the board at all. On 2026-09-27 five NFL
    /// games were open and none reached the board, which looked exactly like "no NFL
    /// games today".
    #[test]
    fn a_timeless_ticker_falls_back_to_the_expiry() {
        use chrono::Duration;
        // A stamped ticker is exact and the expiry is not consulted.
        let exact = kickoff_for("KXMLBGAME-26SEP261915CHCBOS", Some(Utc::now()));
        assert_eq!(exact.map(|t| t.to_rfc3339()).as_deref(), Some("2026-09-26T23:15:00+00:00"));

        // A timeless ticker derives kick-off from the expiry, three hours back.
        let expiry = chrono::DateTime::parse_from_rfc3339("2026-10-06T03:15:00Z")
            .unwrap().with_timezone(&Utc);
        let derived = kickoff_for("KXNFLGAME-26OCT05ATLNO", Some(expiry)).expect("derived");
        assert_eq!(derived, expiry - Duration::hours(3), "20:15 ET Sunday");
        // And it lands on the date the ticker names, which is the sanity check that
        // the three-hour offset is the right shape.
        assert_eq!(derived.to_rfc3339(), "2026-10-06T00:15:00+00:00");

        // Neither source means no game: dropped, never guessed.
        assert_eq!(kickoff_for("KXNFLGAME-26OCT05ATLNO", None), None);
    }

    /// Kalshi's league abbreviations are not always the shared knob's, and a
    /// mismatch is silent — the league simply never appears, reading as "no games".
    #[test]
    fn kalshi_league_codes_are_aliased_onto_the_shared_ones() {
        // The bug: the knob has always said `cfb`, Kalshi says NCAAF.
        assert_eq!(league_from_series("KXNCAAFGAME").as_deref(), Some("cfb"));
        assert_eq!(league_from_series("KXNCAAMBGAME").as_deref(), Some("cbb"));
        // The ones that already agreed must not move.
        for (series, code) in [("KXMLBGAME", "mlb"), ("KXNFLGAME", "nfl"),
                               ("KXNBAGAME", "nba"), ("KXNHLGAME", "nhl"),
                               ("KXEPLGAME", "epl"), ("KXMLSGAME", "mls"),
                               ("KXUCLGAME", "ucl")] {
            assert_eq!(league_from_series(series).as_deref(), Some(code), "{series}");
        }
    }

    /// League codes are normalized to the ones Polymarket already uses, so the
    /// leagues knob and every consumer stay venue-neutral.
    #[test]
    fn league_codes_are_the_shared_lowercase_ones() {
        assert_eq!(league_from_series("KXMLBGAME").as_deref(), Some("mlb"));
        assert_eq!(league_from_series("KXNFLGAME").as_deref(), Some("nfl"));
        assert_eq!(league_from_series("KXEPLGAME").as_deref(), Some("epl"));
        assert_eq!(league_from_series("KXATPMATCH").as_deref(), Some("atp"));
        assert_eq!(league_from_series("KXUFCFIGHT").as_deref(), Some("ufc"));
        // Not a game series, so not a sports catalog entry.
        assert_eq!(league_from_series("KXBTC15M"), None);
        assert_eq!(league_from_series("MLBGAME"), None, "no KX prefix");
        assert_eq!(league_from_series("KXGAME"), None, "no league left after the suffix");
    }

    /// Every squadron is on ONE team's market and reads both of its own legs, so
    /// the opponent must exist under this market's `#no`.
    #[test]
    fn each_market_gains_its_opponents_mirror_leg() {
        let outcome = |label: &str, ticker: &str| MatchedOutcome {
            pm: PmOutcome {
                label: label.into(),
                condition_id: "KXMLBGAME-26SEP261915CHCBOS".into(),
                token_id: format!("{ticker}#yes"),
            },
            odds_name: format!("{label} FC"),
        };
        let game = MatchedGame {
            league: "mlb".into(),
            sport_key: "baseball_mlb".into(),
            odds_event_id: "e1".into(),
            pm_slug: "KXMLBGAME-26SEP261915CHCBOS".into(),
            commence: Utc::now(),
            outcomes: vec![
                outcome("Chicago C", "KXMLBGAME-26SEP261915CHCBOS-CHC"),
                outcome("Boston", "KXMLBGAME-26SEP261915CHCBOS-BOS"),
            ],
        };
        let mirrored = with_mirror_legs(game);
        assert_eq!(mirrored.outcomes.len(), 4, "two markets, two legs each");

        let find = |tok: &str| mirrored.outcomes.iter()
            .find(|o| o.pm.token_id == tok)
            .map(|o| o.pm.label.clone());

        // A squadron on the Cubs' market: YES pays on the Cubs, NO on Boston.
        assert_eq!(find("KXMLBGAME-26SEP261915CHCBOS-CHC#yes").as_deref(), Some("Chicago C"));
        assert_eq!(find("KXMLBGAME-26SEP261915CHCBOS-CHC#no").as_deref(), Some("Boston"));
        // And on Boston's market, the polarity is the other way round.
        assert_eq!(find("KXMLBGAME-26SEP261915CHCBOS-BOS#yes").as_deref(), Some("Boston"));
        assert_eq!(find("KXMLBGAME-26SEP261915CHCBOS-BOS#no").as_deref(), Some("Chicago C"));

        // The odds name travels with the mirror, or the ledger would record the
        // wrong bookmaker outcome against it.
        let no_leg = mirrored.outcomes.iter()
            .find(|o| o.pm.token_id.ends_with("-CHC#no")).expect("mirror exists");
        assert_eq!(no_leg.odds_name, "Boston FC");
    }
}
