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
    collections::{BTreeMap, HashMap, HashSet},
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
    #[serde(default)]
    pub event_title: String,
    #[serde(default)]
    pub market_count: usize,
    #[serde(default)]
    pub fee_enabled_legs: usize,
    #[serde(default)]
    pub neg_risk_other_markets: usize,
    #[serde(default)]
    pub earliest_deadline_at: Option<chrono::DateTime<Utc>>,
    #[serde(default)]
    pub latest_deadline_at: Option<chrono::DateTime<Utc>>,
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
    pub focus_enabled: bool,
    pub focus_net_profit_floor: f64,
    pub focus_delay_ms: u64,
    pub focus_routes: usize,
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
            focus_enabled: env("FOCUS_ENABLED", "true")?,
            focus_net_profit_floor: env("FOCUS_NET_PROFIT_FLOOR_USDC", "-0.10")?,
            focus_delay_ms: env("FOCUS_DELAY_MS", "2000")?,
            focus_routes: env("FOCUS_ROUTES", "5")?,
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
            || !(250..=30000).contains(&c.focus_delay_ms)
            || !(1..=20).contains(&c.focus_routes)
            || ![
                c.budget,
                c.minimum_profit,
                c.haircut,
                c.slippage_bps,
                c.conversion_fee_bps,
                c.chain_cost,
                c.focus_net_profit_floor,
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

#[derive(Clone, Debug, Serialize)]
struct RouteResearchPriority {
    route_id: String,
    event_slug: String,
    required_legs: usize,
    books_present: usize,
    strict_fresh_legs: usize,
    top_levels_present: usize,
    minimum_top_depth_shares: Option<f64>,
    raw_top_spread_per_share: Option<f64>,
    fee_enabled_legs: usize,
    score: f64,
}

fn route_research_priority(
    route: &Route,
    books: &HashMap<String, BookSnapshot>,
    c: &Config,
) -> RouteResearchPriority {
    let now = Utc::now().timestamp_millis();
    let legs = std::iter::once(&route.input)
        .chain(route.outputs.iter())
        .collect::<Vec<_>>();
    let books_present = legs
        .iter()
        .filter(|leg| books.contains_key(&leg.token_id))
        .count();
    let strict_fresh_legs = legs
        .iter()
        .filter(|leg| fresh(books, leg, now, c).is_ok())
        .count();
    let input_top = books.get(&route.input.token_id).and_then(|book| {
        book.asks
            .iter()
            .filter_map(|level| Some((level.price.to_f64()?, level.size.to_f64()?)))
            .filter(|(price, size)| (0.0..1.0).contains(price) && *size > 0.)
            .min_by(|a, b| a.0.total_cmp(&b.0))
    });
    let output_tops = route
        .outputs
        .iter()
        .filter_map(|leg| {
            books
                .get(&leg.token_id)?
                .bids
                .iter()
                .filter_map(|level| Some((level.price.to_f64()?, level.size.to_f64()?)))
                .filter(|(price, size)| (0.0..1.0).contains(price) && *size > 0.)
                .max_by(|a, b| a.0.total_cmp(&b.0))
        })
        .collect::<Vec<_>>();
    let top_levels_present = usize::from(input_top.is_some()) + output_tops.len();
    let complete_tops = input_top.is_some() && output_tops.len() == route.outputs.len();
    let minimum_top_depth_shares = complete_tops.then(|| {
        std::iter::once(input_top.unwrap().1)
            .chain(output_tops.iter().map(|(_, size)| *size))
            .fold(f64::INFINITY, f64::min)
            * c.haircut
    });
    let raw_top_spread_per_share = complete_tops
        .then(|| output_tops.iter().map(|(price, _)| *price).sum::<f64>() - input_top.unwrap().0);
    let required_legs = legs.len();
    let score = (books_present == required_legs) as u8 as f64 * 1_000_000.
        + strict_fresh_legs as f64 * 10_000.
        + complete_tops as u8 as f64 * 1_000.
        + minimum_top_depth_shares.unwrap_or(0.).min(1_000.)
        + raw_top_spread_per_share.unwrap_or(-1.) * 100.
        - route.fee_enabled_legs as f64;
    RouteResearchPriority {
        route_id: route.id.clone(),
        event_slug: route.slug.clone(),
        required_legs,
        books_present,
        strict_fresh_legs,
        top_levels_present,
        minimum_top_depth_shares,
        raw_top_spread_per_share,
        fee_enabled_legs: route.fee_enabled_legs,
        score,
    }
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

fn sensitivity_quote(
    route: &Route,
    books: &HashMap<String, BookSnapshot>,
    shares: f64,
    c: &Config,
    diagnostic: bool,
    fees: bool,
    slippage_bps: f64,
    conversion_fee_bps: f64,
    chain_cost: f64,
) -> Option<Quote> {
    let mut route = route.clone();
    if !fees {
        route.input.fee.enabled = false;
        for output in &mut route.outputs {
            output.fee.enabled = false;
        }
    }
    let mut scenario = c.clone();
    scenario.slippage_bps = slippage_bps;
    scenario.conversion_fee_bps = conversion_fee_bps;
    scenario.chain_cost = chain_cost;
    let result = if diagnostic {
        evaluate_diagnostic(&route, books, shares, &scenario, None)
    } else {
        evaluate(&route, books, shares, &scenario, None)
    };
    result.ok()
}

/// Attribute the observed spread loss in a fixed order. Components are
/// incremental scenario differences, so they sum back to configured PnL.
fn cost_sensitivity(
    route: &Route,
    books: &HashMap<String, BookSnapshot>,
    shares: f64,
    c: &Config,
    diagnostic: bool,
) -> Value {
    let no_cost = sensitivity_quote(route, books, shares, c, diagnostic, false, 0., 0., 0.);
    let fees_only = sensitivity_quote(route, books, shares, c, diagnostic, true, 0., 0., 0.);
    let fees_slippage = sensitivity_quote(
        route,
        books,
        shares,
        c,
        diagnostic,
        true,
        c.slippage_bps,
        0.,
        0.,
    );
    let zero_chain = sensitivity_quote(
        route,
        books,
        shares,
        c,
        diagnostic,
        true,
        c.slippage_bps,
        c.conversion_fee_bps,
        0.,
    );
    let configured = sensitivity_quote(
        route,
        books,
        shares,
        c,
        diagnostic,
        true,
        c.slippage_bps,
        c.conversion_fee_bps,
        c.chain_cost,
    );
    let double_slippage = sensitivity_quote(
        route,
        books,
        shares,
        c,
        diagnostic,
        true,
        c.slippage_bps * 2.,
        c.conversion_fee_bps,
        c.chain_cost,
    );
    let pnl = |quote: &Option<Quote>| quote.as_ref().map(|value| value.net_profit);
    json!({
        "shares": shares,
        "scenario_net_pnl_usdc": {
            "no_fees_slippage_conversion_or_chain": pnl(&no_cost),
            "fees_only": pnl(&fees_only),
            "fees_and_slippage": pnl(&fees_slippage),
            "configured_without_chain": pnl(&zero_chain),
            "configured": pnl(&configured),
            "double_slippage": pnl(&double_slippage)
        },
        "incremental_cost_usdc": {
            "market_fees": pnl(&no_cost).zip(pnl(&fees_only)).map(|(a,b)|a-b),
            "slippage": pnl(&fees_only).zip(pnl(&fees_slippage)).map(|(a,b)|a-b),
            "conversion_fee": pnl(&fees_slippage).zip(pnl(&zero_chain)).map(|(a,b)|a-b),
            "chain": pnl(&zero_chain).zip(pnl(&configured)).map(|(a,b)|a-b),
            "extra_slippage_stress": pnl(&configured).zip(pnl(&double_slippage)).map(|(a,b)|a-b)
        },
        "assumptions": {"slippage_bps":c.slippage_bps,"conversion_fee_bps":c.conversion_fee_bps,"chain_cost_usdc":c.chain_cost}
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
        tracing::info!(target: "polypulse::strategy::neg_risk_conversion_arbitrage", "{message}");
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
        "build":{"package_version":env!("CARGO_PKG_VERSION"),"git_commit":option_env!("POLYPULSE_GIT_COMMIT"),"research_schema_version":2},
        "discovery_parameters":{"event_pages":client.config().event_pages,"events_per_page":client.config().events_per_page,
            "max_routes":client.config().max_relationships,"max_outcomes":client.config().max_partition_legs,"request_concurrency":client.config().request_concurrency}}))?;
    let started = Instant::now();
    let mut refresh = Instant::now();
    let mut routes = Vec::new();
    let mut initialized = false;
    let mut last_episode = HashMap::<String, Instant>::new();
    let mut last_log = Instant::now();
    let mut discovery_cycle = 0;
    let mut next_discovery = Instant::now();
    let mut discovery_failures = 0u32;
    let mut route_cursors = HashMap::<usize, usize>::new();
    let mut discovery_routes_seen = HashSet::<String>::new();
    let mut last_evidence: Option<Instant> = None;
    let mut evidence_cursor = 0;
    let work = async {
        loop {
            if c.duration_secs > 0 && started.elapsed().as_secs() >= c.duration_secs {
                break;
            }
            if Instant::now() >= next_discovery {
                let offset = (discovery_cycle % c.discovery_windows) * client.config().event_pages;
                let route_offset = route_cursors.get(&offset).copied().unwrap_or(0);
                match client
                    .discover_conversion_routes_at_with_route_offset(offset, route_offset)
                    .await
                {
                    Ok((new, mut diagnostics)) => {
                        let routes_before = diagnostics
                            .get("routes_before_limit")
                            .copied()
                            .unwrap_or(new.len());
                        let emitted = new.len();
                        let next_route_offset = if routes_before > emitted && routes_before > 0 {
                            (route_offset + emitted) % routes_before
                        } else {
                            0
                        };
                        route_cursors.insert(offset, next_route_offset);
                        let newly_seen = new
                            .iter()
                            .filter(|route| discovery_routes_seen.insert(route.id.clone()))
                            .count();
                        diagnostics.insert("routes_new_to_run".into(), newly_seen);
                        diagnostics.insert(
                            "routes_unique_seen_in_run".into(),
                            discovery_routes_seen.len(),
                        );
                        diagnostics.insert("next_route_selection_offset".into(), next_route_offset);
                        routes = new;
                        discovery_cycle += 1;
                        initialized = true;
                        refresh = Instant::now();
                        next_discovery = refresh + Duration::from_secs(c.refresh_secs);
                        discovery_failures = 0;
                        journal.event(
                            json!({"event":"discovery","diagnostics":diagnostics,"routes":routes,
                                "selection_policy":"rotating_within_page_window","page_offset":offset}),
                        )?;
                        journal.log(&format!(
                            "discovered {} conversion routes; protocol mapping remains unverified",
                            routes.len()
                        ))?;
                    }
                    Err(e) => {
                        discovery_failures = discovery_failures.saturating_add(1);
                        let retry_secs = discovery_retry_secs(discovery_failures);
                        next_discovery = Instant::now() + Duration::from_secs(retry_secs);
                        journal.event(
                            json!({"event":"error","phase":"discovery","message":format!("{e:#}"),
                                "retry_secs":retry_secs,"consecutive_failures":discovery_failures,
                                "window_page":offset,"retained_routes":routes.len(),
                                "route_age_secs":initialized.then(||refresh.elapsed().as_secs())}),
                        )?;
                        journal.log(&format!(
                            "discovery failed: {e:#}; retry in {retry_secs}s; retained_routes={}",
                            routes.len()
                        ))?;
                        if command == "scan" {
                            return Err(e);
                        }
                    }
                }
            }
            // Reuse discovery metadata for at most two refresh intervals. Every
            // iteration still fetches current books and applies strict freshness gates.
            if !initialized || refresh.elapsed().as_secs() >= c.refresh_secs.saturating_mul(2) {
                journal.event(json!({"event":"sampling_unavailable","reason":"discovery_unavailable_or_expired",
                    "interval_secs":c.poll_secs}))?;
                tokio::time::sleep(Duration::from_secs(c.poll_secs)).await;
                continue;
            }
            let books = client.fetch_conversion_books(&routes).await;
            let book_audit = client.conversion_book_audit();
            let mut route_ranking = routes
                .iter()
                .map(|route| (route_research_priority(route, &books, &c), route))
                .collect::<Vec<_>>();
            route_ranking.sort_by(|(a, _), (b, _)| b.score.total_cmp(&a.score));
            if last_evidence.is_none_or(|t| t.elapsed().as_secs() >= c.evidence_interval_secs) {
                let target = c.evidence_routes.min(routes.len());
                let priority_count = target.div_ceil(2);
                let mut sampled = route_ranking
                    .iter()
                    .take(priority_count)
                    .map(|(_, route)| *route)
                    .collect::<Vec<_>>();
                let mut sampled_ids = sampled
                    .iter()
                    .map(|route| route.id.clone())
                    .collect::<HashSet<_>>();
                for i in 0..routes.len() {
                    if sampled.len() >= target {
                        break;
                    }
                    let route = &routes[(evidence_cursor + i) % routes.len()];
                    if sampled_ids.insert(route.id.clone()) {
                        sampled.push(route);
                    }
                }
                journal.event(json!({"event":"book_evidence","selection_policy":"half_priority_half_rotation",
                    "route_ranking":route_ranking.iter().take(10).map(|(priority,_)|priority).collect::<Vec<_>>(),
                    "sampled_routes":sampled.iter().map(|r|evidence(r,&books,&book_audit,&c)).collect::<Vec<_>>(),"truncated_levels":5}))?;
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
            let mut focused_routes = Vec::<(Route, Quote)>::new();
            for route in &routes {
                for leg in std::iter::once(&route.input).chain(route.outputs.iter()) {
                    if let Err(error) = fresh(&books, leg, Utc::now().timestamp_millis(), &c) {
                        *leg_time_failures.entry(error.to_string()).or_default() += 1;
                    }
                }
                let mut best: Option<Quote> = None;
                let mut diagnostic_best: Option<Quote> = None;
                let mut route_diagnostic_closest: Option<Quote> = None;
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
                                if route_diagnostic_closest
                                    .as_ref()
                                    .is_none_or(|v| quote.net_profit > v.net_profit)
                                {
                                    route_diagnostic_closest = Some(quote.clone());
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
                if c.focus_enabled {
                    if let Some(quote) = route_diagnostic_closest
                        .filter(|q| q.net_profit >= c.focus_net_profit_floor)
                    {
                        focused_routes.push((route.clone(), quote));
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
                            "cost_sensitivity":cost_sensitivity(route,&books,quote.shares,&c,true),
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
                        "cost_sensitivity":cost_sensitivity(route,&books,quote.shares,&c,false),
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
                "route_research_ranking":route_ranking.iter().take(10).map(|(priority,_)|priority).collect::<Vec<_>>(),
                "by_event":by_event,"overlapping_leg_time_failures":leg_time_failures,"diagnostic_rejections":diagnostic_rejects,"diagnostic_candidates":candidates.iter().filter(|(_,_,_,d)|*d).count(),
                "diagnostic_best":diagnostic_closest.map(|(r,q)|json!({"route_id":r.id,"event_slug":r.slug,"route_metadata":{"event_title":r.event_title,"market_count":r.market_count,"fee_enabled_legs":r.fee_enabled_legs,"neg_risk_other_markets":r.neg_risk_other_markets,"earliest_deadline_at":r.earliest_deadline_at,"latest_deadline_at":r.latest_deadline_at},"raw_spread_before_trading_costs":raw_spread(r,&books,q.shares,&c),"cost_sensitivity":cost_sensitivity(r,&books,q.shares,&c,true),"quote":q})),
                "best_non_candidate":closest.map(|(r,q)|json!({"route_id":r.id,"event_slug":r.slug,"route_metadata":{"event_title":r.event_title,"market_count":r.market_count,"fee_enabled_legs":r.fee_enabled_legs,"neg_risk_other_markets":r.neg_risk_other_markets,"earliest_deadline_at":r.earliest_deadline_at,"latest_deadline_at":r.latest_deadline_at},"raw_spread_before_trading_costs":raw_spread(r,&books,q.shares,&c),"cost_sensitivity":cost_sensitivity(r,&books,q.shares,&c,false),"quote":q})),
                "book_health":{"minimum_exchange_age_ms":exchange_ages.iter().min(),"maximum_exchange_age_ms":exchange_ages.iter().max(),"missing_exchange_timestamp":books.len()-exchange_ages.len(),"max_rest_latency_ms":books.values().filter_map(|b|b.request_latency_ms).max()}}))?;
            focused_routes.sort_by(|a, b| {
                b.1.net_profit
                    .total_cmp(&a.1.net_profit)
                    .then_with(|| a.0.id.cmp(&b.0.id))
            });
            focused_routes.truncate(c.focus_routes);
            if !focused_routes.is_empty() && candidates.is_empty() && command != "scan" {
                tokio::time::sleep(Duration::from_millis(c.focus_delay_ms)).await;
                let selected = focused_routes
                    .iter()
                    .map(|(route, _)| route.clone())
                    .collect::<Vec<_>>();
                let focused_books = client.fetch_conversion_books(&selected).await;
                let focused_audit = client.conversion_book_audit();
                let mut observations = Vec::new();
                for (route, initial) in &focused_routes {
                    let repeated =
                        evaluate_diagnostic(route, &focused_books, initial.shares, &c, None);
                    observations.push(match repeated {
                        Ok(quote) => json!({
                            "route_id": route.id,
                            "event_slug": route.slug,
                            "initial_quote": initial,
                            "repeated_quote": quote,
                            "became_profitable": quote.net_profit >= c.minimum_profit,
                            "cost_sensitivity": cost_sensitivity(route, &focused_books, quote.shares, &c, true),
                            "evidence": evidence(route, &focused_books, &focused_audit, &c),
                        }),
                        Err(error) => json!({
                            "route_id": route.id,
                            "event_slug": route.slug,
                            "initial_quote": initial,
                            "repeat_error": error.to_string(),
                            "evidence": evidence(route, &focused_books, &focused_audit, &c),
                        }),
                    });
                }
                journal.event(json!({
                    "event":"focused_observation",
                    "selection":"diagnostic_net_profit_floor",
                    "net_profit_floor_usdc":c.focus_net_profit_floor,
                    "requested_delay_ms":c.focus_delay_ms,
                    "routes":observations,
                }))?;
            }
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
    let path = c.root.join("events.ndjson");
    let mut all_events = Vec::<Value>::new();
    let mut malformed = 0usize;
    let mut directory_event_counts = BTreeMap::<String, usize>::new();
    let mut run_event_counts = BTreeMap::<String, BTreeMap<String, usize>>::new();
    let mut run_first = BTreeMap::<String, chrono::DateTime<Utc>>::new();
    let mut run_last = BTreeMap::<String, chrono::DateTime<Utc>>::new();
    let mut selected: Option<(chrono::DateTime<Utc>, String)> = None;
    for line in BufReader::new(File::open(&path)?).lines() {
        let line = line?;
        let event: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        let kind = event["event"].as_str().unwrap_or("unknown");
        let run_id = event["run_id"].as_str().unwrap_or("legacy").to_string();
        *directory_event_counts.entry(kind.into()).or_default() += 1;
        *run_event_counts
            .entry(run_id.clone())
            .or_default()
            .entry(kind.into())
            .or_default() += 1;
        if let Some(time) = event["recorded_at"]
            .as_str()
            .and_then(|value| value.parse::<chrono::DateTime<Utc>>().ok())
        {
            run_first
                .entry(run_id.clone())
                .and_modify(|current| *current = (*current).min(time))
                .or_insert(time);
            run_last
                .entry(run_id.clone())
                .and_modify(|current| *current = (*current).max(time))
                .or_insert(time);
            if kind == "run_started" && selected.as_ref().is_none_or(|(current, _)| time > *current)
            {
                selected = Some((time, run_id.clone()));
            }
        }
        all_events.push(event);
    }
    let selected_run_id = selected
        .map(|(_, run_id)| run_id)
        .or_else(|| {
            all_events
                .last()
                .and_then(|event| event["run_id"].as_str())
                .map(str::to_string)
        })
        .context("event journal has no run_id")?;
    let run_summaries = run_event_counts
        .iter()
        .map(|(run_id, counts)| {
            (
                run_id.clone(),
                json!({"first_recorded_at":run_first.get(run_id),"last_recorded_at":run_last.get(run_id),
                    "event_counts":counts,"selected":run_id==&selected_run_id}),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut episodes = HashMap::<String, (String, Vec<f64>, usize)>::new();
    let mut diagnostic_episodes = HashMap::<String, (String, Vec<f64>, usize)>::new();
    let mut unique_events = BTreeMap::<String, String>::new();
    let mut unique_routes = std::collections::BTreeSet::new();
    let mut discovery_totals = BTreeMap::<String, u64>::new();
    let mut discovery_peaks = BTreeMap::<String, u64>::new();
    let mut page_discovery_scans = BTreeMap::<usize, u64>::new();
    let mut page_max_routes = BTreeMap::<usize, u64>::new();
    let mut page_truncated_scans = BTreeMap::<usize, u64>::new();
    let mut page_unique_routes = BTreeMap::<usize, HashSet<String>>::new();
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
    let mut first: Option<chrono::DateTime<Utc>> = None;
    let mut last: Option<chrono::DateTime<Utc>> = None;
    let mut scan_rejections = BTreeMap::<String, u64>::new();
    let mut best_non_candidate: Option<Value> = None;
    let mut latest_route_research_ranking = Value::Null;
    let mut focused_route_observations = 0u64;
    let mut focused_became_profitable = 0u64;
    let mut focused_best_initial = f64::NEG_INFINITY;
    let mut focused_best_repeated = f64::NEG_INFINITY;
    for e in all_events.iter().filter(|event| {
        event["run_id"]
            .as_str()
            .is_some_and(|run_id| run_id == selected_run_id)
    }) {
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
            let page_offset = e["diagnostics"]["page_offset"].as_u64().unwrap_or(0) as usize;
            *page_discovery_scans.entry(page_offset).or_default() += 1;
            let routes_before = e["diagnostics"]["routes_before_limit"]
                .as_u64()
                .unwrap_or(0);
            page_max_routes
                .entry(page_offset)
                .and_modify(|current| *current = (*current).max(routes_before))
                .or_insert(routes_before);
            *page_truncated_scans.entry(page_offset).or_default() +=
                u64::from(e["diagnostics"]["routes_truncated"].as_u64().unwrap_or(0) > 0);
            if let Some(routes) = e["routes"].as_array() {
                for r in routes {
                    if let (Some(id), Some(slug), Some(route_id)) =
                        (r["event_id"].as_str(), r["slug"].as_str(), r["id"].as_str())
                    {
                        unique_events.insert(id.into(), slug.into());
                        unique_routes.insert(route_id.to_string());
                        page_unique_routes
                            .entry(page_offset)
                            .or_default()
                            .insert(route_id.to_string());
                    }
                }
            }
            if let Some(values) = e["diagnostics"].as_object() {
                for key in [
                    "routes_before_limit",
                    "routes_truncated",
                    "routes_unique_seen_in_run",
                ] {
                    if let Some(value) = values.get(key).and_then(Value::as_u64) {
                        discovery_peaks
                            .entry(key.into())
                            .and_modify(|current| *current = (*current).max(value))
                            .or_insert(value);
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
            if e["route_research_ranking"].is_array() {
                latest_route_research_ranking = e["route_research_ranking"].clone();
            }
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
        if kind == "focused_observation" {
            for observation in e["routes"].as_array().into_iter().flatten() {
                focused_route_observations += 1;
                focused_became_profitable +=
                    u64::from(observation["became_profitable"].as_bool().unwrap_or(false));
                if let Some(value) = observation["initial_quote"]["net_profit"].as_f64() {
                    focused_best_initial = focused_best_initial.max(value);
                }
                if let Some(value) = observation["repeated_quote"]["net_profit"].as_f64() {
                    focused_best_repeated = focused_best_repeated.max(value);
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
    let page_coverage = page_discovery_scans
        .iter()
        .map(|(offset, scans)| {
            (
                offset.to_string(),
                json!({"discovery_scans":scans,"maximum_routes_before_limit":page_max_routes.get(offset),
                    "unique_routes_emitted":page_unique_routes.get(offset).map(HashSet::len).unwrap_or(0),
                    "scans_with_truncation":page_truncated_scans.get(offset).copied().unwrap_or(0)}),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let report = json!({"generated_at":Utc::now(),"mode":"economics_only","protocol_verified":false,"live_ready":false,
        "selected_run_id":selected_run_id,"run_summaries":run_summaries,
        "directory_event_counts":directory_event_counts,"directory_malformed_lines":malformed,
        "coverage":{"unique_events":unique_events.len(),"unique_routes":unique_routes.len(),"events":unique_events,
            "scans_with_strict_priced_route":priced_scans,"estimated_size_evaluations":estimated_evaluations,
            "strict_priced_evaluations":scan_rejections.get("below_net_profit").copied().unwrap_or(0)+strict_profitable_evaluations},
        "discovery_totals_repeated_observations":discovery_totals,"discovery_peaks":discovery_peaks,
        "discovery_page_coverage":page_coverage,"latest_route_research_ranking":latest_route_research_ranking,
        "focused_observations":{"events":counts.get("focused_observation").copied().unwrap_or(0),
            "route_observations":focused_route_observations,"became_profitable":focused_became_profitable,
            "best_initial_net_profit":focused_best_initial.is_finite().then_some(focused_best_initial),
            "best_repeated_net_profit":focused_best_repeated.is_finite().then_some(focused_best_repeated),
            "counts_toward_promotion":false},
        "by_event_size_checks":by_event,"overlapping_leg_time_failures":leg_failures,
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

fn discovery_retry_secs(failures: u32) -> u64 {
    30u64
        .saturating_mul(1u64 << failures.saturating_sub(1).min(4))
        .min(300)
}

#[cfg(test)]
mod tests {
    #[test]
    fn discovery_retry_backs_off_and_caps() {
        assert_eq!(
            [1, 2, 3, 4, 5, 100].map(super::discovery_retry_secs),
            [30, 60, 120, 240, 300, 300]
        );
    }
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
            focus_enabled: true,
            focus_net_profit_floor: -0.10,
            focus_delay_ms: 2000,
            focus_routes: 5,
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
            event_title: "event".into(),
            market_count: 3,
            fee_enabled_legs: 0,
            neg_risk_other_markets: 0,
            earliest_deadline_at: None,
            latest_deadline_at: None,
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
        let mut with_chain = c.clone();
        with_chain.chain_cost = 0.02;
        let sensitivity = cost_sensitivity(&route, &books, 10., &with_chain, false);
        assert!(
            (sensitivity["scenario_net_pnl_usdc"]["no_fees_slippage_conversion_or_chain"]
                .as_f64()
                .unwrap()
                - 0.3)
                .abs()
                < 1e-8
        );
        assert!(
            (sensitivity["scenario_net_pnl_usdc"]["configured"]
                .as_f64()
                .unwrap()
                - 0.28)
                .abs()
                < 1e-8
        );
        assert!(
            (sensitivity["incremental_cost_usdc"]["chain"]
                .as_f64()
                .unwrap()
                - 0.02)
                .abs()
                < 1e-8
        );
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

    #[test]
    fn report_selects_latest_run_and_preserves_directory_counts() {
        let mut c = Config::load().unwrap();
        c.root = std::env::temp_dir().join(format!("conversion-runs-{}", uuid::Uuid::new_v4()));
        let mut first = Journal::open(&c).unwrap();
        first.event(json!({"event":"run_started"})).unwrap();
        first
            .event(json!({"event":"error","phase":"old_run"}))
            .unwrap();
        let old_id = first.run_id.clone();
        drop(first);
        let mut latest = Journal::open(&c).unwrap();
        latest.event(json!({"event":"run_started"})).unwrap();
        latest
            .event(json!({"event":"scan_summary","routes":0,"candidates":0}))
            .unwrap();
        let latest_id = latest.run_id.clone();
        drop(latest);
        report(&c).unwrap();
        let result: Value =
            serde_json::from_reader(File::open(c.root.join("report.json")).unwrap()).unwrap();
        assert_ne!(old_id, latest_id);
        assert_eq!(result["selected_run_id"], latest_id);
        assert_eq!(result["event_counts"]["error"], Value::Null);
        assert_eq!(result["directory_event_counts"]["error"], 1);
        assert_eq!(result["run_summaries"][old_id]["selected"], false);
        assert_eq!(result["run_summaries"][latest_id]["selected"], true);
        for name in ["events.ndjson", "runtime.log", "report.json"] {
            std::fs::remove_file(c.root.join(name)).unwrap();
        }
        std::fs::remove_dir(c.root).unwrap();
    }
}
