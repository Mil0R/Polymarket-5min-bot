//! Stage-1 conversion economics research. No signer or transaction submission.
use super::{
    BookSnapshot, CrossMarketProbabilityArbitrageConfig, CrossMarketProbabilityResearchClient,
    FeePolicy, RelationLeg,
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub id: String,
    pub event_id: String,
    pub slug: String,
    pub input: RelationLeg,
    pub outputs: Vec<RelationLeg>,
    pub protocol_verified: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Config {
    pub root: PathBuf,
    pub poll_secs: u64,
    pub refresh_secs: u64,
    pub duration_secs: u64,
    pub delays_ms: Vec<u64>,
    pub sizes: Vec<f64>,
    pub budget: f64,
    pub minimum_profit: f64,
    pub haircut: f64,
    pub slippage_bps: f64,
    pub conversion_fee_bps: f64,
    pub chain_cost: f64,
    pub max_age_ms: i64,
    pub max_skew_ms: i64,
    pub max_future_skew_ms: i64,
    pub cooldown_secs: u64,
}
fn env<T: std::str::FromStr>(key: &str, default: &str) -> Result<T> {
    std::env::var(format!("NEG_RISK_CONVERSION_ARBITRAGE_{key}"))
        .unwrap_or_else(|_| default.into())
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid {key}"))
}
impl Config {
    fn load() -> Result<Self> {
        let c = Self {
            root: PathBuf::from(env::<String>(
                "RUN_DIR",
                "results/neg-risk-conversion-arbitrage/shadow-run-001",
            )?),
            poll_secs: env("POLL_SECS", "10")?,
            refresh_secs: env("REFRESH_SECS", "1800")?,
            duration_secs: env("DURATION_SECS", "0")?,
            delays_ms: vec![2000, 5000, 10000],
            sizes: vec![5., 10., 25.],
            budget: env("MAX_INPUT_USDC", "25")?,
            minimum_profit: env("MIN_PROFIT_USDC", "0.05")?,
            haircut: env("DEPTH_HAIRCUT", "0.75")?,
            slippage_bps: env("SLIPPAGE_BPS", "10")?,
            conversion_fee_bps: env("ASSUMED_CONVERSION_FEE_BPS", "0")?,
            chain_cost: env("CHAIN_COST_USDC", "0.02")?,
            max_age_ms: env("MAX_BOOK_AGE_MS", "5000")?,
            max_skew_ms: env("MAX_SNAPSHOT_SKEW_MS", "2000")?,
            max_future_skew_ms: env("MAX_FUTURE_SKEW_MS", "10000")?,
            cooldown_secs: env("EPISODE_COOLDOWN_SECS", "300")?,
        };
        if c.poll_secs == 0
            || c.refresh_secs == 0
            || c.max_age_ms <= 0
            || c.max_skew_ms <= 0
            || !(0..=30000).contains(&c.max_future_skew_ms)
            || c.cooldown_secs == 0
            || ![
                c.budget,
                c.minimum_profit,
                c.haircut,
                c.slippage_bps,
                c.conversion_fee_bps,
                c.chain_cost,
            ]
            .iter()
            .all(|v| v.is_finite())
            || c.budget <= 0.
            || c.minimum_profit < 0.
            || !(0.0..=1.0).contains(&c.haircut)
            || c.haircut == 0.
            || !(0.0..=1000.).contains(&c.slippage_bps)
            || !(0.0..=1000.).contains(&c.conversion_fee_bps)
            || c.chain_cost < 0.
        {
            bail!("invalid conversion research configuration");
        }
        Ok(c)
    }
}
#[derive(Clone, Debug, Serialize)]
struct Quote {
    shares: f64,
    input_cost: f64,
    output_shares: f64,
    exit_proceeds: f64,
    net_profit: f64,
}

fn fee(policy: &FeePolicy, p: f64, q: f64) -> Option<f64> {
    if !policy.enabled {
        return Some(0.);
    }
    let rate = policy.rate.to_f64()?;
    if !rate.is_finite() || rate <= 0. || rate > 1. || policy.exponent > 10 {
        return None;
    }
    Some(q * rate * (p * (1. - p)).powi(policy.exponent as i32))
}
/// Full-fill cash flow, using asks to acquire NO and bids to liquidate YES.
fn walk(book: &BookSnapshot, leg: &RelationLeg, q: f64, buy: bool, c: &Config) -> Option<f64> {
    let mut levels = if buy {
        book.asks.clone()
    } else {
        book.bids.clone()
    };
    levels.sort_by_key(|l| l.price);
    if !buy {
        levels.reverse();
    }
    let mut left = q;
    let mut value = 0.;
    for level in levels {
        let p = level.price.to_f64()?;
        let available = level.size.to_f64()? * c.haircut;
        if !(0.0..1.0).contains(&p) || p == 0. || available <= 0. {
            continue;
        }
        let qty = available.min(left);
        let price = (p
            * (1.
                + if buy {
                    c.slippage_bps / 10000.
                } else {
                    -c.slippage_bps / 10000.
                }))
        .clamp(0., 1.);
        value += qty * price
            + if buy {
                fee(&leg.fee, price, qty)?
            } else {
                -fee(&leg.fee, price, qty)?
            };
        left -= qty;
        if left <= 1e-8 {
            return Some(value);
        }
    }
    None
}
fn fresh<'a>(
    books: &'a HashMap<String, BookSnapshot>,
    leg: &RelationLeg,
    now: i64,
    c: &Config,
) -> Result<&'a BookSnapshot> {
    let b = books.get(&leg.token_id).context("missing_book")?;
    if b.token_id != leg.token_id {
        bail!("token_mismatch");
    }
    let age = now - b.observed_at.timestamp_millis();
    if age < -1000 || age > c.max_age_ms {
        bail!("stale_book");
    }
    if let Some(ts) = b.exchange_timestamp_ms {
        if ts - now > c.max_future_skew_ms {
            bail!("exchange_clock_ahead");
        }
        if now - ts > c.max_age_ms {
            bail!("stale_exchange_timestamp");
        }
    }
    Ok(b)
}
fn evaluate(
    route: &Route,
    books: &HashMap<String, BookSnapshot>,
    q: f64,
    c: &Config,
    fixed_input: Option<f64>,
) -> Result<Quote> {
    let now = Utc::now().timestamp_millis();
    let mut times = Vec::new();
    let mut exchange_times = Vec::new();
    let input_cost = if let Some(cost) = fixed_input {
        cost
    } else {
        let b = fresh(books, &route.input, now, c)?;
        times.push(b.observed_at.timestamp_millis());
        if let Some(t) = b.exchange_timestamp_ms {
            exchange_times.push(t);
        }
        walk(b, &route.input, q, true, c).context("input_depth_or_fee_unavailable")?
    };
    if input_cost > c.budget {
        bail!("input_budget");
    }
    let output_shares = q * (1. - c.conversion_fee_bps / 10000.);
    let mut exit_proceeds = 0.;
    for leg in &route.outputs {
        let b = fresh(books, leg, now, c)?;
        times.push(b.observed_at.timestamp_millis());
        if let Some(t) = b.exchange_timestamp_ms {
            exchange_times.push(t);
        }
        exit_proceeds +=
            walk(b, leg, output_shares, false, c).context("output_depth_or_fee_unavailable")?;
    }
    if route.outputs.is_empty() {
        bail!("no_outputs");
    }
    if times.iter().max().unwrap() - times.iter().min().unwrap() > c.max_skew_ms {
        bail!("snapshot_skew");
    }
    if exchange_times.len() > 1
        && exchange_times.iter().max().unwrap() - exchange_times.iter().min().unwrap()
            > c.max_skew_ms
    {
        bail!("exchange_snapshot_skew");
    }
    // One NO input produces no direct collateral. Cash comes only from YES sales.
    Ok(Quote {
        shares: q,
        input_cost,
        output_shares,
        exit_proceeds,
        net_profit: exit_proceeds - input_cost - c.chain_cost,
    })
}
struct Journal {
    file: File,
    log: File,
    run_id: String,
}
impl Journal {
    fn open(c: &Config) -> Result<Self> {
        fs::create_dir_all(&c.root)?;
        Ok(Self {
            file: OpenOptions::new()
                .create(true)
                .append(true)
                .open(c.root.join("events.ndjson"))?,
            log: OpenOptions::new()
                .create(true)
                .append(true)
                .open(c.root.join("runtime.log"))?,
            run_id: uuid::Uuid::new_v4().to_string(),
        })
    }
    fn event(&mut self, mut event: Value) -> Result<()> {
        event["recorded_at"] = json!(Utc::now());
        event["run_id"] = json!(self.run_id);
        serde_json::to_writer(&mut self.file, &event)?;
        writeln!(self.file)?;
        self.file.flush()?;
        Ok(())
    }
    fn log(&mut self, message: &str) -> Result<()> {
        let line = format!("{} {message}", Utc::now());
        println!("{line}");
        writeln!(self.log, "{line}")?;
        self.log.flush()?;
        Ok(())
    }
}

