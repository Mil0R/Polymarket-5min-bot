//! Read-only cross-venue research. Provider labels never authorize orders.
mod model;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use model::{levels, number, quote, stress, Pricing, Quote};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MCP: &str = "https://predictionmarketspicks.com/api/mcp/mcp";
const KALSHI: &str = "https://external-api.kalshi.com/trade-api/v2";
const GAMMA: &str = "https://gamma-api.polymarket.com";
const CLOB: &str = "https://clob.polymarket.com";

#[derive(Serialize)]
struct Config {
    root: PathBuf,
    registry: PathBuf,
    poll_secs: u64,
    duration_secs: u64,
    max_signal_age_secs: i64,
    max_rest_ms: u64,
    max_book_age_ms: i64,
    include_watch: bool,
    max_pairs: usize,
    min_net_per_pair: f64,
    per_venue_budget: f64,
    haircut: f64,
    slippage_bps: f64,
    sizes: Vec<f64>,
    delays_ms: Vec<u64>,
    subscription_monthly: f64,
}
fn env<T: std::str::FromStr>(key: &str, default: &str) -> Result<T> {
    std::env::var(format!("CROSS_VENUE_ARBITRAGE_{key}"))
        .unwrap_or_else(|_| default.into())
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid {key}"))
}
impl Config {
    fn load() -> Result<Self> {
        let c = Self {
            root: PathBuf::from(env::<String>(
                "RUN_DIR",
                "results/cross-venue-arbitrage/shadow-run-001",
            )?),
            registry: PathBuf::from(env::<String>(
                "REGISTRY_FILE",
                "results/cross-venue-arbitrage/pairs.json",
            )?),
            poll_secs: env("POLL_SECS", "120")?,
            duration_secs: env("DURATION_SECS", "0")?,
            max_signal_age_secs: env("MAX_SIGNAL_AGE_SECS", "900")?,
            max_rest_ms: env("MAX_REST_MS", "1000")?,
            max_book_age_ms: env("MAX_BOOK_AGE_MS", "5000")?,
            include_watch: env("INCLUDE_WATCH", "true")?,
            max_pairs: env("MAX_PAIRS", "5")?,
            min_net_per_pair: env("MIN_NET_PER_PAIR_USD", "0.02")?,
            per_venue_budget: env("PER_VENUE_BUDGET_USD", "500")?,
            haircut: env("DEPTH_HAIRCUT", "0.5")?,
            slippage_bps: env("SLIPPAGE_BPS", "20")?,
            sizes: vec![5., 10., 25.],
            delays_ms: vec![500, 1000, 2000, 5000],
            subscription_monthly: env("SUBSCRIPTION_MONTHLY_USD", "14.99")?,
        };
        if c.poll_secs < 60
            || !(1..=50).contains(&c.max_pairs)
            || c.max_signal_age_secs <= 0
            || c.max_rest_ms == 0
            || c.max_book_age_ms <= 0
            || ![
                c.min_net_per_pair,
                c.per_venue_budget,
                c.haircut,
                c.slippage_bps,
                c.subscription_monthly,
            ]
            .iter()
            .all(|v| v.is_finite())
            || !(0.0..=1.0).contains(&c.haircut)
            || c.haircut == 0.
            || c.per_venue_budget <= 0.
            || c.min_net_per_pair < 0.
            || !(0.0..=1000.).contains(&c.slippage_bps)
            || c.subscription_monthly < 0.
        {
            bail!("invalid cross-venue configuration");
        }
        Ok(c)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    id: String,
    sport: String,
    game: String,
    kalshi_ticker: String,
    poly_slug: String,
    poly_yes_token: String,
    poly_no_token: String,
    #[serde(default)]
    reviewed_digest: Option<String>,
    #[serde(default)]
    review_notes: String,
    #[serde(default)]
    valid_until: Option<DateTime<Utc>>,
    #[serde(default)]
    poly_fee_cap_per_net_share: Option<f64>,
    #[serde(default)]
    kalshi_fee_cap_per_contract: Option<f64>,
    #[serde(default)]
    fee_evidence: String,
}
fn registry(path: &Path) -> Result<Vec<Pair>> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // An empty registry is a valid discovery-only state. Manual pair approval
        // remains required before any book pricing is attempted.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create empty registry {}", path.display()))?;
        file.write_all(b"[]\n")?;
        return Ok(Vec::new());
    }
    let pairs: Vec<Pair> = serde_json::from_reader(
        File::open(path).with_context(|| format!("open {}", path.display()))?,
    )?;
    let mut ids = HashSet::new();
    let mut contracts = HashSet::new();
    let mut names = HashSet::new();
    for p in &pairs {
        if p.id.is_empty()
            || p.sport.is_empty()
            || p.game.is_empty()
            || !ids.insert(p.id.clone())
            || !contracts.insert((p.kalshi_ticker.clone(), p.poly_slug.clone()))
            || !names.insert((p.sport.clone(), p.game.clone()))
            || !safe_id(&p.kalshi_ticker)
            || !safe_id(&p.poly_slug)
            || !digits(&p.poly_yes_token)
            || !digits(&p.poly_no_token)
            || p.poly_yes_token == p.poly_no_token
        {
            bail!("invalid or duplicate pair identity: {}", p.id);
        }
        for fee in [p.poly_fee_cap_per_net_share, p.kalshi_fee_cap_per_contract]
            .into_iter()
            .flatten()
        {
            if !fee.is_finite() || !(0.0..=1.0).contains(&fee) {
                bail!("invalid fee cap for {}", p.id);
            }
        }
    }
    Ok(pairs)
}
fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

struct Journal {
    root: PathBuf,
    run_id: String,
    _lock: File,
}
impl Journal {
    fn new(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("writer.lock"))?;
        lock.try_lock_exclusive()
            .context("another cross-venue writer uses this run directory")?;
        let j = Self {
            root: root.into(),
            run_id: uuid::Uuid::new_v4().to_string(),
            _lock: lock,
        };
        // Isolate a torn final record after a crash; preserve it for the report audit.
        let path = j.path();
        if path.exists() {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = OpenOptions::new().read(true).append(true).open(path)?;
            if f.metadata()?.len() > 0 {
                f.seek(SeekFrom::End(-1))?;
                let mut byte = [0];
                f.read_exact(&mut byte)?;
                if byte[0] != b'\n' {
                    f.write_all(b"\n")?;
                }
            }
        }
        Ok(j)
    }
    fn path(&self) -> PathBuf {
        self.root
            .join(format!("events-{}.ndjson", Utc::now().format("%Y-%m-%d")))
    }
    fn event(&self, kind: &str, data: Value) -> Result<()> {
        let record = json!({"schema_version":1,"run_id":self.run_id,"recorded_at":Utc::now(),"event":kind,"data":data});
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())?;
        serde_json::to_writer(&mut f, &record)?;
        f.write_all(b"\n")?;
        f.flush()?;
        Ok(())
    }
}

