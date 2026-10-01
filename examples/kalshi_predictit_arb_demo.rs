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

//! Offline demo of the Kalshi–PredictIt Arb Raptor.
//!
//! No network, no key, no cost: this publishes the bundled fictional SAMPLE
//! fixture (NOT real market data) through the same `watch` channel and
//! snapshot type the spawned Raptor uses, and prints what a consumer would see.
//! It never reads `X402_WALLET_KEY` and cannot enter live (paid) mode.
//!
//! ```text
//! cp src/config.balanced.rs.example src/config.rs   # if you have no config.rs yet
//! cargo run --example kalshi_predictit_arb_demo
//! cargo run --example kalshi_predictit_arb_demo -- all governor
//! ```
//!
//! Disclosure: kalshi-predictit-arb is built and operated by Team Takatini.
//! Docs: https://mastertyrone.github.io/kalshi-predictit-arb/ · client:
//! https://github.com/mastertyrone/kalshi-predictit-arb

use dradis::raptors::kalshi_predictit_arb::{publish_demo, ArbRaptorConfig, ArbSnapshot, FeedMode};
use tokio::sync::watch;

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("all") => FeedMode::All,
        _ => FeedMode::Opportunities,
    };
    let q = args.collect::<Vec<_>>().join(" ");
    let cfg = ArbRaptorConfig {
        q,
        limit: 25,
        mode,
        ..Default::default()
    };

    let (tx, rx) = watch::channel(ArbSnapshot::default());
    let sample = publish_demo(&tx, &cfg);
    let snap = *rx.borrow();

    println!("Kalshi–PredictIt Arb Raptor — DEMO (fictional SAMPLE data, NOT real market data)");
    println!(
        "mode={} q={:?} limit={}",
        cfg.mode.as_str(),
        cfg.q,
        cfg.limit
    );
    for p in &sample.pairs {
        let net = p
            .net_yield_c
            .map(|n| format!("{n}¢"))
            .unwrap_or_else(|| "n/a".into());
        println!(
            "- {}\n    kalshi={} predictit={} / {} dir={} net={} executable={}",
            p.event,
            or_na(&p.kalshi_ticker),
            or_na(&p.predictit_market),
            or_na(&p.predictit_contract),
            or_na(&p.direction),
            net,
            p.executable
        );
    }
    println!(
        "snapshot: pairs={} executable={} best={}¢ ({}%) roc={}% connected={} sample_data={}",
        snap.num_pairs,
        snap.num_executable,
        snap.best_net_yield_c,
        snap.best_net_yield_pct,
        snap.best_roc_annualized_pct,
        snap.connected,
        snap.sample_data
    );
    println!(
        "Live mode costs $0.02 USDC per call on Base (x402), capped at ${}/call and ${}/day; \
         it needs X402_WALLET_KEY and a signing build (intl_clob, or --features x402).",
        dec_str(cfg.max_usd_per_call),
        dec_str(cfg.daily_budget_usd)
    );
}

fn or_na(s: &str) -> &str {
    if s.is_empty() {
        "n/a"
    } else {
        s
    }
}

fn dec_str(d: rust_decimal::Decimal) -> String {
    d.normalize().to_string()
}
