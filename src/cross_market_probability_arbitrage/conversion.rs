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
    pub discovery_windows: usize,
    pub diagnostic_enabled: bool,
    pub diagnostic_max_exchange_age_ms: i64,
    pub diagnostic_max_rest_ms: u64,
    pub evidence_interval_secs: u64,
    pub evidence_routes: usize,
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
            discovery_windows: env("DISCOVERY_WINDOWS", "3")?,
            diagnostic_enabled: env("DIAGNOSTIC_ENABLED", "false")?,
            diagnostic_max_exchange_age_ms: env("DIAGNOSTIC_MAX_EXCHANGE_AGE_MS", "600000")?,
            diagnostic_max_rest_ms: env("DIAGNOSTIC_MAX_REST_MS", "1000")?,
            evidence_interval_secs: env("EVIDENCE_INTERVAL_SECS", "60")?,
            evidence_routes: env("EVIDENCE_ROUTES", "5")?,
        };
        if c.poll_secs == 0
            || c.refresh_secs == 0
            || c.max_age_ms <= 0
            || c.max_skew_ms <= 0
            || !(0..=30000).contains(&c.max_future_skew_ms)
            || c.cooldown_secs == 0
            || !(1..=20).contains(&c.discovery_windows)
            || !(1..=600000).contains(&c.diagnostic_max_exchange_age_ms)
            || !(1..=5000).contains(&c.diagnostic_max_rest_ms)
            || c.evidence_interval_secs == 0
            || !(1..=20).contains(&c.evidence_routes)
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

/// Diagnostic cohort only. Keep receipt freshness, depth, cost and clock checks;
/// allow bounded old exchange timestamps and compare repeat REST snapshots later.
fn evaluate_diagnostic(
    route: &Route,
    books: &HashMap<String, BookSnapshot>,
    q: f64,
    c: &Config,
    fixed_input: Option<f64>,
) -> Result<Quote> {
    let now = Utc::now().timestamp_millis();
    let mut observed = HashMap::new();
    for leg in std::iter::once(&route.input)
        .filter(|_| fixed_input.is_none())
        .chain(route.outputs.iter())
    {
        let book = books.get(&leg.token_id).context("missing_book")?;
        let ts = book
            .exchange_timestamp_ms
            .context("missing_exchange_timestamp")?;
        if now - ts > c.diagnostic_max_exchange_age_ms {
            bail!("diagnostic_exchange_too_old");
        }
        if ts - now > c.max_future_skew_ms {
            bail!("exchange_clock_ahead");
        }
        if book
            .request_latency_ms
            .is_none_or(|ms| ms > c.diagnostic_max_rest_ms)
        {
            bail!("diagnostic_rest_latency");
        }
        let mut copy = book.clone();
        copy.exchange_timestamp_ms = None;
        observed.insert(leg.token_id.clone(), copy);
    }
    evaluate(route, &observed, q, c, fixed_input)
}

fn evidence(
    route: &Route,
    books: &HashMap<String, BookSnapshot>,
    audit: &BTreeMap<String, Value>,
    c: &Config,
) -> Value {
    let now = Utc::now().timestamp_millis();
    json!({"route_id":route.id,"event_slug":route.slug,"legs":std::iter::once(&route.input).chain(route.outputs.iter()).map(|leg| {
        let b = books.get(&leg.token_id);
        json!({"token_id":leg.token_id,"is_input":leg.token_id==route.input.token_id,
            "freshness_error":fresh(books,leg,now,c).err().map(|e|e.to_string()),
            "observed_at":b.map(|b|b.observed_at),"exchange_age_ms":b.and_then(|b|b.exchange_timestamp_ms).map(|ts|now-ts),
            "request":audit.get(&leg.token_id),"bids":b.map(|b|b.bids.iter().take(5).collect::<Vec<_>>()),
            "asks":b.map(|b|b.asks.iter().take(5).collect::<Vec<_>>())})
    }).collect::<Vec<_>>()})
}