#[derive(Serialize)]
struct Response {
    body: Value,
    started_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
    latency_ms: u128,
}
// Preserve partial successes and timing even if another leg fails.
async fn book_response(request: reqwest::RequestBuilder) -> Response {
    let started_at = Utc::now();
    let start = Instant::now();
    match receive(request).await {
        Ok(response) => response,
        Err(error) => Response {
            body: json!({"request_error": error.to_string()}),
            started_at,
            received_at: Utc::now(),
            latency_ms: start.elapsed().as_millis(),
        },
    }
}
async fn receive(request: reqwest::RequestBuilder) -> Result<Response> {
    let started_at = Utc::now();
    let started = Instant::now();
    let mut response = request.send().await.map_err(|e| e.without_url())?;
    if !response.status().is_success() {
        bail!("HTTP {}", response.status());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.without_url())? {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            bail!("response exceeds 8 MiB");
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&bytes)?;
    let body = if let Ok(v) = serde_json::from_str(text) {
        v
    } else {
        let mut result = None;
        for block in text.replace("\r\n", "\n").split("\n\n") {
            let data = block
                .lines()
                .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                if v.get("result").is_some() || v.get("error").is_some() {
                    result = Some(v);
                }
            }
        }
        result.context("invalid JSON or MCP SSE response")?
    };
    Ok(Response {
        body,
        started_at,
        received_at: Utc::now(),
        latency_ms: started.elapsed().as_millis(),
    })
}
async fn feed(http: &reqwest::Client, key: Option<&str>, endpoint: &str) -> Result<Response> {
    let mut req=http.post(endpoint).header("Accept","application/json, text/event-stream").json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"find_arbitrage","arguments":{"minGap":0}}}));
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    receive(req).await
}
fn feed_status(body: &Value) -> (&'static str, bool) {
    if body.get("error").is_some() || body["result"]["isError"] == true {
        if body["result"]["structuredContent"]["error"].as_str()
            == Some("No arbitrage data available right now.")
        {
            return ("provider_no_data", false);
        }
        // Retry only explicit transient JSON-RPC errors; unknown tool errors fail closed.
        return if body["error"]["code"] == -32603 {
            ("rpc_transient_error", true)
        } else {
            ("rpc_tool_error", false)
        };
    }
    if !body["result"]["structuredContent"]["gaps"].is_array() {
        return ("schema_error", false);
    }
    ("success", false)
}
fn request_error_status(error: &anyhow::Error) -> (&'static str, bool) {
    let message = error.to_string();
    if message.starts_with("HTTP 401") || message.starts_with("HTTP 403") {
        ("authentication_error", false)
    } else if message.starts_with("HTTP 429") {
        // Leave rate limiting to the next polling interval, not a short retry loop.
        ("rate_limited", false)
    } else if message.starts_with("HTTP 5") {
        ("http_server_error", true)
    } else if message.starts_with("HTTP ") {
        ("http_client_error", false)
    } else if error.downcast_ref::<reqwest::Error>().is_some() {
        ("transport_error", true)
    } else {
        ("response_decode_error", false)
    }
}
async fn feed_with_retry(
    http: &reqwest::Client,
    key: Option<&str>,
    j: &Journal,
) -> Result<Option<Response>> {
    feed_with_retry_at(http, key, j, MCP).await
}
async fn feed_with_retry_at(
    http: &reqwest::Client,
    key: Option<&str>,
    j: &Journal,
    endpoint: &str,
) -> Result<Option<Response>> {
    for attempt in 0..3 {
        match feed(http, key, endpoint).await {
            Ok(response) => {
                j.event("pmp_raw", json!(&response))?;
                let (status, retryable) = feed_status(&response.body);
                j.event("feed_attempt", json!({"attempt":attempt+1,"status":status,"retryable":retryable,"latency_ms":response.latency_ms}))?;
                if status == "success" {
                    return Ok(Some(response));
                }
                if status == "provider_no_data" {
                    j.event("feed_unavailable", json!({"reason":status}))?;
                    tracing::info!("PMP temporarily returned no data; waiting for next cycle");
                    return Ok(None);
                }
                if !retryable || attempt == 2 {
                    bail!("{status}");
                }
            }
            Err(error) => {
                let (status, retryable) = request_error_status(&error);
                j.event("feed_attempt", json!({"attempt":attempt+1,"status":status,"retryable":retryable,"reason":error.to_string()}))?;
                if !retryable || attempt == 2 {
                    return Err(error.context(status));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2u64.saturating_pow(attempt + 1))).await;
    }
    unreachable!("all feed attempts return or fail")
}
fn feed_data(response: &Response) -> Result<&Value> {
    let body = &response.body;
    if body.get("error").is_some() || body["result"]["isError"] == true {
        bail!("PMP RPC/tool error (see raw feed)");
    }
    let data = &body["result"]["structuredContent"];
    if !data["gaps"].is_array() {
        bail!("PMP missing structured gaps (auth or schema issue)");
    }
    Ok(data)
}
fn signal_age(data: &Value, now: DateTime<Utc>) -> Result<i64> {
    let at = data["as_of"]
        .as_str()
        .context("PMP missing as_of")?
        .parse::<DateTime<Utc>>()?;
    let age = (now - at).num_seconds();
    if age < -5 {
        bail!("PMP timestamp in future");
    }
    Ok(age.max(0))
}
fn signal(row: &Value) -> String {
    row["signal"]
        .as_str()
        .unwrap_or("UNKNOWN")
        .to_ascii_uppercase()
        .replace(' ', "_")
}

fn source_is_stale(data: &Value, age: i64, max_age: i64) -> bool {
    age > max_age || data["stale"] == true
}

fn eligible_signal(sig: &str, include_watch: bool) -> bool {
    matches!(sig, "ARB" | "ARB_FLAGGED") || (include_watch && sig == "WATCH")
}

fn list(v: &Value) -> Result<Vec<String>> {
    let decoded = if let Some(s) = v.as_str() {
        serde_json::from_str::<Value>(s)?
    } else {
        v.clone()
    };
    decoded
        .as_array()
        .context("missing array")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .context("invalid array member")
        })
        .collect()
}
fn fields(v: &Value, names: &[&str]) -> Value {
    json!(names
        .iter()
        .map(|k| (*k, v[*k].clone()))
        .collect::<BTreeMap<_, _>>())
}
async fn metadata(http: &reqwest::Client, p: &Pair) -> Result<(Value, String)> {
    let (k, g) = tokio::join!(
        receive(http.get(format!("{KALSHI}/markets/{}", p.kalshi_ticker))),
        receive(
            http.get(format!("{GAMMA}/markets"))
                .query(&[("slug", &p.poly_slug)])
        )
    );
    let k = k?;
    let g = g?;
    let km = &k.body["market"];
    let matches = g
        .body
        .as_array()
        .context("Gamma markets not array")?
        .iter()
        .filter(|m| m["slug"].as_str() == Some(&p.poly_slug))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        bail!("Gamma slug did not resolve uniquely");
    }
    let pm = matches[0];
    if km["ticker"].as_str() != Some(&p.kalshi_ticker) {
        bail!("Kalshi ticker identity mismatch");
    }
    let tokens = list(&pm["clobTokenIds"])?;
    if tokens.len() != 2
        || !tokens.contains(&p.poly_yes_token)
        || !tokens.contains(&p.poly_no_token)
    {
        bail!("Polymarket token identity mismatch");
    }
    if km["rules_primary"].as_str().unwrap_or_default().is_empty()
        || pm["description"].as_str().unwrap_or_default().is_empty()
    {
        bail!("missing resolution rules");
    }
    // Bind manual semantic approval and fee assumptions to exact contracts and rule text.
    let mut review = p.clone();
    review.reviewed_digest = None;
    let evidence = json!({"pair":review,
        "kalshi":fields(km,&["ticker","event_ticker","title","subtitle","yes_sub_title","no_sub_title","rules_primary","rules_secondary","expiration_time","close_time"]),
        "polymarket":fields(pm,&["slug","conditionId","question","description","resolutionSource","endDate","outcomes","clobTokenIds","feesEnabled","feeSchedule","negRisk"])});
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&evidence)?));
    let tradable = km["status"] == "active"
        && pm["active"] == true
        && pm["closed"] == false
        && pm["acceptingOrders"] == true
        && pm["enableOrderBook"] == true;
    Ok((
        json!({"evidence":evidence,"kalshi_market":km,"polymarket_market":pm,"tradable":tradable}),
        digest,
    ))
}
fn approved(p: &Pair, digest: &str, now: DateTime<Utc>) -> Result<()> {
    if p.reviewed_digest.as_deref() != Some(digest) {
        bail!("missing_or_changed_rule_review");
    }
    if p.review_notes.trim().is_empty() || p.fee_evidence.trim().is_empty() {
        bail!("missing_review_evidence");
    }
    if p.valid_until.is_none_or(|v| v <= now) {
        bail!("review_expired");
    }
    if p.poly_fee_cap_per_net_share.is_none() || p.kalshi_fee_cap_per_contract.is_none() {
        bail!("unknown_fee_caps");
    }
    Ok(())
}

