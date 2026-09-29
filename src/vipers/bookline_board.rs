//! Bookline's board lane: the viper's quoting rule, run off the sports ledger's
//! own snapshots against EVERY pre-game market the board prices, not only the
//! one market the sports squadron happens to hold.
//!
//! # Why a second lane
//!
//! The squadron lane (`bookline_impl`) is bound to its squadron: it reads
//! `ctx.sports` and `ctx.snapshot`, so it can only ever observe the market the
//! sports slot deployed onto. That slot is chosen by liquidity, which is right for
//! the vipers that trade real money there and is indifferent to whether the game
//! has a favorite Bookline could quote. Measured on production over five game
//! days: the board held 26 to 89 games a day, of which 13 to 27 cleared every
//! Bookline gate at some point in the pre-game window, while the squadron lane,
//! bound to one liquid coin-flip at a time, wrote zero rows. The evidence was on
//! the board the whole time; the lane collecting it was looking at one game.
//!
//! This lane collects it. It changes nothing about slot selection and nothing the
//! live vipers read: it is a consumer of the ledger's rows and the published board,
//! both of which already exist, and it writes only to `bookline_shadow` under
//! `lane = 'board'`.
//!
//! # What its record is, and is not
//!
//! The squadron lane sees the venue book every 50 ms and fills the moment the best
//! ask touches its bid. This lane sees the book only when the ledger snapshots it,
//! every five to ten minutes inside the last two hours and less often before that,
//! and can only fill when a snapshot happens to catch the ask at or under the bid.
//! An ask that dipped through the bid between snapshots and recovered is a fill
//! the squadron lane would have taken and this lane cannot see. Its fill count is
//! therefore a FLOOR under the rule's real fill rate, and its record is not a
//! second sample of the squadron lane's. The two are summarized per lane and never
//! summed; see `db::bookline_shadow_lane_summary`.
//!
//! It also takes only the exit every position plans for: settlement. The squadron
//! lane's two early exits (the settle-snipe and the resting take-profit) read a
//! live bid at tick cadence, and a replay at snapshot cadence would exercise them
//! only where a snapshot happened to land, which is a rule in the record's favor
//! or against it depending on the game. Leaving them out keeps this lane's record
//! the settlement-only floor it claims to be.
//!
//! # Where it runs
//!
//! One task per instance, spawned beside the ledger in `main.rs`, on the primary
//! database (the ledger's own). It reads the GLOBAL config row: the lane belongs
//! to no squadron, so a squadron-scoped value has nothing to attach to. Every
//! parameter that decides one of its quotes, pulls or fills is its OWN knob
//! (`bookline_board_*`, the "Bookline Board Lane" group in Setup), seeded from the
//! same compile-time defaults as the squadron lane's. The first cut read the
//! squadron lane's keys off the global row, which the Bookline card never
//! patches (it patches a squadron row), so the lane sat on compile-time defaults
//! with no way to move them. Inert without the ledger, its only source of rows.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use tokio::sync::watch;
use tracing::{debug, info};

use crate::helpers::db::{self, SportsLedgerRow, BOOKLINE_LANE_BOARD};
use crate::helpers::dynamic_config::DynamicConfig;
use crate::raptors::sports_ledger::{self, SportsBoard};
use crate::vipers::bookline_impl::{
    has_room, line_usable, maker_fee, pull_reason, quote_price, required_edge, LineRefusal,
};

/// The `asset` column on this lane's rows. The squadron lane writes its
/// squadron's shard name there ("SPORTS" on Polymarket International, the sports
/// scope on the other venues); this lane belongs to no squadron, so it writes the
/// class. The lane column is what keeps the two apart; this is only a label.
pub const BOARD_ASSET: &str = "SPORTS";

/// How often the lane looks. Snapshots land minutes apart, so a minute is the
/// same cadence the ledger itself runs at.
const TICK_SECS: u64 = 60;
/// Games considered for a new quote: kick-off inside this window either side of
/// now. Mirrors the ledger's own board seed window, so the lane sees the slate the
/// board does. Pre-game is enforced separately by the line's clock.
const LOOKBACK_SECS: i64 = 6 * 3600;
const LOOKAHEAD_SECS: i64 = 36 * 3600;

/// Per-process memory. Nothing here is needed for correctness after a restart:
/// the rows carry the record, and the ledger's holds check prevents a second
/// commitment on a market that already has one.
#[derive(Default)]
pub struct BoardLaneState {
    /// Newest ledger snapshot `ts` already scored per market. A snapshot is scored
    /// once, when it arrives, not on every tick until the next one: rescoring the
    /// same book with an older line can only make the same decision or a worse one.
    scored: HashMap<String, String>,
    was_enabled: Option<bool>,
    cap_logged: bool,
}