pub async fn run(command: &str) -> Result<()> {
    dotenvy::dotenv().ok();
    let c = Config::load()?;
    if command == "report" {
        return report(&c);
    }
    if !matches!(command, "scan" | "shadow") {
        bail!("use scan, shadow or report");
    }
    let mut base = CrossMarketProbabilityArbitrageConfig::from_env()?;
    base.event_pages = env("EVENT_PAGES", "10")?;
    base.max_relationships = env("MAX_ROUTES", "100")?;
    base.max_partition_legs = env("MAX_OUTCOMES", "6")?;
    base.request_concurrency = env("REQUEST_CONCURRENCY", "8")?;
    base.websocket_books = false;
    if base.event_pages == 0
        || base.event_pages > 100
        || base.max_relationships == 0
        || base.max_relationships > 2000
        || !(2..=12).contains(&base.max_partition_legs)
        || !(1..=32).contains(&base.request_concurrency)
    {
        bail!("invalid discovery limits");
    }
    let client = CrossMarketProbabilityResearchClient::new(base)?;
    let mut journal = Journal::open(&c)?;
    journal.event(json!({"event":"run_started","config":c,"mode":"read_only_shadow","protocol_verified":false,"stage":"economics_only"}))?;
    let started = Instant::now();
    let mut refresh = Instant::now();
    let mut routes = Vec::new();
    let mut initialized = false;
    let mut last_episode = HashMap::<String, Instant>::new();
    let mut last_log = Instant::now();
    let work = async {
        loop {
            if c.duration_secs > 0 && started.elapsed().as_secs() >= c.duration_secs {
                break;
            }
            if !initialized || refresh.elapsed().as_secs() >= c.refresh_secs {
                match client.discover_conversion_routes().await {
                    Ok((new, diagnostics)) => {
                        routes = new;
                        initialized = true;
                        refresh = Instant::now();
                        journal.event(
                            json!({"event":"discovery","diagnostics":diagnostics,"routes":routes}),
                        )?;
                        journal.log(&format!(
                            "discovered {} conversion routes; protocol mapping remains unverified",
                            routes.len()
                        ))?;
                    }
                    Err(e) => {
                        journal.event(
                            json!({"event":"error","phase":"discovery","message":e.to_string()}),
                        )?;
                        journal.log(&format!("discovery failed: {e}"))?;
                        if command == "scan" {
                            return Err(e);
                        }
                        tokio::time::sleep(Duration::from_secs(c.poll_secs)).await;
                        continue;
                    }
                }
            }
            let books = client.fetch_conversion_books(&routes).await;
            let mut rejects = BTreeMap::<String, usize>::new();
            let mut candidates = Vec::new();
            let mut closest: Option<(&Route, Quote)> = None;
            for route in &routes {
                let mut best: Option<Quote> = None;
                for &q in &c.sizes {
                    match evaluate(route, &books, q, &c, None) {
                        Ok(quote) if quote.net_profit >= c.minimum_profit => {
                            if best
                                .as_ref()
                                .is_none_or(|b| quote.net_profit > b.net_profit)
                            {
                                best = Some(quote);
                            }
                        }
                        Ok(quote) => {
                            if closest
                                .as_ref()
                                .is_none_or(|(_, q)| quote.net_profit > q.net_profit)
                            {
                                closest = Some((route, quote));
                            }
                            *rejects.entry("below_net_profit".into()).or_default() += 1;
                        }
                        Err(e) => {
                            *rejects.entry(e.to_string()).or_default() += 1;
                        }
                    }
                }
                if let Some(quote) = best {
                    if last_episode
                        .get(&route.id)
                        .is_some_and(|t| t.elapsed().as_secs() < c.cooldown_secs)
                    {
                        continue;
                    }
                    last_episode.insert(route.id.clone(), Instant::now());
                    let id = uuid::Uuid::new_v4().to_string();
                    journal.event(json!({"event":"candidate","episode_id":id,"route":route,"quote":quote,"books":std::iter::once(&route.input).chain(route.outputs.iter()).filter_map(|l|books.get(&l.token_id)).collect::<Vec<_>>()}))?;
                    candidates.push((id, route.clone(), quote));
                }
            }
            let now_ms = Utc::now().timestamp_millis();
            let exchange_ages = books
                .values()
                .filter_map(|b| b.exchange_timestamp_ms.map(|t| now_ms - t))
                .collect::<Vec<_>>();
            journal.event(json!({"event":"scan_summary","routes":routes.len(),"books":books.len(),"candidates":candidates.len(),"size_rejections":rejects,
                "best_non_candidate":closest.map(|(r,q)|json!({"route_id":r.id,"event_slug":r.slug,"quote":q})),
                "book_health":{"minimum_exchange_age_ms":exchange_ages.iter().min(),"maximum_exchange_age_ms":exchange_ages.iter().max(),"missing_exchange_timestamp":books.len()-exchange_ages.len(),"max_rest_latency_ms":books.values().filter_map(|b|b.request_latency_ms).max()}}))?;
            if last_log.elapsed().as_secs() >= 60 || !candidates.is_empty() || command == "scan" {
                journal.log(&format!(
                    "routes={} books={} candidates={} elapsed_secs={}",
                    routes.len(),
                    books.len(),
                    candidates.len(),
                    started.elapsed().as_secs()
                ))?;
                last_log = Instant::now();
            }
            if !candidates.is_empty() {
                let delay_start = Instant::now();
                let selected = candidates
                    .iter()
                    .map(|(_, r, _)| r.clone())
                    .collect::<Vec<_>>();
                for &delay in &c.delays_ms {
                    tokio::time::sleep(
                        Duration::from_millis(delay).saturating_sub(delay_start.elapsed()),
                    )
                    .await;
                    let delayed = client.fetch_conversion_books(&selected).await;
                    for (id, route, initial) in &candidates {
                        match evaluate(route,&delayed,initial.shares,&c,Some(initial.input_cost)) {
                            Ok(quote)=>journal.event(json!({"event":"delayed_exit","episode_id":id,"event_id":route.event_id,"delay_ms":delay,"actual_delay_ms":delay_start.elapsed().as_millis(),"quote":quote,"full_exit":true,"books":route.outputs.iter().filter_map(|l|delayed.get(&l.token_id)).collect::<Vec<_>>()}))?,
                            Err(e)=>journal.event(json!({"event":"delayed_exit","episode_id":id,"event_id":route.event_id,"delay_ms":delay,"actual_delay_ms":delay_start.elapsed().as_millis(),"full_exit":false,"reason":e.to_string(),"conservative_pnl":-initial.input_cost-c.chain_cost,"residual_inventory_unpriced":true}))?,
                        }
                    }
                }
            }
            if command == "scan"
                || (c.duration_secs > 0 && started.elapsed().as_secs() >= c.duration_secs)
            {
                break;
            }
            tokio::time::sleep(Duration::from_secs(c.poll_secs)).await;
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {r=work=>r?, _=tokio::signal::ctrl_c()=>{}}
    journal.event(json!({"event":"run_stopped"}))?;
    journal.log("stopped; generating report")?;
    report(&c)
}

fn report(c: &Config) -> Result<()> {
    let mut episodes = HashMap::<String, (String, Vec<f64>, usize)>::new();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut malformed = 0;
    let mut first: Option<chrono::DateTime<Utc>> = None;
    let mut last: Option<chrono::DateTime<Utc>> = None;
    let mut scan_rejections = BTreeMap::<String, u64>::new();
    let mut best_non_candidate: Option<Value> = None;
    for line in BufReader::new(File::open(c.root.join("events.ndjson"))?).lines() {
        let line = line?;
        let e: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        let kind = e["event"].as_str().unwrap_or("unknown");
        if let Some(t) = e["recorded_at"]
            .as_str()
            .and_then(|s| s.parse::<chrono::DateTime<Utc>>().ok())
        {
            first = Some(first.map_or(t, |v| v.min(t)));
            last = Some(last.map_or(t, |v| v.max(t)));
        }
        if kind == "scan_summary" {
            if let Some(p) = e["best_non_candidate"]["quote"]["net_profit"].as_f64() {
                if best_non_candidate.as_ref().is_none_or(|v| {
                    p > v["quote"]["net_profit"]
                        .as_f64()
                        .unwrap_or(f64::NEG_INFINITY)
                }) {
                    best_non_candidate = Some(e["best_non_candidate"].clone());
                }
            }
            if let Some(r) = e["size_rejections"].as_object() {
                for (k, v) in r {
                    *scan_rejections.entry(k.clone()).or_default() += v.as_u64().unwrap_or(0);
                }
            }
        }
        *counts.entry(kind.into()).or_default() += 1;
        if kind == "candidate" {
            episodes.insert(
                e["episode_id"].as_str().unwrap_or_default().into(),
                (
                    e["route"]["event_id"].as_str().unwrap_or_default().into(),
                    Vec::new(),
                    0,
                ),
            );
        }
        if kind == "delayed_exit" {
            if let Some(item) = episodes.get_mut(e["episode_id"].as_str().unwrap_or_default()) {
                item.1.push(if e["full_exit"] == true {
                    e["quote"]["net_profit"]
                        .as_f64()
                        .unwrap_or(f64::NEG_INFINITY)
                } else {
                    e["conservative_pnl"].as_f64().unwrap_or(f64::NEG_INFINITY)
                });
                item.2 += usize::from(e["full_exit"] == true);
            }
        }
    }
    let complete = episodes
        .values()
        .filter(|(_, p, _)| p.len() == c.delays_ms.len())
        .collect::<Vec<_>>();
    let robust = complete
        .iter()
        .filter(|(_, p, f)| *f == c.delays_ms.len() && p.iter().all(|p| *p >= c.minimum_profit))
        .count();
    let events = complete
        .iter()
        .filter(|(_, p, f)| *f == c.delays_ms.len() && p.iter().all(|p| *p >= c.minimum_profit))
        .map(|(id, _, _)| id)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let report = json!({"generated_at":Utc::now(),"mode":"economics_only","protocol_verified":false,"live_ready":false,
        "first_recorded_at":first,"last_recorded_at":last,"size_rejections":scan_rejections,"best_non_candidate":best_non_candidate,
        "event_counts":counts,"episodes":episodes.len(),"completed_episodes":complete.len(),"pending_episodes":episodes.len()-complete.len(),
        "robust_episodes":robust,"robust_distinct_events":events,"malformed_lines":malformed,
        "worst_delay_pnl_sum":complete.iter().map(|(_,p,_)|p.iter().copied().fold(f64::INFINITY,f64::min)).sum::<f64>(),
        "next_step":if robust>=20 && events>=3 && malformed==0 && complete.iter().map(|(_,p,_)|p.iter().copied().fold(f64::INFINITY,f64::min)).sum::<f64>()>0. {"REVIEW_ECONOMICS_THEN_VERIFY_CONTRACT_PATH"}else{"INSUFFICIENT_EVIDENCE"},
        "limitations":["Gamma membership is not proof of deployed converter outputs, collateral or contract version.","Conversion fee and latency are assumptions; no conversion is submitted or simulated on-chain.","PnL is hypothetical, not account profit. Each delay is an alternative scenario, not another trade.","Repeated episodes in one event are correlated. Cooldown is not proof of independence.","Incomplete exits use zero residual recovery as a conservative scenario, not a liquidation estimate.","REST polling misses short-lived opportunities; page and route limits bound coverage."],"config":c});
    serde_json::to_writer_pretty(File::create(c.root.join("report.json"))?, &report)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    #[test]
    fn single_no_has_no_cash_bonus_and_requires_every_exit() {
        let c = Config {
            root: PathBuf::new(),
            poll_secs: 1,
            refresh_secs: 1,
            duration_secs: 1,
            delays_ms: vec![1],
            sizes: vec![10.],
            budget: 25.,
            minimum_profit: 0.,
            haircut: 1.,
            slippage_bps: 0.,
            conversion_fee_bps: 0.,
            chain_cost: 0.,
            max_age_ms: 5000,
            max_skew_ms: 2000,
            max_future_skew_ms: 10000,
            cooldown_secs: 1,
        };
        let leg = |token: &str| RelationLeg {
            condition_id: token.into(),
            question: token.into(),
            outcome: super::super::Outcome::Yes,
            token_id: token.into(),
            fee: FeePolicy {
                enabled: false,
                rate: Decimal::ZERO,
                exponent: 1,
            },
        };
        let route = Route {
            id: "r".into(),
            event_id: "e".into(),
            slug: "s".into(),
            input: leg("a"),
            outputs: vec![leg("b"), leg("c")],
            protocol_verified: false,
        };
        let mut books = HashMap::new();
        for (token, p) in [("a", 57), ("b", 34), ("c", 26)] {
            let levels = vec![super::super::BookLevel {
                price: Decimal::new(p, 2),
                size: Decimal::from(100),
            }];
            books.insert(
                token.into(),
                BookSnapshot {
                    token_id: token.into(),
                    observed_at: Utc::now(),
                    exchange_timestamp_ms: None,
                    source: Default::default(),
                    request_latency_ms: None,
                    bids: levels.clone(),
                    asks: levels,
                },
            );
        }
        assert!((evaluate(&route, &books, 10., &c, None).unwrap().net_profit - 0.3).abs() < 1e-8);
        let mut charged = c.clone();
        charged.conversion_fee_bps = 100.;
        assert!(
            (evaluate(&route, &books, 10., &charged, None)
                .unwrap()
                .net_profit
                - 0.24)
                .abs()
                < 1e-8
        );
        let mut shallow = c.clone();
        shallow.haircut = 0.01;
        assert!(evaluate(&route, &books, 10., &shallow, None).is_err());
        let mut delayed = books.clone();
        delayed.remove("a");
        assert!(
            (evaluate(&route, &delayed, 10., &c, Some(5.7))
                .unwrap()
                .net_profit
                - 0.3)
                .abs()
                < 1e-8
        );
        delayed.get_mut("b").unwrap().observed_at = Utc::now() - chrono::Duration::seconds(30);
        assert!(evaluate(&route, &delayed, 10., &c, Some(5.7)).is_err());
        books.remove("c");
        assert!(evaluate(&route, &books, 10., &c, None).is_err());
    }

    #[test]
    fn unknown_enabled_fees_are_not_treated_as_free() {
        let policy = FeePolicy {
            enabled: true,
            rate: Decimal::ZERO,
            exponent: 1,
        };
        assert_eq!(fee(&policy, 0.5, 10.), None);
        let policy = FeePolicy {
            enabled: true,
            rate: Decimal::new(4, 2),
            exponent: 1,
        };
        assert!((fee(&policy, 0.5, 10.).unwrap() - 0.1).abs() < 1e-8);
    }
}