#[derive(Serialize)]
struct Books {
    kalshi: Response,
    poly_yes: Response,
    poly_no: Response,
}
async fn books(http: &reqwest::Client, p: &Pair) -> Result<Books> {
    let (k, y, n) = tokio::join!(
        book_response(http.get(format!("{KALSHI}/markets/{}/orderbook", p.kalshi_ticker))),
        book_response(
            http.get(format!("{CLOB}/book"))
                .query(&[("token_id", &p.poly_yes_token)])
        ),
        book_response(
            http.get(format!("{CLOB}/book"))
                .query(&[("token_id", &p.poly_no_token)])
        )
    );
    Ok(Books {
        kalshi: k,
        poly_yes: y,
        poly_no: n,
    })
}
fn validate_books(b: &Books, p: &Pair, c: &Config) -> Result<()> {
    let now = Utc::now();
    for r in [&b.kalshi, &b.poly_yes, &b.poly_no] {
        if r.body.get("request_error").is_some() {
            bail!("book_request_failed");
        }
        if r.latency_ms > c.max_rest_ms as u128
            || (now - r.started_at).num_milliseconds() > c.max_book_age_ms
        {
            bail!("REST_latency_or_snapshot_skew");
        }
    }
    for (r, id) in [
        (&b.poly_yes, &p.poly_yes_token),
        (&b.poly_no, &p.poly_no_token),
    ] {
        if r.body["asset_id"].as_str() != Some(id) {
            bail!("book_token_identity");
        }
        let timestamp = number(&r.body["timestamp"])?;
        let age = now.timestamp_millis() as f64 - timestamp;
        if age < -2000. || age > c.max_book_age_ms as f64 {
            bail!("Polymarket_exchange_timestamp");
        }
    }
    Ok(())
}
fn evaluate(b: &Books, p: &Pair, c: &Config, direction: &str, q: f64) -> Result<Quote> {
    validate_books(b, p, c)?;
    let poly = if direction == "poly_yes_kalshi_no" {
        &b.poly_yes
    } else {
        &b.poly_no
    };
    let side = if direction == "poly_yes_kalshi_no" {
        "yes_dollars"
    } else {
        "no_dollars"
    };
    let pa = levels(&poly.body["asks"], false, false)?;
    let ka = levels(&b.kalshi.body["orderbook_fp"][side], true, true)?;
    quote(
        direction,
        &pa,
        &ka,
        q,
        &Pricing {
            haircut: c.haircut,
            slippage_bps: c.slippage_bps,
            poly_fee_cap: p
                .poly_fee_cap_per_net_share
                .context("unknown poly fee cap")?,
            kalshi_fee_cap: p
                .kalshi_fee_cap_per_contract
                .context("unknown Kalshi fee cap")?,
            per_venue_budget: c.per_venue_budget,
        },
    )
}