/// One sweep's tallies, for the log line and for tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub quoted: usize,
    pub filled: usize,
    pub pulled: usize,
    pub settled: usize,
    pub live_after: usize,
    /// Markets on the board, pre-game, scored this sweep.
    pub scored: usize,
}

pub async fn run_bookline_board(mut config_rx: watch::Receiver<Arc<DynamicConfig>>) {
    let mut st = BoardLaneState::default();
    loop {
        let cfg = config_rx.borrow().clone();
        if let Some(pool) = db::pool() {
            let board = sports_ledger::board();
            let report = sweep(pool, &cfg, &board, &mut st, Utc::now()).await;
            if report.quoted + report.filled + report.pulled + report.settled > 0 {
                info!(
                    "📖 Bookline board lane: scored {} market(s) — quoted {}, filled {}, pulled {}, settled {}; {} live (simulated, snapshot cadence)",
                    report.scored, report.quoted, report.filled, report.pulled, report.settled, report.live_after,
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(TICK_SECS)) => {}
            _ = config_rx.changed() => {}
        }
    }
}

/// One pass: look after what is resting or filled, then quote what the board
/// newly offers. Takes the board as a parameter so a test can hand it one built
/// with `fold_rows` rather than publishing to the process-wide channel.
pub async fn sweep(
    pool: &sqlx::SqlitePool,
    cfg: &DynamicConfig,
    board: &SportsBoard,
    st: &mut BoardLaneState,
    now: DateTime<Utc>,
) -> SweepReport {
    let enabled = cfg.bookline_board_lane_enabled;
    if st.was_enabled != Some(enabled) {
        info!("📖 Bookline board lane {}", if enabled {
            "enabled — replaying Bookline's rule against every pre-game market on the bookmaker board (simulated; never places an order)"
        } else {
            "disabled — resting simulated bids will be pulled; filled ones still settle"
        });
        st.was_enabled = Some(enabled);
    }
    let mut report = SweepReport::default();

    // ── Look after what is out there. Not gated on the switch: off means "stop
    // quoting", never "abandon the rows", and this sweep is the only thing that
    // will ever fill, pull or settle them.
    for row in db::bookline_shadow_open(pool, BOOKLINE_LANE_BOARD, BOARD_ASSET).await {
        let quote_px = Decimal::try_from(row.quote_price).unwrap_or(Decimal::ZERO);

        if row.filled_at.is_some() {
            // Settlement, and only settlement: see the module note on early exits.
            if let Some(resolved) = db::sports_resolved_price(pool, &row.token_id).await {
                let px = Decimal::try_from(resolved).unwrap_or(Decimal::ZERO);
                // Fee-free at settlement; the entry leg costs what the venue charges
                // makers, which is zero, a rebate or a fee depending on the venue.
                let ret = ((px - quote_px - maker_fee(quote_px)) / quote_px).to_f64().unwrap_or(0.0);
                if db::bookline_shadow_close(pool, row.id, resolved, "settlement", ret).await {
                    report.settled += 1;
                    info!("📖 Bookline board settled [{}] {} @ ${:.2} — {:+.2}% (simulated)",
                          row.market, row.side, resolved, ret * 100.0);
                }
            }
            continue;
        }

        // ── Resting: replay the snapshots taken since the bid was placed ──────
        //
        // In order, each one asks first "would the bid have been pulled here?" and
        // only then "did the ask cross it?". A pull decided at one snapshot stands
        // even if a later snapshot would have filled: the squadron lane, seeing
        // the same sequence live, would have been out of the book by then.
        let mut decided = false;
        for snap in db::sports_ledger_rows_for_token_since(pool, &row.token_id, &row.quoted_at).await {
            match replay_step(&snap, quote_px, row.consensus_at_quote, cfg) {
                Replay::Pull(why) => {
                    if db::bookline_shadow_pull(pool, row.id, why).await {
                        report.pulled += 1;
                        info!("📖 Bookline board pull [{}] {}: {} (at snapshot {}, simulated)",
                              row.market, row.side, why, snap.ts);
                    }
                    decided = true;
                    break;
                }
                Replay::Fill(ask) => {
                    if db::bookline_shadow_fill_at(pool, row.id, &snap.ts).await {
                        report.filled += 1;
                        info!("📖 Bookline board filled [{}] {} @ ${:.3} (ask ${:.3} at snapshot {}) — simulated",
                              row.market, row.side, row.quote_price, ask, snap.ts);
                    }
                    decided = true;
                    break;
                }
                Replay::Rest => {}
            }
        }
        if decided { continue; }

        // ── Still resting after the replay: judge it against the clock NOW ────
        //
        // The last pre-game snapshot lands minutes before kick-off and nothing
        // follows it, so kick-off arriving, the feed going quiet past the hold
        // bar, the line leaving the board and the switch going off all have to
        // be seen from here rather than from a snapshot that will never come.
        let line = board.get(&row.token_id);
        let verdict = match line {
            Some(l) => line_usable(
                Decimal::try_from(l.consensus).unwrap_or(Decimal::ZERO), None, l.num_books,
                l.dispersion.and_then(|d| Decimal::try_from(d).ok()),
                l.age_secs(now), l.secs_to_start(now), cfg.bookline_board_min_consensus,
                cfg.bookline_board_min_books, cfg.bookline_board_max_dispersion,
                cfg.bookline_board_pull_feed_age(), cfg.bookline_board_pull_before_start_secs,
            ),
            None => Err(LineRefusal::NoLine),
        };
        let adverse = line
            .and_then(|l| Decimal::try_from(l.consensus).ok())
            .map(|c| Decimal::try_from(row.consensus_at_quote).unwrap_or(c) - c)
            .unwrap_or(Decimal::ZERO);
        let why = if !enabled {
            Some("Bookline board lane disabled")
        } else {
            pull_reason(verdict, adverse, cfg.bookline_board_pull_on_adverse_drift)
        };
        if let Some(why) = why {
            if db::bookline_shadow_pull(pool, row.id, why).await {
                report.pulled += 1;
                info!("📖 Bookline board pull [{}] {}: {} (simulated)", row.market, row.side, why);
            }
        }
    }

    if !enabled {
        report.live_after = db::bookline_shadow_open(pool, BOOKLINE_LANE_BOARD, BOARD_ASSET).await.len();
        return report;
    }

    // ── Quote what the board newly offers ─────────────────────────────────────
    let live = db::bookline_shadow_open(pool, BOOKLINE_LANE_BOARD, BOARD_ASSET).await;
    let held: HashSet<String> = live.iter().map(|r| r.condition_id.clone()).collect();
    let mut open_markets = held.len();
    // Not a knob of this lane: it holds no capital and its return is per share,
    // so the size only sets the `shares` column. The squadron lane's compile-time
    // default keeps the two ledgers' share counts comparable.
    let size = crate::config::BOOKLINE_TRADE_SIZE_USDC;

    let from = (now - ChronoDuration::seconds(LOOKBACK_SECS)).to_rfc3339();
    let to = (now + ChronoDuration::seconds(LOOKAHEAD_SECS)).to_rfc3339();
    let newest = db::sports_ledger_board_rows(pool, &from, &to).await;
    let mut by_market: HashMap<&str, Vec<&SportsLedgerRow>> = HashMap::new();
    for r in &newest {
        by_market.entry(r.condition_id.as_str()).or_default().push(r);
    }
    // Deterministic order, so a cap that bites does so on the same games each tick.
    let mut markets: Vec<(&str, Vec<&SportsLedgerRow>)> = by_market.into_iter().collect();
    markets.sort_by(|a, b| a.0.cmp(b.0));

    let mut refusals: HashMap<String, usize> = HashMap::new();
    for (cid, legs) in markets {
        // Pre-game only, from the board's clock: the ledger keeps a finished game's
        // line on the board for hours, and a game under way is not this lane's.
        let pre_game = legs.iter().any(|l| board.get(&l.token_id).is_some_and(|b| b.commence > now));
        if !pre_game { continue; }
        // A snapshot is scored once. The key is the newest `ts` any leg carries.
        let newest_ts = legs.iter().map(|l| l.ts.as_str()).max().unwrap_or_default().to_string();
        if st.scored.get(cid).is_some_and(|t| *t == newest_ts) { continue; }
        st.scored.insert(cid.to_string(), newest_ts);
        report.scored += 1;

        if held.contains(cid) {
            // One side, one market: the same rule as the squadron lane. A bid on
            // both outcomes is two-sided market making, which this viper is not.
            continue;
        }
        // The sanity bound: counted in markets, like the squadron lane's cap, but
        // sized to the whole board. Nothing here is capital, so exposure is not a
        // second limit; `Decimal::MAX` says so where `has_room` asks.
        if let Err(why) = has_room(open_markets, Decimal::ZERO, size, cfg.bookline_board_max_open_markets, Decimal::MAX) {
            if !st.cap_logged {
                info!("📖 Bookline board lane: {why} ({open_markets} of {} live) — not quoting more until some settle",
                      cfg.bookline_board_max_open_markets);
                st.cap_logged = true;
            }
            break;
        }
        st.cap_logged = false;

        // Score every outcome and take the one with the most room under its
        // consensus. On a two-outcome game the favorite floor leaves one at most;
        // a three-outcome (draw) market can leave more, hence the choice.
        let mut best: Option<(&SportsLedgerRow, Decimal, Decimal, Decimal, Decimal)> = None;
        for leg in &legs {
            match score_leg(leg, board, cfg, now) {
                Ok((px, consensus, edge)) => {
                    let room = consensus - px;
                    if best.as_ref().is_none_or(|b| room > b.4) {
                        best = Some((leg, px, consensus, edge, room));
                    }
                }
                Err(why) => *refusals.entry(why).or_default() += 1,
            }
        }
        let Some((leg, px, consensus, edge, room)) = best else { continue };
        let shares = size / px;
        if shares < crate::config::MIN_ORDER_SHARES {
            *refusals.entry("trade size is below the venue's minimum at this price".into()).or_default() += 1;
            continue;
        }
        let line = board.get(&leg.token_id).expect("scored leg has a board line");
        info!(
            "📖 Bookline board quote [{}] {} @ ${:.3} x {:.2} | consensus {:.3} ({} books, disp {:?}), room {:.3}, edge {:.3}, {}m to kick-off — simulated",
            leg.pm_slug, leg.outcome_label, px, shares, consensus, line.num_books, line.dispersion,
            room, edge, line.secs_to_start(now) / 60,
        );
        if db::bookline_shadow_quote(
            pool, BOOKLINE_LANE_BOARD, BOARD_ASSET, &leg.condition_id, &leg.token_id,
            &leg.pm_slug, &leg.outcome_label,
            Some(line.league.as_str()), Some(&line.commence.to_rfc3339()),
            px.to_f64().unwrap_or(0.0), shares.to_f64().unwrap_or(0.0),
            line.consensus, edge.to_f64().unwrap_or(0.0), line.num_books, line.dispersion,
        ).await {
            report.quoted += 1;
            open_markets += 1;
        }
    }
    if !refusals.is_empty() {
        let mut list: Vec<(String, usize)> = refusals.into_iter().collect();
        list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        debug!("📖 Bookline board lane refusals this sweep: {}",
               list.iter().map(|(k, n)| format!("{n}x {k}")).collect::<Vec<_>>().join(" | "));
    }
    report.live_after = open_markets;
    report
}

/// One resting quote against one later snapshot.
#[derive(Debug, PartialEq)]
enum Replay {
    Pull(&'static str),
    Fill(f64),
    Rest,
}

/// The pull-then-fill decision for a resting bid at `quote_px`, as of one ledger
/// snapshot taken after it was placed.
///
/// The line is judged as it stood AT the snapshot (age zero: the odds were read
/// then), with the hold-side thresholds the squadron lane uses for a bid already
/// resting. Adverse drift is measured from the consensus the bid was committed
/// against, carried on the row. Then the fill rule, the pessimistic one the whole
/// engine uses: a resting BUY at Q fills when the best ASK is at or under Q. An
/// absent ask is not a price and fills nothing.
fn replay_step(snap: &SportsLedgerRow, quote_px: Decimal, consensus_at_quote: f64, cfg: &DynamicConfig) -> Replay {
    let verdict = match snap.consensus {
        Some(c) => line_usable(
            Decimal::try_from(c).unwrap_or(Decimal::ZERO), None, snap.num_books,
            snap.dispersion.and_then(|d| Decimal::try_from(d).ok()),
            0, snap.secs_to_start, cfg.bookline_board_min_consensus, cfg.bookline_board_min_books,
            cfg.bookline_board_max_dispersion, cfg.bookline_board_pull_feed_age(), cfg.bookline_board_pull_before_start_secs,
        ),
        None => Err(LineRefusal::NoLine),
    };
    let adverse = match snap.consensus.and_then(|c| Decimal::try_from(c).ok()) {
        Some(c) => Decimal::try_from(consensus_at_quote).unwrap_or(c) - c,
        None => Decimal::ZERO,
    };
    if let Some(why) = pull_reason(verdict, adverse, cfg.bookline_board_pull_on_adverse_drift) {
        return Replay::Pull(why);
    }
    match snap.pm_ask {
        Some(ask) if ask > 0.0 && Decimal::try_from(ask).is_ok_and(|a| a <= quote_px) => Replay::Fill(ask),
        _ => Replay::Rest,
    }
}

/// Where a new bid on this leg would rest, or why none would: `(price,
/// consensus, required_edge)`.
///
/// The line comes from the published board (it carries the drift the edge rule
/// wants); the bid and ask come from the leg's newest ledger row, which is the
/// only book this lane has. The row has to be as fresh as the line: a bid read
/// two snapshots ago is not a queue to join, whatever the odds say now.
fn score_leg(
    leg: &SportsLedgerRow,
    board: &SportsBoard,
    cfg: &DynamicConfig,
    now: DateTime<Utc>,
) -> Result<(Decimal, Decimal, Decimal), String> {
    let line = board.get(&leg.token_id).ok_or_else(|| LineRefusal::NoLine.label().to_string())?;
    let row_age = DateTime::parse_from_rfc3339(&leg.ts)
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0))
        .unwrap_or(i64::MAX);
    if row_age > cfg.bookline_board_max_feed_age_secs {
        return Err("book snapshot is older than the line the lane may quote against".into());
    }
    let consensus = Decimal::try_from(line.consensus).unwrap_or(Decimal::ZERO);
    let bid = leg.pm_bid.and_then(|b| Decimal::try_from(b).ok()).unwrap_or(Decimal::ZERO);
    let ask = leg.pm_ask.and_then(|a| Decimal::try_from(a).ok());
    // A mid needs both levels; `None` skips the plausibility check rather than
    // faking a mid from one side, the same rule the squadron lane applies.
    let mid = match ask {
        Some(a) if bid > Decimal::ZERO && a > Decimal::ZERO => Some((bid + a) / Decimal::from(2)),
        _ => None,
    };
    let secs_to_start = line.secs_to_start(now);
    line_usable(
        consensus, mid, line.num_books,
        line.dispersion.and_then(|d| Decimal::try_from(d).ok()),
        line.age_secs(now), secs_to_start,
        cfg.bookline_board_min_consensus, cfg.bookline_board_min_books, cfg.bookline_board_max_dispersion,
        cfg.bookline_board_max_feed_age_secs, cfg.bookline_board_pull_before_start_secs,
    ).map_err(|r| r.label().to_string())?;
    let drift = match (line.drift, line.drift_secs) {
        (Some(d), Some(s)) => Decimal::try_from(d).ok().map(|d| (d, s)),
        _ => None,
    };
    let edge = required_edge(
        secs_to_start, drift, cfg.bookline_board_base_edge, cfg.bookline_board_min_edge,
        cfg.bookline_board_edge_taper_secs, cfg.bookline_board_pull_before_start_secs,
        cfg.bookline_board_drift_mult,
    );
    let px = quote_price(consensus, bid, edge, crate::venues::sports_tick_size())
        .map_err(|e| e.to_string())?;
    Ok((px, consensus, edge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raptors::sports_ledger::fold_rows;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// A ledger row for one outcome of one game, as the ledger would write it.
    fn row(cid: &str, token: &str, label: &str, consensus: f64, bid: Option<f64>, ask: Option<f64>,
           commence: &str, ts: &str) -> SportsLedgerRow {
        let secs_to_start = (t(commence) - t(ts)).num_seconds();
        SportsLedgerRow {
            ts: ts.into(), league: "nfl".into(), sport_key: "americanfootball_nfl".into(),
            odds_event_id: format!("ev-{cid}"), pm_slug: format!("game-{cid}"),
            condition_id: cid.into(), token_id: token.into(), outcome_label: label.into(),
            odds_outcome: label.into(), commence: commence.into(), secs_to_start,
            consensus: Some(consensus), num_books: 9, dispersion: Some(0.02), max_book_age_secs: Some(60),
            pm_bid: bid, pm_ask: ask, pm_bid_size: Some(100.0), pm_ask_size: Some(100.0),
            credits_remaining: Some(90_000),
            odds_at: Some(ts.into()), pm_at: Some(ts.into()), overround: Some(1.04),
            raw_consensus: Some(consensus * 1.04),
        }
    }

    /// Schema plus migrations: `bookline_shadow` and its `lane` column are both
    /// created in `run_migrations`, which the bare in-memory pool does not run.
    async fn test_pool() -> sqlx::SqlitePool {
        let pool = db::memory_pool_for_tests().await;
        db::run_migrations(&pool).await;
        pool
    }

    async fn record(pool: &sqlx::SqlitePool, rows: &[SportsLedgerRow]) {
        assert_eq!(db::record_sports_ledger_rows(pool, rows).await, rows.len());
    }

    async fn open_board(pool: &sqlx::SqlitePool) -> Vec<db::BooklineShadow> {
        db::bookline_shadow_open(pool, BOOKLINE_LANE_BOARD, BOARD_ASSET).await
    }

    /// Two games on the board, snapshotted 90 minutes out. One is a coin flip (the
    /// kind the liquidity selector picks and the squadron lane then idles on); the
    /// other has a 0.70 favorite with a bid sitting under consensus minus the edge.
    /// The lane quotes the favorite, exactly one side of it, and nothing on the
    /// coin flip. The squadron lane's holds check does not see the new row.
    #[tokio::test]
    async fn quotes_one_side_of_each_quotable_game_and_only_its_own_lane_sees_it() {
        let pool = test_pool().await;
        let cfg = DynamicConfig::default();
        let now = t("2026-09-27T22:30:00Z");
        let kick = "2026-09-28T00:00:00Z";
        let ts = "2026-09-27T22:30:00Z";
        let rows = vec![
            // Coin flip: both sides under the 0.55 favorite floor.
            row("flip", "flip-a", "Rams", 0.512, Some(0.50), Some(0.52), kick, ts),
            row("flip", "flip-b", "Broncos", 0.488, Some(0.47), Some(0.49), kick, ts),
            // Favorite: bid 0.66 sits well under 0.70 minus the tapered edge.
            row("fav", "fav-a", "Chiefs", 0.70, Some(0.66), Some(0.68), kick, ts),
            row("fav", "fav-b", "Jets", 0.30, Some(0.31), Some(0.33), kick, ts),
        ];
        record(&pool, &rows).await;
        let board = fold_rows(&SportsBoard::new(), &rows, now);

        let mut st = BoardLaneState::default();
        let r = sweep(&pool, &cfg, &board, &mut st, now).await;
        assert_eq!(r.scored, 2, "both pre-game markets were scored");
        assert_eq!(r.quoted, 1, "one quote, on the favorite");

        let live = open_board(&pool).await;
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].token_id, "fav-a");
        assert_eq!(live[0].side, "Chiefs");
        assert_eq!(live[0].lane, BOOKLINE_LANE_BOARD);
        // Joined the queue at the best bid; never improved it.
        assert!((live[0].quote_price - 0.66).abs() < 1e-9, "quote {}", live[0].quote_price);

        // Lane isolation: the squadron lane, on this very market, is not committed.
        assert!(db::bookline_shadow_holds(&pool, BOOKLINE_LANE_BOARD, BOARD_ASSET, "fav").await);
        assert!(!db::bookline_shadow_holds(&pool, db::BOOKLINE_LANE_SQUADRON, BOARD_ASSET, "fav").await);
        assert!(db::bookline_shadow_open(&pool, db::BOOKLINE_LANE_SQUADRON, BOARD_ASSET).await.is_empty());

        // The same snapshot is not scored twice, and a held market is not re-quoted.
        let r2 = sweep(&pool, &cfg, &board, &mut st, now + ChronoDuration::seconds(60)).await;
        assert_eq!(r2.scored, 0);
        assert_eq!(r2.quoted, 0);
        assert_eq!(open_board(&pool).await.len(), 1);
    }

    /// The replay fills from a later snapshot whose ask reached the bid, and
    /// stamps the fill with that snapshot's time, not the tick that noticed it.
    #[tokio::test]
    async fn a_later_snapshot_with_the_ask_at_the_bid_fills_at_that_snapshots_time() {
        let pool = test_pool().await;
        let cfg = DynamicConfig::default();
        let kick = "2026-09-28T00:00:00Z";
        let first = vec![
            row("fav", "fav-a", "Chiefs", 0.70, Some(0.66), Some(0.68), kick, "2026-09-27T22:30:00Z"),
            row("fav", "fav-b", "Jets", 0.30, Some(0.31), Some(0.33), kick, "2026-09-27T22:30:00Z"),
        ];
        record(&pool, &first).await;
        let now = t("2026-09-27T22:30:00Z");
        let board = fold_rows(&SportsBoard::new(), &first, now);
        let mut st = BoardLaneState::default();
        assert_eq!(sweep(&pool, &cfg, &board, &mut st, now).await.quoted, 1);
        // The row's `quoted_at` is wall-clock now; the replay reads snapshots after
        // it, so the later snapshots must be stamped after the quote.
        let quoted_at = t(&open_board(&pool).await[0].quoted_at);
        let later1 = (quoted_at + ChronoDuration::seconds(600)).to_rfc3339();
        let later2 = (quoted_at + ChronoDuration::seconds(1200)).to_rfc3339();
        let kick_after = (quoted_at + ChronoDuration::seconds(5400)).to_rfc3339();

        // Ask still above the bid: rests.
        let snap1 = vec![row("fav", "fav-a", "Chiefs", 0.70, Some(0.66), Some(0.67), &kick_after, &later1)];
        record(&pool, &snap1).await;
        let board = fold_rows(&board, &snap1, t(&later1));
        let r = sweep(&pool, &cfg, &board, &mut st, t(&later1)).await;
        assert_eq!((r.filled, r.pulled), (0, 0));

        // Ask at the bid: fills, at snapshot time.
        let snap2 = vec![row("fav", "fav-a", "Chiefs", 0.70, Some(0.65), Some(0.66), &kick_after, &later2)];
        record(&pool, &snap2).await;
        let board = fold_rows(&board, &snap2, t(&later2));
        let r = sweep(&pool, &cfg, &board, &mut st, t(&later2) + ChronoDuration::seconds(45)).await;
        assert_eq!(r.filled, 1);
        let live = open_board(&pool).await;
        assert_eq!(live[0].filled_at.as_deref(), Some(later2.as_str()), "stamped with the snapshot, not the tick");

        // Settlement closes it fee-free with the plain return; a win here.
        db::record_sports_line_result(&pool, "fav", "fav-a", "Chiefs", 1.0).await;
        let r = sweep(&pool, &cfg, &board, &mut st, t(&later2) + ChronoDuration::seconds(7200)).await;
        assert_eq!(r.settled, 1);
        assert!(open_board(&pool).await.is_empty());
        let rets = db::bookline_shadow_returns(&pool, BOOKLINE_LANE_BOARD, BOARD_ASSET).await;
        assert_eq!(rets.len(), 1);
        let expected = (1.0 - 0.66 - maker_fee(Decimal::try_from(0.66).unwrap()).to_f64().unwrap()) / 0.66;
        assert!((rets[0].1 - expected).abs() < 1e-9, "ret {} vs {expected}", rets[0].1);
        let summary = db::bookline_shadow_lane_summary(&pool, BOOKLINE_LANE_BOARD).await;
        assert_eq!((summary.quoted, summary.settled, summary.wins, summary.pulled), (1, 1, 1, 0));
        // And the squadron lane's summary is untouched by all of it.
        assert_eq!(db::bookline_shadow_lane_summary(&pool, db::BOOKLINE_LANE_SQUADRON).await.quoted, 0);
    }

    /// Pull decisions come before fill decisions at each snapshot: a snapshot
    /// inside the pull-before-kick-off window whose ask happens to sit at the bid is
    /// a pull, because the squadron lane would already have been out of the book.
    /// Likewise a kick-off that arrives with no further snapshot pulls from the
    /// clock, and the switch going off pulls what rests but never touches a fill.
    #[tokio::test]
    async fn pulls_win_over_fills_and_the_clock_pulls_without_a_snapshot() {
        let pool = test_pool().await;
        let mut cfg = DynamicConfig::default();
        let kick = "2026-09-28T00:00:00Z";
        let first = vec![row("g", "g-a", "Fav", 0.70, Some(0.66), Some(0.68), kick, "2026-09-27T22:30:00Z")];
        record(&pool, &first).await;
        let now = t("2026-09-27T22:30:00Z");
        let board = fold_rows(&SportsBoard::new(), &first, now);
        let mut st = BoardLaneState::default();
        assert_eq!(sweep(&pool, &cfg, &board, &mut st, now).await.quoted, 1);
        let quoted_at = t(&open_board(&pool).await[0].quoted_at);

        // A snapshot ten minutes before kick-off, ask at the bid: pulled, not filled.
        let kick_soon = (quoted_at + ChronoDuration::seconds(600)).to_rfc3339();
        let late = (quoted_at + ChronoDuration::seconds(1)).to_rfc3339();
        let snap = vec![row("g", "g-a", "Fav", 0.70, Some(0.65), Some(0.66), &kick_soon, &late)];
        record(&pool, &snap).await;
        let board = fold_rows(&board, &snap, t(&late));
        let r = sweep(&pool, &cfg, &board, &mut st, t(&late)).await;
        assert_eq!((r.filled, r.pulled), (0, 1));
        assert!(open_board(&pool).await.is_empty());

        // Fresh quote on another game forty minutes out, then no more snapshots:
        // the clock alone pulls it once kick-off is inside the window. The feed is
        // still inside the hold bar (45 minutes) at every step, so the pull can
        // only be kick-off arriving and not the line going stale.
        let kick2 = "2026-09-28T03:00:00Z";
        let ts2 = "2026-09-28T02:20:00Z";
        let g2 = vec![row("h", "h-a", "Fav2", 0.70, Some(0.66), Some(0.68), kick2, ts2)];
        record(&pool, &g2).await;
        let board = fold_rows(&board, &g2, t(ts2));
        assert_eq!(sweep(&pool, &cfg, &board, &mut st, t(ts2)).await.quoted, 1);
        // Twenty-five minutes out: still resting.
        assert_eq!(sweep(&pool, &cfg, &board, &mut st, t(ts2) + ChronoDuration::seconds(900)).await.pulled, 0);
        // Ten minutes out, no snapshot since: pulled from the clock.
        let r = sweep(&pool, &cfg, &board, &mut st, t(kick2) - ChronoDuration::seconds(600)).await;
        assert_eq!(r.pulled, 1);
        assert!(open_board(&pool).await.is_empty());

        // The switch: off pulls a resting bid and quotes nothing new.
        let ts3 = "2026-09-28T05:00:00Z";
        let kick3 = "2026-09-28T07:00:00Z";
        let g3 = vec![row("i", "i-a", "Fav3", 0.70, Some(0.66), Some(0.68), kick3, ts3)];
        record(&pool, &g3).await;
        let board = fold_rows(&board, &g3, t(ts3));
        assert_eq!(sweep(&pool, &cfg, &board, &mut st, t(ts3)).await.quoted, 1);
        cfg.bookline_board_lane_enabled = false;
        let r = sweep(&pool, &cfg, &board, &mut st, t(ts3) + ChronoDuration::seconds(60)).await;
        assert_eq!((r.pulled, r.quoted), (1, 0));
        assert!(open_board(&pool).await.is_empty());
    }

    /// The sanity bound counts markets and stops quoting at it.
    #[tokio::test]
    async fn the_open_market_bound_holds() {
        let pool = test_pool().await;
        let mut cfg = DynamicConfig::default();
        cfg.bookline_board_max_open_markets = 2;
        let kick = "2026-09-28T00:00:00Z";
        let ts = "2026-09-27T22:30:00Z";
        let rows: Vec<SportsLedgerRow> = (0..4)
            .map(|i| row(&format!("m{i}"), &format!("m{i}-a"), "Fav", 0.70, Some(0.66), Some(0.68), kick, ts))
            .collect();
        record(&pool, &rows).await;
        let board = fold_rows(&SportsBoard::new(), &rows, t(ts));
        let mut st = BoardLaneState::default();
        let r = sweep(&pool, &cfg, &board, &mut st, t(ts)).await;
        assert_eq!(r.quoted, 2);
        assert_eq!(open_board(&pool).await.len(), 2);
    }

    /// `replay_step` in isolation: the three outcomes and the absent-ask rule.
    #[test]
    fn replay_step_decides_pull_then_fill_then_rest() {
        let cfg = DynamicConfig::default();
        let kick = "2026-09-28T00:00:00Z";
        let q = Decimal::try_from(0.66).unwrap();
        // Plenty of time, ask above the bid: rests.
        let s = row("g", "g-a", "Fav", 0.70, Some(0.66), Some(0.67), kick, "2026-09-27T22:30:00Z");
        assert_eq!(replay_step(&s, q, 0.70, &cfg), Replay::Rest);
        // Ask at the bid: fills.
        let s = row("g", "g-a", "Fav", 0.70, Some(0.65), Some(0.66), kick, "2026-09-27T22:30:00Z");
        assert_eq!(replay_step(&s, q, 0.70, &cfg), Replay::Fill(0.66));
        // No ask recorded: nothing to fill against.
        let s = row("g", "g-a", "Fav", 0.70, Some(0.65), None, kick, "2026-09-27T22:30:00Z");
        assert_eq!(replay_step(&s, q, 0.70, &cfg), Replay::Rest);
        // Consensus moved against the bid by more than the drift bar: pulled, even
        // though the ask is at the bid.
        let s = row("g", "g-a", "Fav", 0.68, Some(0.65), Some(0.66), kick, "2026-09-27T22:30:00Z");
        assert_eq!(replay_step(&s, q, 0.70, &cfg), Replay::Pull("consensus moved against the resting bid"));
        // Inside the pull-before-kick-off window: pulled.
        let s = row("g", "g-a", "Fav", 0.70, Some(0.65), Some(0.66), kick, "2026-09-27T23:52:00Z");
        assert_eq!(replay_step(&s, q, 0.70, &cfg), Replay::Pull(LineRefusal::TooCloseToStart.label()));
    }
}