fn raw_spread(
    route: &Route,
    books: &HashMap<String, BookSnapshot>,
    shares: f64,
    c: &Config,
) -> Option<f64> {
    let mut raw = c.clone();
    raw.slippage_bps = 0.;
    let mut input = route.input.clone();
    input.fee.enabled = false;
    let cost = walk(books.get(&input.token_id)?, &input, shares, true, &raw)?;
    let mut proceeds = 0.;
    for output in &route.outputs {
        let mut leg = output.clone();
        leg.fee.enabled = false;
        proceeds += walk(
            books.get(&leg.token_id)?,
            &leg,
            shares * (1. - c.conversion_fee_bps / 10000.),
            false,
            &raw,
        )?;
    }
    Some(proceeds - cost)
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
    journal.event(json!({"event":"run_started","config":c,"mode":"read_only_shadow","protocol_verified":false,"stage":"economics_only",
        "discovery_parameters":{"event_pages":client.config().event_pages,"events_per_page":client.config().events_per_page,
            "max_routes":client.config().max_relationships,"max_outcomes":client.config().max_partition_legs,"request_concurrency":client.config().request_concurrency}}))?;
    let started = Instant::now();
    let mut refresh = Instant::now();
    let mut routes = Vec::new();
    let mut initialized = false;
    let mut last_episode = HashMap::<String, Instant>::new();
    let mut last_log = Instant::now();
    let mut discovery_cycle = 0;
    let mut last_evidence: Option<Instant> = None;
    let mut evidence_cursor = 0;
    let work = async {
        loop {
            if c.duration_secs > 0 && started.elapsed().as_secs() >= c.duration_secs {
                break;
            }
            if !initialized || refresh.elapsed().as_secs() >= c.refresh_secs {
                let offset = (discovery_cycle % c.discovery_windows) * client.config().event_pages;
                match client.discover_conversion_routes_at(offset).await {
                    Ok((new, diagnostics)) => {
                        routes = new;
                        discovery_cycle += 1;
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
            let book_audit = client.conversion_book_audit();
            if last_evidence.is_none_or(|t| t.elapsed().as_secs() >= c.evidence_interval_secs) {
                let sampled = (0..c.evidence_routes.min(routes.len()))
                    .map(|i| &routes[(evidence_cursor + i) % routes.len()])
                    .collect::<Vec<_>>();
                journal.event(json!({"event":"book_evidence","sampled_routes":sampled.iter().map(|r|evidence(r,&books,&book_audit,&c)).collect::<Vec<_>>(),"truncated_levels":5}))?;
                evidence_cursor = evidence_cursor.wrapping_add(c.evidence_routes);
                last_evidence = Some(Instant::now());
            }
            let mut rejects = BTreeMap::<String, usize>::new();
            let mut diagnostic_rejects = BTreeMap::<String, usize>::new();
            let mut by_event = BTreeMap::<String, BTreeMap<String, usize>>::new();
            let mut leg_time_failures = BTreeMap::<String, usize>::new();
            let mut candidates = Vec::new();
            let mut closest: Option<(&Route, Quote)> = None;
            let mut diagnostic_closest: Option<(&Route, Quote)> = None;
            for route in &routes {
                for leg in std::iter::once(&route.input).chain(route.outputs.iter()) {
                    if let Err(error) = fresh(&books, leg, Utc::now().timestamp_millis(), &c) {
                        *leg_time_failures.entry(error.to_string()).or_default() += 1;
                    }
                }
                let mut best: Option<Quote> = None;
                let mut diagnostic_best: Option<Quote> = None;
                for &q in &c.sizes {
                    match evaluate(route, &books, q, &c, None) {
                        Ok(quote) if quote.net_profit >= c.minimum_profit => {
                            *by_event
                                .entry(route.slug.clone())
                                .or_default()
                                .entry("strict_profitable".into())
                                .or_default() += 1;
                            if best
                                .as_ref()
                                .is_none_or(|b| quote.net_profit > b.net_profit)
                            {
                                best = Some(quote);
                            }
                        }
                        Ok(quote) => {
                            *by_event
                                .entry(route.slug.clone())
                                .or_default()
                                .entry("below_net_profit".into())
                                .or_default() += 1;
                            if closest
                                .as_ref()
                                .is_none_or(|(_, q)| quote.net_profit > q.net_profit)
                            {
                                closest = Some((route, quote));
                            }
                            *rejects.entry("below_net_profit".into()).or_default() += 1;
                        }
                        Err(e) => {
                            *by_event
                                .entry(route.slug.clone())
                                .or_default()
                                .entry(e.to_string())
                                .or_default() += 1;
                            *rejects.entry(e.to_string()).or_default() += 1;
                        }
                    }
                    if c.diagnostic_enabled {
                        match evaluate_diagnostic(route, &books, q, &c, None) {
                            Ok(quote) => {
                                *by_event
                                    .entry(route.slug.clone())
                                    .or_default()
                                    .entry("diagnostic_priced".into())
                                    .or_default() += 1;
                                if diagnostic_closest
                                    .as_ref()
                                    .is_none_or(|(_, v)| quote.net_profit > v.net_profit)
                                {
                                    diagnostic_closest = Some((route, quote.clone()));
                                }
                                if quote.net_profit >= c.minimum_profit
                                    && diagnostic_best
                                        .as_ref()
                                        .is_none_or(|v| quote.net_profit > v.net_profit)
                                {
                                    diagnostic_best = Some(quote);
                                } else if quote.net_profit < c.minimum_profit {
                                    *diagnostic_rejects
                                        .entry("below_net_profit".into())
                                        .or_default() += 1;
                                }
                            }
                            Err(e) => {
                                *diagnostic_rejects.entry(e.to_string()).or_default() += 1;
                            }
                        }
                    }
                }
                if let Some(quote) = diagnostic_best {
                    let key = format!("diagnostic:{}", route.id);
                    if last_episode
                        .get(&key)
                        .is_none_or(|t| t.elapsed().as_secs() >= c.cooldown_secs)
                    {
                        last_episode.insert(key, Instant::now());
                        let id = uuid::Uuid::new_v4().to_string();
                        journal.event(json!({"event":"diagnostic_candidate","episode_id":id,"route":route,"quote":quote,
                            "raw_spread_before_trading_costs":raw_spread(route,&books,quote.shares,&c),"evidence":evidence(route,&books,&book_audit,&c),
                            "books":std::iter::once(&route.input).chain(route.outputs.iter()).filter_map(|l|books.get(&l.token_id)).collect::<Vec<_>>(),"execution_verified":false}))?;
                        candidates.push((id, route.clone(), quote, true));
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
                    journal.event(json!({"event":"candidate","episode_id":id,"route":route,"quote":quote,
                        "raw_spread_before_trading_costs":raw_spread(route,&books,quote.shares,&c),"evidence":evidence(route,&books,&book_audit,&c),
                        "books":std::iter::once(&route.input).chain(route.outputs.iter()).filter_map(|l|books.get(&l.token_id)).collect::<Vec<_>>()}))?;
                    candidates.push((id, route.clone(), quote, false));
                }
            }
            let now_ms = Utc::now().timestamp_millis();
            let exchange_ages = books
                .values()
                .filter_map(|b| b.exchange_timestamp_ms.map(|t| now_ms - t))
                .collect::<Vec<_>>();
            journal.event(json!({"event":"scan_summary","routes":routes.len(),"books":books.len(),"candidates":candidates.iter().filter(|(_,_,_,d)|!*d).count(),"size_rejections":rejects,
                "by_event":by_event,"overlapping_leg_time_failures":leg_time_failures,"diagnostic_rejections":diagnostic_rejects,"diagnostic_candidates":candidates.iter().filter(|(_,_,_,d)|*d).count(),
                "diagnostic_best":diagnostic_closest.map(|(r,q)|json!({"route_id":r.id,"event_slug":r.slug,"raw_spread_before_trading_costs":raw_spread(r,&books,q.shares,&c),"quote":q})),
                "best_non_candidate":closest.map(|(r,q)|json!({"route_id":r.id,"event_slug":r.slug,"raw_spread_before_trading_costs":raw_spread(r,&books,q.shares,&c),"quote":q})),
                "book_health":{"minimum_exchange_age_ms":exchange_ages.iter().min(),"maximum_exchange_age_ms":exchange_ages.iter().max(),"missing_exchange_timestamp":books.len()-exchange_ages.len(),"max_rest_latency_ms":books.values().filter_map(|b|b.request_latency_ms).max()}}))?;
            if last_log.elapsed().as_secs() >= 60 || !candidates.is_empty() || command == "scan" {
                journal.log(&format!(
                    "routes={} books={} strict_candidates={} diagnostic_candidates={} elapsed_secs={}",
                    routes.len(),
                    books.len(),
                    candidates.iter().filter(|(_,_,_,d)| !*d).count(),
                    candidates.iter().filter(|(_,_,_,d)| *d).count(),
                    started.elapsed().as_secs()
                ))?;
                last_log = Instant::now();
            }
            if !candidates.is_empty() {
                let delay_start = Instant::now();
                let selected = candidates
                    .iter()
                    .map(|(_, r, _, _)| r.clone())
                    .collect::<Vec<_>>();
                for &delay in &c.delays_ms {
                    tokio::time::sleep(
                        Duration::from_millis(delay).saturating_sub(delay_start.elapsed()),
                    )
                    .await;
                    let delayed = client.fetch_conversion_books(&selected).await;
                    let delayed_audit = client.conversion_book_audit();
                    for (id, route, initial, diagnostic) in &candidates {
                        let kind = if *diagnostic {
                            "diagnostic_delayed_exit"
                        } else {
                            "delayed_exit"
                        };
                        let result = if *diagnostic {
                            evaluate_diagnostic(
                                route,
                                &delayed,
                                initial.shares,
                                &c,
                                Some(initial.input_cost),
                            )
                        } else {
                            evaluate(
                                route,
                                &delayed,
                                initial.shares,
                                &c,
                                Some(initial.input_cost),
                            )
                        };
                        let repeated = evidence(route, &delayed, &delayed_audit, &c);
                        match result {
                            Ok(quote)=>journal.event(json!({"event":kind,"episode_id":id,"event_id":route.event_id,"delay_ms":delay,"actual_delay_ms":delay_start.elapsed().as_millis(),"quote":quote,"full_exit":true,"repeat_evidence":repeated,"books":route.outputs.iter().filter_map(|l|delayed.get(&l.token_id)).collect::<Vec<_>>()}))?,
                            Err(e)=>journal.event(json!({"event":kind,"episode_id":id,"event_id":route.event_id,"delay_ms":delay,"actual_delay_ms":delay_start.elapsed().as_millis(),"full_exit":false,"reason":e.to_string(),"repeat_evidence":repeated,"conservative_pnl":-initial.input_cost-c.chain_cost,"residual_inventory_unpriced":true}))?,
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
    let mut diagnostic_episodes = HashMap::<String, (String, Vec<f64>, usize)>::new();
    let mut unique_events = BTreeMap::<String, String>::new();
    let mut unique_routes = std::collections::BTreeSet::new();
    let mut discovery_totals = BTreeMap::<String, u64>::new();
    let mut diagnostic_rejections = BTreeMap::<String, u64>::new();
    let mut by_event = BTreeMap::<String, BTreeMap<String, u64>>::new();
    let mut leg_failures = BTreeMap::<String, u64>::new();
    let mut diagnostic_best: Option<Value> = None;
    let mut health = BTreeMap::<String, Vec<i64>>::new();
    let mut priced_scans = 0u64;
    let mut estimated_evaluations = 0u64;
    let mut strict_profitable_evaluations = 0u64;
    let mut initial_hashes = HashMap::<String, BTreeMap<String, String>>::new();
    let mut repeat_hash_checks = BTreeMap::<String, u64>::new();
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
        if kind == "diagnostic_candidate" {
            let hashes = e["evidence"]["legs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|leg| {
                    Some((
                        leg["token_id"].as_str()?.to_string(),
                        leg["request"]["hash"].as_str()?.to_string(),
                    ))
                })
                .collect();
            initial_hashes.insert(e["episode_id"].as_str().unwrap_or_default().into(), hashes);
        }
        if kind == "diagnostic_delayed_exit" {
            for leg in e["repeat_evidence"]["legs"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let key = match (
                    initial_hashes
                        .get(e["episode_id"].as_str().unwrap_or_default())
                        .and_then(|h| h.get(leg["token_id"].as_str().unwrap_or_default())),
                    leg["request"]["hash"].as_str(),
                ) {
                    (Some(a), Some(b)) if a == b => "same_hash",
                    (Some(_), Some(_)) => "changed_hash",
                    _ => "missing_hash_or_response",
                };
                *repeat_hash_checks.entry(key.into()).or_default() += 1;
            }
        }
        if kind == "discovery" {
            if let Some(routes) = e["routes"].as_array() {
                for r in routes {
                    if let (Some(id), Some(slug), Some(route_id)) =
                        (r["event_id"].as_str(), r["slug"].as_str(), r["id"].as_str())
                    {
                        unique_events.insert(id.into(), slug.into());
                        unique_routes.insert(route_id.to_string());
                    }
                }
            }
            merge_counts(&mut discovery_totals, &e["diagnostics"]);
        }
        if let Some(t) = e["recorded_at"]
            .as_str()
            .and_then(|s| s.parse::<chrono::DateTime<Utc>>().ok())
        {
            first = Some(first.map_or(t, |v| v.min(t)));
            last = Some(last.map_or(t, |v| v.max(t)));
        }
        if kind == "scan_summary" {
            estimated_evaluations += e["routes"].as_u64().unwrap_or(0) * c.sizes.len() as u64;
            priced_scans += u64::from(
                e["best_non_candidate"].is_object() || e["candidates"].as_u64().unwrap_or(0) > 0,
            );
            merge_counts(&mut diagnostic_rejections, &e["diagnostic_rejections"]);
            merge_counts(&mut leg_failures, &e["overlapping_leg_time_failures"]);
            if let Some(events) = e["by_event"].as_object() {
                for (slug, counts) in events {
                    strict_profitable_evaluations +=
                        counts["strict_profitable"].as_u64().unwrap_or(0);
                    merge_counts(by_event.entry(slug.clone()).or_default(), counts);
                }
            }
            if let Some(values) = e["book_health"].as_object() {
                for (k, v) in values {
                    if let Some(v) = v.as_i64() {
                        health.entry(k.clone()).or_default().push(v);
                    }
                }
            }
            if let Some(p) = e["diagnostic_best"]["quote"]["net_profit"].as_f64() {
                if diagnostic_best.as_ref().is_none_or(|v| {
                    p > v["quote"]["net_profit"]
                        .as_f64()
                        .unwrap_or(f64::NEG_INFINITY)
                }) {
                    diagnostic_best = Some(e["diagnostic_best"].clone());
                }
            }
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
        if matches!(kind, "candidate" | "diagnostic_candidate") {
            let destination = if kind == "candidate" {
                &mut episodes
            } else {
                &mut diagnostic_episodes
            };
            destination.insert(
                e["episode_id"].as_str().unwrap_or_default().into(),
                (
                    e["route"]["event_id"].as_str().unwrap_or_default().into(),
                    Vec::new(),
                    0,
                ),
            );
        }
        if matches!(kind, "delayed_exit" | "diagnostic_delayed_exit") {
            let destination = if kind == "delayed_exit" {
                &mut episodes
            } else {
                &mut diagnostic_episodes
            };
            if let Some(item) = destination.get_mut(e["episode_id"].as_str().unwrap_or_default()) {
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
    let diagnostic_complete = diagnostic_episodes
        .values()
        .filter(|(_, p, _)| p.len() == c.delays_ms.len())
        .collect::<Vec<_>>();
    let diagnostic_robust = diagnostic_complete
        .iter()
        .filter(|(_, p, f)| *f == c.delays_ms.len() && p.iter().all(|p| *p >= c.minimum_profit))
        .count();
    let health_summary=health.into_iter().map(|(k,mut values)|{
        values.sort_unstable(); let n=values.len();
        (k,json!({"samples":n,"min":values[0],"median":values[n/2],"p95":values[((n-1) as f64*0.95).ceil() as usize],"max":values[n-1]}))
    }).collect::<BTreeMap<_,_>>();
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
        "coverage":{"unique_events":unique_events.len(),"unique_routes":unique_routes.len(),"events":unique_events,
            "scans_with_strict_priced_route":priced_scans,"estimated_size_evaluations":estimated_evaluations,
            "strict_priced_evaluations":scan_rejections.get("below_net_profit").copied().unwrap_or(0)+strict_profitable_evaluations},
        "discovery_totals_repeated_observations":discovery_totals,"by_event_size_checks":by_event,"overlapping_leg_time_failures":leg_failures,
        "book_health_per_scan_distribution":health_summary,
        "diagnostic_only":{"episodes":diagnostic_episodes.len(),"completed_episodes":diagnostic_complete.len(),"pending_episodes":diagnostic_episodes.len()-diagnostic_complete.len(),
            "all_delay_positive_episodes":diagnostic_robust,"best_observed_quote":diagnostic_best,"size_rejections":diagnostic_rejections,
            "worst_delay_pnl_sum":diagnostic_complete.iter().map(|(_,p,_)|p.iter().copied().fold(f64::INFINITY,f64::min)).sum::<f64>(),
            "repeat_leg_hash_checks":repeat_hash_checks,"counts_toward_promotion":false,"execution_verified":false},
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

fn merge_counts(destination: &mut BTreeMap<String, u64>, source: &Value) {
    if let Some(values) = source.as_object() {
        for (key, value) in values {
            if let Some(value) = value.as_u64() {
                *destination.entry(key.clone()).or_default() += value;
            }
        }
    }
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
            discovery_windows: 1,
            diagnostic_enabled: true,
            diagnostic_max_exchange_age_ms: 600000,
            diagnostic_max_rest_ms: 1000,
            evidence_interval_secs: 60,
            evidence_routes: 5,
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
        for book in books.values_mut() {
            book.exchange_timestamp_ms = Some(Utc::now().timestamp_millis() - 60000);
            book.request_latency_ms = Some(30);
        }
        assert!(evaluate(&route, &books, 10., &c, None).is_err());
        assert!(
            evaluate_diagnostic(&route, &books, 10., &c, None)
                .unwrap()
                .net_profit
                > 0.
        );
        books.get_mut("b").unwrap().request_latency_ms = Some(2000);
        assert!(evaluate_diagnostic(&route, &books, 10., &c, None).is_err());
        books.get_mut("b").unwrap().request_latency_ms = Some(30);
        books.get_mut("b").unwrap().observed_at = Utc::now() - chrono::Duration::seconds(30);
        assert!(evaluate_diagnostic(&route, &books, 10., &c, None).is_err());
        books.get_mut("b").unwrap().observed_at = Utc::now();
        books.get_mut("b").unwrap().exchange_timestamp_ms =
            Some(Utc::now().timestamp_millis() - 700000);
        assert!(evaluate_diagnostic(&route, &books, 10., &c, None).is_err());
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

    #[test]
    fn diagnostic_profit_never_promotes_strict_research() {
        let mut c = Config::load().unwrap();
        c.root = std::env::temp_dir().join(format!("conversion-report-{}", uuid::Uuid::new_v4()));
        c.minimum_profit = 0.05;
        let mut journal = Journal::open(&c).unwrap();
        for id in 0..21 {
            journal.event(json!({"event":"diagnostic_candidate","episode_id":id.to_string(),"route":{"event_id":(id%3).to_string()}})).unwrap();
            for delay in &c.delays_ms {
                journal.event(json!({"event":"diagnostic_delayed_exit","episode_id":id.to_string(),"delay_ms":delay,"full_exit":true,"quote":{"net_profit":0.1}})).unwrap();
            }
        }
        report(&c).unwrap();
        let result: Value =
            serde_json::from_reader(File::open(c.root.join("report.json")).unwrap()).unwrap();
        assert_eq!(result["diagnostic_only"]["all_delay_positive_episodes"], 21);
        assert_eq!(result["episodes"], 0);
        assert_eq!(result["next_step"], "INSUFFICIENT_EVIDENCE");
        drop(journal);
        std::fs::remove_file(c.root.join("events.ndjson")).unwrap();
        std::fs::remove_file(c.root.join("runtime.log")).unwrap();
        std::fs::remove_file(c.root.join("report.json")).unwrap();
        std::fs::remove_dir(c.root).unwrap();
    }
}