async fn cycle(
    http: &reqwest::Client,
    key: Option<&str>,
    c: &Config,
    j: &Journal,
    seen: &mut HashSet<String>,
) -> Result<()> {
    let Some(response) = feed_with_retry(http, key, j).await? else {
        return Ok(());
    };
    let data = feed_data(&response)?;
    let age = signal_age(data, Utc::now())?;
    let pairs = registry(&c.registry)?; // Hot reload reviewed registry, not secrets or strategy parameters.
    let rows = data["gaps"].as_array().unwrap();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut selected = Vec::new();
    let mut selected_ids = HashSet::new();
    let mut funnel = BTreeMap::<String, usize>::new();
    for row in rows {
        let sig = signal(row);
        *counts.entry(sig.clone()).or_default() += 1;
        if !eligible_signal(&sig, c.include_watch) {
            *funnel.entry("label_skipped".into()).or_default() += 1;
            continue;
        }
        let pair = pairs.iter().find(|p| {
            row["game"].as_str() == Some(&p.game) && row["sport"].as_str() == Some(&p.sport)
        });
        let reason = if data["polymarket_venue"] != "international" {
            Some("unknown_or_wrong_venue")
        } else if source_is_stale(data, age, c.max_signal_age_secs) {
            Some("source_stale")
        } else if pair.is_none() {
            Some("unmapped_pair")
        } else {
            None
        };
        *funnel
            .entry(reason.unwrap_or("eligible_mapped").into())
            .or_default() += 1;
        j.event("candidate",json!({"row":row,"source_as_of":data["as_of"],"age_seconds":age,"pair_id":pair.map(|p|&p.id),"rejection":reason}))?;
        if reason.is_none() {
            let p = pair.unwrap();
            if selected_ids.insert(p.id.clone()) {
                selected.push(p);
            }
        }
    }
    // Rotate the bounded set over refreshes instead of starving pairs after the cap.
    selected.sort_by_key(|p| p.id.clone());
    if !selected.is_empty() {
        let offset = (Utc::now().timestamp().max(0) as usize / c.poll_secs as usize * c.max_pairs)
            % selected.len();
        selected.rotate_left(offset);
    }
    funnel.insert(
        "deferred_by_pair_limit".into(),
        selected.len().saturating_sub(c.max_pairs),
    );
    selected.truncate(c.max_pairs);
    funnel.insert("selected_pairs".into(), selected.len());
    j.event("feed_summary",json!({"signals":counts,"funnel":funnel,"registry_pairs":pairs.len(),"returned":rows.len(),"total_count":data["total_count"],"truncated":data["truncated"],"truncated_unknown":data["truncated"].is_null(),"age_seconds":age,"source_age_exceeded":age>c.max_signal_age_secs,"vendor_stale":data["stale"],"source_stale":source_is_stale(data,age,c.max_signal_age_secs),"selected_pairs":selected.len(),"pro_key_present":key.is_some(),"pro_entitlement_verified":false}))?;
    tracing::info!(?counts,age_seconds=age,selected_pairs=selected.len(),truncated=?data["truncated"],"PMP candidate summary");
    for p in selected {
        let result = probe(http, p, c, j, seen).await;
        if let Err(error) = result {
            j.event(
                "pair_error",
                json!({"pair_id":p.id,"reason":error.to_string()}),
            )?;
            tracing::warn!(pair=%p.id,%error,"pair probe rejected");
        }
    }
    Ok(())
}
async fn probe(
    http: &reqwest::Client,
    p: &Pair,
    c: &Config,
    j: &Journal,
    seen: &mut HashSet<String>,
) -> Result<()> {
    let (meta, digest) = metadata(http, p).await?;
    j.event(
        "rule_audit",
        json!({"pair_id":p.id,"digest":digest,"metadata":meta}),
    )?;
    approved(p, &digest, Utc::now())?;
    if meta["tradable"] != true {
        bail!("market_not_tradable");
    }
    let initial = books(http, p).await?;
    let start = Instant::now();
    let episode = uuid::Uuid::new_v4().to_string();
    let mut quotes = Vec::new();
    let mut errors = Vec::new();
    for direction in ["poly_yes_kalshi_no", "poly_no_kalshi_yes"] {
        for &q in &c.sizes {
            match evaluate(&initial, p, c, direction, q) {
                Ok(v) => quotes.push(v),
                Err(e) => {
                    errors.push(json!({"direction":direction,"quantity":q,"reason":e.to_string()}))
                }
            }
        }
    }
    j.event("book_probe",json!({"pair_id":p.id,"episode_id":episode,"books":initial,"quotes":quotes,"rejections":errors,
        "kalshi_exchange_timestamp_verified":false,"economic_assumptions":"manually reviewed fee upper bounds; equal NET payouts; REST receipt-time Kalshi freshness only"}))?;
    // One chosen direction and size per market pair per run. Repeated snapshots are not independent profit.
    let best = quotes
        .into_iter()
        .filter(|q| q.net_per_pair >= c.min_net_per_pair)
        .max_by(|a, b| a.net_profit.total_cmp(&b.net_profit));
    let Some(best) = best else {
        return Ok(());
    };
    let counted = seen.insert(p.id.clone());
    j.event("opportunity",json!({"pair_id":p.id,"episode_id":episode,"quote":best,"first_in_run":counted,"counts_as_fill":false}))?;
    tracing::info!(pair=%p.id,net_profit=best.net_profit,quantity=best.quantity,"conditional cross-venue opportunity");
    for &delay in &c.delays_ms {
        tokio::time::sleep(Duration::from_millis(delay).saturating_sub(start.elapsed())).await;
        let fresh = books(http, p).await;
        let mut item = json!({"pair_id":p.id,"episode_id":episode,"delay_ms":delay,"actual_delay_ms":start.elapsed().as_millis()});
        match fresh {
            Ok(b) => {
                let result = evaluate(&b, p, c, &best.direction, best.quantity);
                item["books"] = json!(b);
                match result {
                    Ok(q) => {
                        item["stress"] = stress(&best, &q);
                        item["quote"] = json!(q);
                    }
                    Err(e) => item["reason"] = json!(e.to_string()),
                }
            }
            Err(e) => item["reason"] = json!(e.to_string()),
        }
        j.event("delayed_probe", item)?;
    }
    Ok(())
}

pub async fn run(command: &str) -> Result<()> {
    let mut c = Config::load()?;
    if command == "report" {
        return report(&c);
    }
    if !matches!(command, "scan" | "shadow" | "inspect" | "probe") {
        bail!("expected scan, shadow, inspect, probe or report");
    }
    // Diagnostic runs must not become the latest run of the shadow report.
    if matches!(command, "inspect" | "probe") {
        c.root = c.root.join(command);
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .user_agent("PolyPulse-cross-venue-research/1")
        .build()?;
    let j = Journal::new(&c.root)?;
    let key = std::env::var("PMP_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty());
    j.event("run_started",json!({"config":c,"mode":command,"pro_key_present":key.is_some(),"package_version":env!("CARGO_PKG_VERSION"),"live_ready":false}))?;
    if command == "inspect" {
        for p in registry(&c.registry)? {
            match metadata(&http, &p).await {
                Ok((meta, digest)) => {
                    j.event(
                        "review_proposal",
                        json!({"pair_id":p.id,"digest":digest,"metadata":meta}),
                    )?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"pair_id":p.id,"proposed_reviewed_digest":digest,"metadata":meta})
                        )?
                    );
                }
                Err(error) => {
                    j.event(
                        "pair_error",
                        json!({"pair_id":p.id,"reason":error.to_string()}),
                    )?;
                    tracing::warn!(pair=%p.id,%error,"inspect failed");
                }
            }
        }
    } else if command == "probe" {
        let pairs = registry(&c.registry)?;
        if pairs.is_empty() {
            j.event(
                "diagnostic_summary",
                json!({"registry_pairs":0,"attempted":0,"reason":"empty_registry"}),
            )?;
            j.event("run_stopped", json!({}))?;
            report(&c)?;
            bail!(
                "probe needs manually reviewed pairs in {}",
                c.registry.display()
            );
        }
        let mut seen = HashSet::new();
        for p in pairs.iter().take(c.max_pairs) {
            if let Err(error) = probe(&http, p, &c, &j, &mut seen).await {
                j.event(
                    "pair_error",
                    json!({"pair_id":p.id,"reason":error.to_string(),"source":"manual_diagnostic"}),
                )?;
                tracing::warn!(pair=%p.id,%error,"diagnostic probe rejected");
            }
        }
        j.event("diagnostic_summary", json!({"registry_pairs":pairs.len(),"attempted":pairs.len().min(c.max_pairs),"deferred":pairs.len().saturating_sub(c.max_pairs),"source":"manual_diagnostic","pmp_required":false}))?;
    } else {
        let mut seen = HashSet::new();
        let started = Instant::now();
        let work = async {
            loop {
                if let Err(error) = cycle(&http, key.as_deref(), &c, &j, &mut seen).await {
                    j.event("cycle_error", json!({"reason":error.to_string()}))?;
                    tracing::warn!(%error,"cross-venue cycle failed; retry after configured interval");
                }
                if command == "scan"
                    || (c.duration_secs > 0 && started.elapsed().as_secs() >= c.duration_secs)
                {
                    break;
                }
                let wait = if c.duration_secs == 0 {
                    Duration::from_secs(c.poll_secs)
                } else {
                    Duration::from_secs(c.poll_secs)
                        .min(Duration::from_secs(c.duration_secs).saturating_sub(started.elapsed()))
                };
                tokio::time::sleep(wait).await;
                if c.duration_secs > 0 && started.elapsed().as_secs() >= c.duration_secs {
                    break;
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! {r=work=>r?,_=tokio::signal::ctrl_c()=>{}}
    }
    j.event("run_stopped", json!({}))?;
    report(&c)
}

fn events(root: &Path, mut visit: impl FnMut(Value)) -> Result<usize> {
    let mut paths = fs::read_dir(root)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with("events-"))
                && p.extension().is_some_and(|e| e == "ndjson")
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut malformed = 0;
    for path in paths {
        for line in BufReader::new(File::open(path)?).lines() {
            match serde_json::from_str::<Value>(&line?) {
                Ok(v) => visit(v),
                Err(_) => malformed += 1,
            }
        }
    }
    Ok(malformed)
}
fn report(c: &Config) -> Result<()> {
    let mut last_run = String::new();
    let mut runs = BTreeMap::<String, usize>::new();
    let malformed = events(&c.root, |e| {
        if let Some(id) = e["run_id"].as_str() {
            *runs.entry(id.into()).or_default() += 1;
            if e["event"] == "run_started" {
                last_run = id.into();
            }
        }
    })?;
    if last_run.is_empty() {
        bail!("no run_started in journal");
    }
    let mut counts = BTreeMap::<String, usize>::new();
    let mut rejections = BTreeMap::<String, usize>::new();
    let mut opportunities = BTreeMap::<String, Value>::new();
    let mut repeated = BTreeMap::<String, Vec<Value>>::new();
    let mut selected_config = Value::Null;
    let mut ages = Vec::new();
    let mut returned = 0;
    let mut truncated = 0;
    let mut truncated_unknown = 0;
    let mut source_stale_cycles = 0;
    let mut age_exceeded_cycles = 0;
    let mut vendor_stale_cycles = 0;
    let mut raw_source_stale = false;
    let mut row_distribution = BTreeMap::<u64, usize>::new();
    let mut total_matches = 0;
    let mut funnel = BTreeMap::<String, u64>::new();
    let mut attempt_statuses = BTreeMap::<String, usize>::new();
    let mut raw_statuses = BTreeMap::<String, usize>::new();
    let mut historical_http_errors = 0;
    let mut selected_mode = Value::Null;
    let mut key_present = false;
    let mut candidate_outcomes = BTreeMap::<String, usize>::new();
    let mut signal_counts = BTreeMap::<String, u64>::new();
    let mut unique_source_snapshots = HashSet::new();
    let mut latency = BTreeMap::<String, Vec<u64>>::new();
    let mut request_failures = BTreeMap::<String, usize>::new();
    let mut best_priced: Option<Value> = None;
    let mut first_time: Option<DateTime<Utc>> = None;
    let mut last_time: Option<DateTime<Utc>> = None;
    let mut first_episodes = HashSet::<String>::new();
    events(&c.root, |e| {
        if e["run_id"].as_str() != Some(&last_run) {
            return;
        }
        let kind = e["event"].as_str().unwrap_or("unknown");
        let d = &e["data"];
        if let Some(t) = e["recorded_at"]
            .as_str()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        {
            first_time = Some(first_time.map_or(t, |v| v.min(t)));
            last_time = Some(last_time.map_or(t, |v| v.max(t)));
        }
        *counts.entry(kind.into()).or_default() += 1;
        if kind == "run_started" {
            selected_config = d["config"].clone();
            selected_mode = d["mode"].clone();
            key_present = d["pro_key_present"] == true;
        }
        if let Some(reason) = d["reason"].as_str().or_else(|| d["rejection"].as_str()) {
            *rejections.entry(reason.into()).or_default() += 1;
        }
        if kind == "feed_summary" {
            if let Some(age) = d["age_seconds"].as_i64() {
                ages.push(age);
            }
            returned += d["returned"].as_u64().unwrap_or(0);
            if let Some(n) = d["returned"].as_u64() {
                *row_distribution.entry(n).or_default() += 1;
                total_matches += usize::from(d["total_count"].as_u64() == Some(n));
            }
            if let Some(stages) = d["funnel"].as_object() {
                for (key, n) in stages {
                    *funnel.entry(key.clone()).or_default() += n.as_u64().unwrap_or(0);
                }
            } else {
                // Historical journals lack funnel fields. Labels are sufficient to
                // reconstruct skipped rows, but not registry size or selected pairs.
                if let Some(signals) = d["signals"].as_object() {
                    for (sig, n) in signals {
                        if !eligible_signal(
                            sig,
                            selected_config["include_watch"].as_bool().unwrap_or(true),
                        ) {
                            *funnel.entry("label_skipped".into()).or_default() +=
                                n.as_u64().unwrap_or(0);
                        }
                    }
                }
            }
            truncated += usize::from(d["truncated"] == true);
            truncated_unknown +=
                usize::from(d["truncated_unknown"] == true || d["truncated"].is_null());
            let configured_age = selected_config["max_signal_age_secs"]
                .as_i64()
                .unwrap_or(900);
            let age_exceeded = d["age_seconds"]
                .as_i64()
                .is_some_and(|age| age > configured_age);
            age_exceeded_cycles += usize::from(age_exceeded);
            let vendor_stale = d["vendor_stale"] == true || raw_source_stale;
            raw_source_stale = false;
            vendor_stale_cycles += usize::from(vendor_stale);
            source_stale_cycles +=
                usize::from(d["source_stale"] == true || age_exceeded || vendor_stale);
            if let Some(signals) = d["signals"].as_object() {
                for (key, v) in signals {
                    *signal_counts.entry(key.clone()).or_default() += v.as_u64().unwrap_or(0);
                }
            }
        }
        if kind == "pmp_raw" {
            *raw_statuses
                .entry(feed_status(&d["body"]).0.into())
                .or_default() += 1;
            raw_source_stale = d["body"]["result"]["structuredContent"]["stale"] == true;
            if let Some(at) = d["body"]["result"]["structuredContent"]["as_of"].as_str() {
                unique_source_snapshots.insert(at.to_string());
            }
        }
        if kind == "feed_attempt" {
            *attempt_statuses
                .entry(d["status"].as_str().unwrap_or("unknown").into())
                .or_default() += 1;
        }
        if kind == "candidate" {
            *candidate_outcomes
                .entry(d["rejection"].as_str().unwrap_or("eligible_mapped").into())
                .or_default() += 1;
        }
        if kind == "cycle_error" && d["reason"].as_str().is_some_and(|s| s.starts_with("HTTP ")) {
            historical_http_errors += 1;
        }
        if matches!(kind, "book_probe" | "delayed_probe") {
            for venue in ["kalshi", "poly_yes", "poly_no"] {
                if let Some(ms) = d["books"][venue]["latency_ms"].as_u64() {
                    latency.entry(venue.into()).or_default().push(ms);
                }
                if d["books"][venue]["body"].get("request_error").is_some() {
                    *request_failures.entry(venue.into()).or_default() += 1;
                }
            }
        }
        if kind == "book_probe" {
            for reject in d["rejections"].as_array().into_iter().flatten() {
                if let Some(reason) = reject["reason"].as_str() {
                    *rejections.entry(reason.into()).or_default() += 1;
                }
            }
            for q in d["quotes"].as_array().into_iter().flatten() {
                if let Some(pnl) = q["net_profit"].as_f64() {
                    if best_priced.as_ref().is_none_or(|b| {
                        pnl > b["quote"]["net_profit"]
                            .as_f64()
                            .unwrap_or(f64::NEG_INFINITY)
                    }) {
                        best_priced = Some(
                            json!({"pair_id":d["pair_id"],"recorded_at":e["recorded_at"],"quote":q}),
                        );
                    }
                }
            }
        }
        if kind == "opportunity" && d["first_in_run"] == true {
            if let Some(pair) = d["pair_id"].as_str() {
                opportunities.insert(pair.into(), d.clone());
                if let Some(id) = d["episode_id"].as_str() {
                    first_episodes.insert(id.into());
                }
            }
        }
        if kind == "delayed_probe" {
            if let Some(id) = d["episode_id"].as_str() {
                if first_episodes.contains(id) {
                    repeated.entry(id.into()).or_default().push(fields(
                        d,
                        &["delay_ms", "actual_delay_ms", "quote", "stress", "reason"],
                    ));
                }
            }
        }
    })?;
    ages.sort();
    let configured_delays = selected_config["delays_ms"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_u64).collect::<HashSet<_>>())
        .unwrap_or_default();
    let threshold = selected_config["min_net_per_pair"]
        .as_f64()
        .context("report missing recorded threshold")?;
    let mut robust = 0;
    let mut worst_sum = 0.;
    let mut details = Vec::new();
    for (id, initial) in &opportunities {
        let obs = repeated
            .get(initial["episode_id"].as_str().unwrap_or_default())
            .cloned()
            .unwrap_or_default();
        let found = obs
            .iter()
            .filter_map(|d| d["delay_ms"].as_u64())
            .collect::<HashSet<_>>();
        let ok = malformed == 0
            && !configured_delays.is_empty()
            && found == configured_delays
            && obs.len() == configured_delays.len()
            && obs.iter().all(|d| {
                d["quote"]["net_per_pair"]
                    .as_f64()
                    .is_some_and(|v| v >= threshold)
                    && d["stress"]["poly_first_locked_cost_pnl"]
                        .as_f64()
                        .is_some_and(|v| v >= 0.)
                    && d["stress"]["kalshi_first_locked_cost_pnl"]
                        .as_f64()
                        .is_some_and(|v| v >= 0.)
            });
        if ok {
            robust += 1;
            worst_sum += obs
                .iter()
                .flat_map(|d| {
                    [
                        d["quote"]["net_profit"].as_f64(),
                        d["stress"]["poly_first_locked_cost_pnl"].as_f64(),
                        d["stress"]["kalshi_first_locked_cost_pnl"].as_f64(),
                    ]
                })
                .flatten()
                .fold(f64::INFINITY, f64::min);
        }
        details.push(json!({"pair_id":id,"initial":initial,"complete":found==configured_delays,"robust_conditional":ok,"delayed":obs.iter().map(|v|fields(v,&["delay_ms","actual_delay_ms","quote","stress","reason"])).collect::<Vec<_>>()}));
    }
    let elapsed_secs = first_time
        .zip(last_time)
        .map(|(a, b)| (b - a).num_seconds().max(0))
        .unwrap_or(0);
    let monthly = selected_config["subscription_monthly"]
        .as_f64()
        .unwrap_or(0.);
    let overhead = monthly * elapsed_secs as f64 / (30. * 86400.);
    let latency_summary=latency.into_iter().map(|(venue,mut values)|{
        values.sort_unstable();let n=values.len();
        let failures=request_failures.get(&venue).copied().unwrap_or(0);
        (venue,json!({"samples":n,"p50_ms":values[n/2],"p95_ms":values[(n-1)*95/100],"max_ms":values[n-1],"failed_requests":failures}))
    }).collect::<BTreeMap<_,_>>();
    let result = json!({"generated_at":Utc::now(),"selected_run_id":last_run,"selected_mode":selected_mode,"runs_event_counts":runs,"directory_malformed_lines":malformed,
        "first_recorded_at":first_time,"last_recorded_at":last_time,"elapsed_secs":elapsed_secs,
        "event_counts":counts,"rejections":rejections,
        "feed_health":{"attempt_status_counts":attempt_statuses,"raw_response_status_counts":raw_statuses,"legacy_http_cycle_errors":historical_http_errors,"note":"Raw responses and attempts overlap; do not add them. Historical provider_no_data was recorded as cycle_error; it is not a verified network failure."},
        "selection":{"funnel_counts":funnel,"candidate_outcomes":candidate_outcomes,"note":"Age-exceeded scans are not rejected opportunities. Historical registry size cannot be inferred from zero selected pairs."},
        "source":{"returned_rows_repeated":returned,"returned_rows_distribution":row_distribution,"returned_equals_total_cycles":total_matches,"api_key_present":key_present,"pro_entitlement_verified":false,"coverage_status":"unverified","truncated_scans":truncated,"truncated_unknown_cycles":truncated_unknown,"source_stale_cycles":source_stale_cycles,"source_age_exceeded_cycles":age_exceeded_cycles,"vendor_stale_cycles":vendor_stale_cycles,"max_age_secs":ages.last(),"median_age_secs":ages.get(ages.len()/2),"signal_counts_repeated":signal_counts,"unique_source_snapshots":unique_source_snapshots.len()},
        "rest_latency":latency_summary,"best_priced_quote":best_priced,
        "subscription_overhead":{"monthly_usd":monthly,"prorated_30day_usd":overhead,"note":"Separate operating cost; conditional quote sums are not portfolio income"},
        "unique_pair_opportunities":opportunities.len(),"robust_conditional_pairs":robust,"conditional_worst_delay_profit_sum":worst_sum,
        "actual_fills":0,"realized_pnl":0,"live_ready":false,"config":selected_config,"pairs":details,
        "limitations":["No orders, actual fills or settlements. Profits are conditional quote scenarios, not earnings.","Pair independence is not guaranteed across related games/events or restarts; latest run only.","Kalshi freshness is bounded by REST request/receipt timing; no verified exchange book timestamp.","Manual rules and fee cap review required. Fees paid in shares must be covered for equal NET payout.","Free/truncated feed cannot establish full Pro coverage. Missing mappings are not evidence of absent arbitrage.","No automatic promotion. Quote opportunity sums omit failed-leg realized costs, capital recycling and subscription overhead."]});
    let target = c.root.join("report.json");
    let temporary = c.root.join(format!("report-{}.tmp", uuid::Uuid::new_v4()));
    serde_json::to_writer_pretty(File::create(&temporary)?, &result)?;
    fs::rename(temporary, &target)?;
    println!(
        "Report: {} | run={} | unique_pairs={} robust_conditional={} actual_fills=0",
        target.display(),
        last_run,
        opportunities.len(),
        robust
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn feed_errors_distinguish_no_data_auth_transient_and_schema() {
        let no_data = json!({"result":{"isError":true,"structuredContent":{"error":"No arbitrage data available right now."}}});
        assert_eq!(feed_status(&no_data), ("provider_no_data", false));
        assert_eq!(
            feed_status(&json!({"error":{"code":-32603}})),
            ("rpc_transient_error", true)
        );
        assert_eq!(
            feed_status(&json!({"result":{"isError":true}})),
            ("rpc_tool_error", false)
        );
        assert_eq!(
            feed_status(&json!({"result":{"structuredContent":{}}})),
            ("schema_error", false)
        );
        assert_eq!(
            request_error_status(&anyhow::anyhow!("HTTP 401 Unauthorized")),
            ("authentication_error", false)
        );
        assert_eq!(
            request_error_status(&anyhow::anyhow!("HTTP 429 Too Many Requests")),
            ("rate_limited", false)
        );
        assert_eq!(
            request_error_status(&anyhow::anyhow!("HTTP 504 Gateway Timeout")),
            ("http_server_error", true)
        );
        assert!(source_is_stale(&json!({"stale":true}), 1, 900));
        assert!(source_is_stale(&json!({"stale":false}), 901, 900));
        assert!(!eligible_signal("ALIGNED", true));
    }

    #[tokio::test]
    async fn feed_retries_server_error_then_records_no_data_without_retry() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (status, body) in [
                ("503 Service Unavailable", "{}"),
                (
                    "200 OK",
                    r#"{"result":{"isError":true,"structuredContent":{"error":"No arbitrage data available right now."}}}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut req = [0; 4096];
                socket.read(&mut req).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let root = std::env::temp_dir().join(format!("cross-feed-{}", uuid::Uuid::new_v4()));
        let j = Journal::new(&root).unwrap();
        let result = feed_with_retry_at(&reqwest::Client::new(), None, &j, &endpoint)
            .await
            .unwrap();
        assert!(result.is_none());
        server.await.unwrap();
        let mut recorded = Vec::new();
        events(&root, |v| recorded.push(v)).unwrap();
        let attempts: Vec<_> = recorded
            .iter()
            .filter(|v| v["event"] == "feed_attempt")
            .collect();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0]["data"]["status"], "http_server_error");
        assert_eq!(attempts[1]["data"]["status"], "provider_no_data");
        assert_eq!(
            recorded.iter().filter(|v| v["event"] == "pmp_raw").count(),
            1
        );
        drop(j);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn old_report_infers_labels_and_vendor_stale_without_claiming_entitlement() {
        let mut c = Config::load().unwrap();
        c.root = std::env::temp_dir().join(format!("cross-audit-{}", uuid::Uuid::new_v4()));
        c.max_signal_age_secs = 120;
        let j = Journal::new(&c.root).unwrap();
        j.event(
            "run_started",
            json!({"config":c,"mode":"shadow","pro_key_present":true}),
        )
        .unwrap();
        j.event(
            "pmp_raw",
            json!({"body":{"result":{"structuredContent":{"gaps":[],"stale":true}}}}),
        )
        .unwrap();
        j.event(
            "feed_summary",
            json!({"age_seconds":10,"returned":7,"total_count":7,"signals":{"ALIGNED":7}}),
        )
        .unwrap();
        j.event("pmp_raw", json!({"body":{"result":{"isError":true,"structuredContent":{"error":"No arbitrage data available right now."}}}})).unwrap();
        j.event(
            "cycle_error",
            json!({"reason":"PMP RPC/tool error (see raw feed)"}),
        )
        .unwrap();
        // A newer diagnostic run in a child directory must not replace shadow.
        let diagnostic = Journal::new(&c.root.join("probe")).unwrap();
        diagnostic
            .event("run_started", json!({"config":c,"mode":"probe"}))
            .unwrap();
        report(&c).unwrap();
        let r: Value =
            serde_json::from_reader(File::open(c.root.join("report.json")).unwrap()).unwrap();
        assert_eq!(r["selected_run_id"], j.run_id);
        assert_eq!(r["source"]["source_stale_cycles"], 1);
        assert_eq!(r["source"]["source_age_exceeded_cycles"], 0);
        assert_eq!(r["source"]["vendor_stale_cycles"], 1);
        assert_eq!(r["source"]["returned_rows_distribution"]["7"], 1);
        assert_eq!(r["source"]["returned_equals_total_cycles"], 1);
        assert_eq!(r["source"]["pro_entitlement_verified"], false);
        assert_eq!(r["selection"]["funnel_counts"]["label_skipped"], 7);
        assert_eq!(
            r["feed_health"]["raw_response_status_counts"]["provider_no_data"],
            1
        );
        drop(diagnostic);
        drop(j);
        fs::remove_dir_all(c.root).unwrap();
    }
    fn pair() -> Pair {
        Pair {
            id: "test".into(),
            sport: "NFL".into(),
            game: "team-a vs team-b".into(),
            kalshi_ticker: "TEST-YES".into(),
            poly_slug: "test-event".into(),
            poly_yes_token: "1".into(),
            poly_no_token: "2".into(),
            reviewed_digest: Some("digest".into()),
            review_notes: "same resolution, cancellation and date".into(),
            valid_until: Some(Utc::now() + chrono::Duration::hours(1)),
            poly_fee_cap_per_net_share: Some(0.02),
            kalshi_fee_cap_per_contract: Some(0.02),
            fee_evidence: "checked caps".into(),
        }
    }
    #[test]
    fn review_rejects_drift_expiry_and_unknown_fees() {
        let mut p = pair();
        assert!(approved(&p, "digest", Utc::now()).is_ok());
        assert!(approved(&p, "new-digest", Utc::now()).is_err());
        p.poly_fee_cap_per_net_share = None;
        assert!(approved(&p, "digest", Utc::now()).is_err());
        p.poly_fee_cap_per_net_share = Some(0.02);
        p.valid_until = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(approved(&p, "digest", Utc::now()).is_err());
    }
    #[test]
    fn missing_registry_becomes_empty_discovery_state() {
        let path = std::env::temp_dir().join(format!(
            "cross-venue-registry-{}.json",
            uuid::Uuid::new_v4()
        ));
        let pairs = registry(&path).unwrap();
        assert!(pairs.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "[]");
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn reports_isolate_runs_deduplicate_and_use_recorded_config() {
        let mut c = Config::load().unwrap();
        c.root = std::env::temp_dir().join(format!("cross-venue-test-{}", uuid::Uuid::new_v4()));
        c.min_net_per_pair = 0.02;
        c.delays_ms = vec![500, 1000];
        {
            let old = Journal::new(&c.root).unwrap();
            old.event("run_started", json!({"config":c})).unwrap();
            old.event("cycle_error", json!({"reason":"historical"}))
                .unwrap();
        }
        let j = Journal::new(&c.root).unwrap();
        j.event("run_started", json!({"config":c})).unwrap();
        assert!(Journal::new(&c.root).is_err());
        for (ep, first) in [("one", true), ("duplicate", false)] {
            j.event("opportunity",json!({"pair_id":"test","episode_id":ep,"first_in_run":first,"quote":{"net_profit":0.4}})).unwrap();
            for delay in &c.delays_ms {
                j.event("delayed_probe",json!({"episode_id":ep,"delay_ms":delay,"quote":{"net_per_pair":0.03,"net_profit":0.3},"stress":{"poly_first_locked_cost_pnl":0.2,"kalshi_first_locked_cost_pnl":0.1}})).unwrap();
            }
        }
        c.min_net_per_pair = 100.; // Reporting must not reinterpret old samples using this.
        report(&c).unwrap();
        let r: Value =
            serde_json::from_reader(File::open(c.root.join("report.json")).unwrap()).unwrap();
        assert_eq!(r["selected_run_id"], j.run_id);
        assert_eq!(r["unique_pair_opportunities"], 1);
        assert_eq!(r["robust_conditional_pairs"], 1);
        assert_eq!(r["conditional_worst_delay_profit_sum"], 0.1);
        assert!(r["rejections"].get("historical").is_none());
        assert_eq!(r["actual_fills"], 0);
        drop(j);
        fs::remove_dir_all(&c.root).unwrap();
    }
    #[test]
    fn incomplete_delay_never_counts_as_robust() {
        let mut c = Config::load().unwrap();
        c.root = std::env::temp_dir().join(format!("cross-venue-test-{}", uuid::Uuid::new_v4()));
        let j = Journal::new(&c.root).unwrap();
        j.event("run_started", json!({"config":c})).unwrap();
        j.event("opportunity",json!({"pair_id":"test","episode_id":"one","first_in_run":true,"quote":{"net_profit":0.4}})).unwrap();
        j.event(
            "delayed_probe",
            json!({"episode_id":"one","delay_ms":500,"reason":"book_request_failed"}),
        )
        .unwrap();
        report(&c).unwrap();
        let r: Value =
            serde_json::from_reader(File::open(c.root.join("report.json")).unwrap()).unwrap();
        assert_eq!(r["robust_conditional_pairs"], 0);
        assert_eq!(r["pairs"][0]["complete"], false);
        drop(j);
        fs::remove_dir_all(&c.root).unwrap();
    }
    #[tokio::test]
    async fn mcp_sse_and_failed_book_timing_are_preserved() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut req = [0; 4096];
            socket.read(&mut req).await.unwrap();
            let body="event: message\r\ndata: {\"result\":{\"structuredContent\":{\"gaps\":[]}},\"jsonrpc\":\"2.0\",\"id\":1}\r\n\r\n";
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let r = receive(http.get(format!("http://{addr}"))).await.unwrap();
        assert!(feed_data(&r).unwrap()["gaps"].is_array());
        server.await.unwrap();
        let fail = book_response(http.get(format!("http://{addr}"))).await;
        assert!(fail.body.get("request_error").is_some());
        assert!(fail.received_at >= fail.started_at);
    }
    #[test]
    fn source_age_ignores_vendor_stale_boolean() {
        let now = Utc::now();
        let data = json!({"as_of":now-chrono::Duration::seconds(407),"stale":false});
        assert_eq!(signal_age(&data, now).unwrap(), 407);
        assert!(signal_age(&json!({"stale":false}), now).is_err());
    }
    #[test]
    fn public_preview_not_confused_with_full_board() {
        let response = Response {
            body: json!({"result":{"structuredContent":{"gaps":[],"truncated":true}}}),
            started_at: Utc::now(),
            received_at: Utc::now(),
            latency_ms: 1,
        };
        assert_eq!(feed_data(&response).unwrap()["truncated"], true);
    }
}
