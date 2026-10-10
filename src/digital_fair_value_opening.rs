//! SDDFV-only causal opening evidence. Never retains terminal price/labels.
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

pub const CAPTURE_VERSION: u64 = 3;
const MAX_OPENING_AGE_MS: i64 = 10_000; // Existing replay gate, not relaxed for cache hits.
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
}

fn failure(
    slug: &str,
    condition: &str,
    end_ms: i64,
    now: i64,
    source: &str,
    reason: &str,
) -> Value {
    json!({"schema_version":3,"verified":false,"source":source,"slug":slug,
        "condition_id":condition,"window_end_ms":end_ms,"observed_at_ms":now,
        "official_price_to_beat":null,"reason":reason})
}

pub fn event_evidence(
    event: &Value,
    slug: &str,
    condition: &str,
    end_ms: i64,
    now: i64,
    source: &str,
) -> Value {
    let mut audit = failure(
        slug,
        condition,
        end_ms,
        now,
        source,
        "opening_field_missing",
    );
    let market = event["markets"]
        .as_array()
        .and_then(|markets| (markets.len() == 1).then(|| &markets[0]));
    let reason = if now < end_ms - 300_000 || now >= end_ms {
        Some("outside_live_market_window")
    } else if event["slug"].as_str() != Some(slug)
        || market.is_none_or(|m| {
            m["slug"].as_str() != Some(slug)
                || !m["conditionId"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(condition))
        })
    {
        Some("opening_market_identity_mismatch")
    } else if event["closed"] != false || market.unwrap()["closed"] != false {
        Some("opening_market_closed_or_state_unknown")
    } else if !event["eventMetadata"]["priceToBeat"].is_null()
        && number(&event["eventMetadata"]["priceToBeat"]).is_none()
    {
        Some("opening_field_invalid")
    } else {
        None
    };
    if let Some(reason) = reason {
        audit["reason"] = json!(reason);
    } else if let Some(price) = number(&event["eventMetadata"]["priceToBeat"]) {
        audit["verified"] = json!(true);
        audit["official_price_to_beat"] = json!(price);
        audit["reason"] = json!("verified_live_opening");
    }
    audit["metadata_present"] = json!(event["eventMetadata"].is_object());
    audit["source_capability"] = json!(if audit["verified"] == true {
        "available"
    } else if audit["reason"] == "opening_field_missing" {
        "live_opening_field_unavailable"
    } else {
        "not_verified"
    });
    audit
}

/// Parse JSON payloads only. Never execute scripts, scrape a displayed number,
/// or use unrelated history/final-price queries as an opening observation.
pub fn page_queries(html: &str) -> Vec<Value> {
    let mut flight = String::new();
    for tail in html.split("self.__next_f.push(").skip(1) {
        if let Ok(value) = Value::deserialize(&mut serde_json::Deserializer::from_str(tail)) {
            if value[0] == 1 {
                if let Some(text) = value[1].as_str() {
                    flight.push_str(text);
                }
            }
        }
    }
    let mut queries = Vec::new();
    for tail in flight.split("\"queries\"").skip(1) {
        let Some(tail) = tail.trim_start().strip_prefix(':') else {
            continue;
        };
        if !tail.trim_start().starts_with('[') {
            continue;
        }
        if let Ok(Value::Array(values)) =
            Value::deserialize(&mut serde_json::Deserializer::from_str(tail.trim_start()))
        {
            queries.extend(values);
        }
    }
    queries
}

pub fn page_evidence(html: &str, slug: &str, condition: &str, end_ms: i64, now: i64) -> Value {
    let mut audit = failure(
        slug,
        condition,
        end_ms,
        now,
        "polymarket_event_page",
        "opening_page_query_missing",
    );
    let start = end_ms - 300_000;
    let Some(asset) = slug
        .strip_suffix(&format!("-updown-5m-{}", start / 1000))
        .filter(|s| matches!(*s, "btc" | "eth" | "sol"))
    else {
        audit["reason"] = json!("opening_page_window_identity_mismatch");
        return audit;
    };
    if !(start..end_ms).contains(&now) {
        audit["reason"] = json!("outside_live_market_window");
        return audit;
    }
    let queries = page_queries(html);
    let mut event_opening = None;
    let events: Vec<_> = queries
        .iter()
        .filter(|q| q["queryKey"][0] == "/api/event/slug" && q["queryKey"][1] == slug)
        .collect();
    if events.is_empty() {
        audit["reason"] = json!("opening_page_event_identity_missing");
        return audit;
    }
    for query in events {
        let event = &query["state"]["data"];
        let matched = event_evidence(event, slug, condition, end_ms, now, "polymarket_event_page");
        if query["state"]["status"] != "success"
            || !matches!(
                matched["reason"].as_str(),
                Some("opening_field_missing" | "verified_live_opening")
            )
        {
            audit["reason"] = json!("opening_page_event_identity_or_state_mismatch");
            return audit;
        }
        let market = &event["markets"][0];
        if market["endDate"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis())
            != Some(end_ms)
        {
            audit["reason"] = json!("opening_page_window_identity_mismatch");
            return audit;
        }
        if let Some(price) = number(&event["eventMetadata"]["priceToBeat"]) {
            if event_opening.is_some_and(|prior: f64| ((price / prior).ln() * 10000.0).abs() > 1e-6)
            {
                audit["reason"] = json!("opening_page_conflicting_event_opening");
                return audit;
            }
            event_opening = Some(price);
        }
        let cfg = &market["cryptoMarketConfig"];
        let resolution = market["resolution_source"]
            .as_str()
            .or_else(|| market["resolutionSource"].as_str());
        if cfg["asset"] != asset
            || cfg["duration"] != "5m"
            || cfg["twapEnabled"] != true
            || cfg["twapLookbackSeconds"] != 60
            || resolution
                != Some(
                    format!("https://data.chain.link/streams/{asset}-usd-twap-60s-streams")
                        .as_str(),
                )
        {
            audit["reason"] = json!("opening_page_reference_rule_mismatch");
            return audit;
        }
    }
    let timestamp = |value: &Value| {
        value
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis())
    };
    let mut found: Option<(f64, i64, Value)> = None;
    for query in &queries {
        let key = &query["queryKey"];
        if key.as_array().map(Vec::len) != Some(8)
            || key[0] != "crypto-prices"
            || key[1] != "price"
            || key[2] != asset.to_ascii_uppercase()
            || timestamp(&key[3]) != Some(start)
            || key[4] != "fiveminute"
            || timestamp(&key[5]) != Some(end_ms)
            || key[6] != true
            || key[7] != 60
        {
            continue;
        }
        let state = &query["state"];
        let updated = state["dataUpdatedAt"].as_i64();
        if state["status"] != "success"
            || state["data"].get("closePrice") != Some(&Value::Null)
            || !updated.is_some_and(|t| (start..=now).contains(&t))
        {
            audit["reason"] = json!("opening_page_noncausal_or_terminal_query");
            return audit;
        }
        let Some(price) = number(&state["data"]["openPrice"]) else {
            audit["reason"] = json!("opening_page_price_invalid");
            return audit;
        };
        if found
            .as_ref()
            .is_some_and(|(prior, _, _)| ((price / prior).ln() * 10000.0).abs() > 1e-6)
        {
            audit["reason"] = json!("opening_page_conflicting_queries");
            return audit;
        }
        found = Some((price, updated.unwrap(), key.clone()));
    }
    if let Some((price, updated, key)) = found {
        if event_opening.is_some_and(|p: f64| ((price / p).ln() * 10000.0).abs() > 1e-6) {
            audit["reason"] = json!("opening_page_conflicting_event_opening");
            return audit;
        }
        audit["verified"] = json!(true);
        audit["official_price_to_beat"] = json!(price);
        audit["reason"] = json!("verified_live_opening");
        audit["query_key"] = key;
        audit["source_data_updated_at_ms"] = json!(updated);
        audit["reference_rule"] = json!("chainlink_twap_sixty");
        audit["source_timestamp_policy"] = json!(
            "immutable window opening; payload timestamp preserved; NOT a fresh market quote"
        );
    }
    audit["source_capability"] = json!(if audit["verified"] == true {
        "available"
    } else {
        "not_verified"
    });
    audit
}

async fn fetch_page(client: &reqwest::Client, slug: &str, condition: &str, end_ms: i64) -> Value {
    let started = chrono::Utc::now().timestamp_millis();
    // The official SSR page is ~1.2 MiB; local measured complete responses
    // take ~2s. Bound the whole transfer, still within the 20s capture deadline.
    let result = tokio::time::timeout(Duration::from_millis(3000), async {
        let mut response = client
            .get(format!("https://polymarket.com/event/{slug}"))
            .header("User-Agent", "PolyPulse-opening-audit/1.0")
            .send()
            .await
            .map_err(|_| "opening_page_request_failed")?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let mut value = failure(
                slug,
                condition,
                end_ms,
                chrono::Utc::now().timestamp_millis(),
                "polymarket_event_page",
                "opening_page_http_non_success",
            );
            value["http_status"] = json!(status);
            return Ok(value);
        }
        if response.url().host_str() != Some("polymarket.com")
            || response.url().path() != format!("/event/{slug}")
        {
            return Err("opening_page_redirect_identity_mismatch");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "opening_page_body_failed")?
        {
            if bytes.len() + chunk.len() > MAX_PAGE_BYTES {
                return Err("opening_page_too_large");
            }
            bytes.extend_from_slice(&chunk);
        }
        let html = std::str::from_utf8(&bytes).map_err(|_| "opening_page_utf8_invalid")?;
        let mut value = page_evidence(
            html,
            slug,
            condition,
            end_ms,
            chrono::Utc::now().timestamp_millis(),
        );
        use sha2::{Digest, Sha256};
        value["response_sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
        value["response_bytes"] = json!(bytes.len());
        value["http_status"] = json!(status);
        Ok::<_, &str>(value)
    })
    .await;
    let mut value = match result {
        Ok(Ok(value)) => value,
        Ok(Err(reason)) => failure(
            slug,
            condition,
            end_ms,
            chrono::Utc::now().timestamp_millis(),
            "polymarket_event_page",
            reason,
        ),
        Err(_) => failure(
            slug,
            condition,
            end_ms,
            chrono::Utc::now().timestamp_millis(),
            "polymarket_event_page",
            "opening_page_timeout",
        ),
    };
    value["request_started_at_ms"] = json!(started);
    value
}

/// A standalone public-data capability probe, not a trade or profitability test.
pub async fn run_probe(symbols: &str, output: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use std::io::Write;
    let assets: Vec<String> = symbols
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    if assets.is_empty()
        || assets
            .iter()
            .any(|s| !matches!(s.as_str(), "btc" | "eth" | "sol"))
    {
        bail!("opening-probe supports comma-separated btc,eth,sol only");
    }
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let discoverer = crate::market::MarketDiscoverer::new(assets, 5);
    let start = discoverer.calculate_current_window_timestamp(chrono::Utc::now());
    let markets = discoverer.get_markets_for_timestamp(start).await?;
    if markets.is_empty() {
        bail!("opening-probe discovered no live markets; retry in a new window");
    }
    let mut records = Vec::new();
    for market in markets {
        records.push(json!({"asset":market.crypto_symbol,"slug":market.slug,
            "evidence":fetch(&client,&market.slug,&market.market_id.to_string(),(start+300)*1000).await}));
    }
    let report = json!({"generated_at":chrono::Utc::now(),"window_start_ms":start*1000,
        "capture_version":CAPTURE_VERSION,"purpose":"opening_source_capability_only",
        "actual_order_submitted":false,"records":records});
    let parent = output.parent().context("probe report parent missing")?;
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .context("probe refuses to overwrite an existing report; choose a new filename")?;
    writeln!(file, "{}", serde_json::to_string_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    println!("Opening probe: {}", output.display());
    Ok(())
}

pub async fn fetch(client: &reqwest::Client, slug: &str, condition: &str, end_ms: i64) -> Value {
    let mut attempts = Vec::new();
    let mut last = Value::Null;
    // Official full event endpoint first; list endpoint is a bounded fallback.
    for source in ["gamma_event_by_slug", "gamma_events_list"] {
        let started = chrono::Utc::now().timestamp_millis();
        let request = if source == "gamma_event_by_slug" {
            client.get(format!(
                "https://gamma-api.polymarket.com/events/slug/{slug}"
            ))
        } else {
            client
                .get("https://gamma-api.polymarket.com/events")
                .query(&[("slug", slug)])
        };
        let result = request.timeout(Duration::from_millis(1500)).send().await;
        let (status, body) = match result {
            Ok(response) => {
                let status = response.status().as_u16();
                if !response.status().is_success() {
                    (Some(status), Err("opening_http_non_success"))
                } else {
                    (
                        Some(status),
                        response
                            .json::<Value>()
                            .await
                            .map_err(|_| "opening_json_decode_failed"),
                    )
                }
            }
            Err(_) => (None, Err("opening_request_failed")),
        };
        let now = chrono::Utc::now().timestamp_millis();
        let event = body.as_ref().ok().and_then(|body| {
            if source == "gamma_events_list" {
                body.as_array()?.iter().find(|event| event["slug"] == slug)
            } else {
                body.is_object().then_some(body)
            }
        });
        last = match event {
            Some(event) => event_evidence(event, slug, condition, end_ms, now, source),
            None => failure(
                slug,
                condition,
                end_ms,
                now,
                source,
                body.err()
                    .unwrap_or("opening_event_not_found_or_wrong_shape"),
            ),
        };
        last["request_started_at_ms"] = json!(started);
        last["http_status"] = json!(status);
        attempts.push(
            json!({"source":source,"observed_at_ms":now,"http_status":status,
            "reason":last["reason"],"elapsed_ms":now-started}),
        );
        if last["verified"] == true {
            break;
        }
        // A valid, identity-matched response without the field is a source
        // capability limitation, not a request failure. Do not blindly repeat
        // it on an equivalent Gamma list route.
        if last["reason"] == "opening_field_missing" {
            last = fetch_page(client, slug, condition, end_ms).await;
            let now = chrono::Utc::now().timestamp_millis();
            attempts.push(
                json!({"source":"polymarket_event_page","observed_at_ms":now,
                "http_status":last["http_status"],"reason":last["reason"],
                "elapsed_ms":now-last["request_started_at_ms"].as_i64().unwrap_or(now)}),
            );
            last["gamma_source_capability"] = json!("live_opening_field_unavailable");
            last["gamma_list_fallback"] = json!("skipped_equivalent_field_missing_response");
            break;
        }
    }
    last["attempts"] = json!(attempts);
    last
}

#[derive(Default)]
pub struct OpeningCache {
    entries: BTreeMap<String, (Value, Option<Value>, bool)>,
}

impl OpeningCache {
    pub fn record(&mut self, mut audit: Value) {
        if audit["verified"] == true
            && (number(&audit["official_price_to_beat"]).is_none()
                || audit["observed_at_ms"].as_i64().is_none()
                || audit["window_end_ms"].as_i64().is_none())
        {
            audit["verified"] = json!(false);
            audit["official_price_to_beat"] = Value::Null;
            audit["reason"] = json!("opening_cache_evidence_invalid");
        }
        let slug = audit["slug"].as_str().unwrap_or_default().to_owned();
        let entry = self
            .entries
            .entry(slug)
            .or_insert((audit.clone(), None, false));
        if audit["verified"] == true {
            if let Some(first) = &entry.1 {
                if first["condition_id"] != audit["condition_id"]
                    || (number(&first["official_price_to_beat"]).unwrap()
                        / number(&audit["official_price_to_beat"]).unwrap())
                    .ln()
                    .abs()
                        * 10000.0
                        > 1e-6
                {
                    entry.2 = true; // Never silently overwrite an immutable opening price.
                }
            } else {
                entry.1 = Some(audit.clone());
            }
        }
        if audit["observed_at_ms"].as_i64() >= entry.0["observed_at_ms"].as_i64() {
            entry.0 = audit;
        }
        while self.entries.len() > 128 {
            let key = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.0["window_end_ms"].as_i64().unwrap_or(0))
                .map(|(key, _)| key.clone())
                .unwrap();
            self.entries.remove(&key);
        }
    }

    pub fn snapshot(&self, slug: &str, condition: &str, end_ms: i64, now: i64) -> Value {
        let Some((last, first, conflict)) = self.entries.get(slug) else {
            return failure(
                slug,
                condition,
                end_ms,
                now,
                "gamma_event_cache",
                "opening_not_queried",
            );
        };
        if *conflict {
            return failure(
                slug,
                condition,
                end_ms,
                now,
                "gamma_event_cache",
                "opening_value_conflict",
            );
        }
        let eligible = |audit: &&Value| {
            let stamp = audit["observed_at_ms"].as_i64().unwrap_or(0);
            audit["verified"] == true
                && audit["slug"] == slug
                && audit["condition_id"] == condition
                && audit["window_end_ms"] == end_ms
                && now < end_ms
                && stamp >= end_ms - 300_000
                && (0..=MAX_OPENING_AGE_MS).contains(&(now - stamp))
        };
        let mut value = std::iter::once(last)
            .chain(first.iter())
            .filter(eligible)
            .max_by_key(|a| a["observed_at_ms"].as_i64())
            .cloned()
            .unwrap_or_else(|| {
                let mut value = last.clone();
                value["verified"] = json!(false);
                value["official_price_to_beat"] = Value::Null;
                if last["verified"] == true {
                    value["reason"] = json!("opening_cached_evidence_stale_or_not_asof");
                }
                value
            });
        value["cache_first_verified_at_ms"] = first
            .as_ref()
            .map(|a| a["observed_at_ms"].clone())
            .unwrap_or(Value::Null);
        value["cache_last_lookup_reason"] = last["reason"].clone();
        value["cache_timestamp_policy"] =
            json!("original_receive_time_preserved; same-market <=10s only");
        value
    }
}

pub fn capture_summary(events: &[Value]) -> Value {
    let mut checkpoints = 0u64;
    let mut fees = 0u64;
    let mut opening = 0u64;
    let mut midpoints = 0u64;
    let mut baseline = BTreeMap::<String, u64>::new();
    let mut opening_reasons = BTreeMap::<String, u64>::new();
    let mut connections = BTreeMap::<String, u64>::new();
    let mut coverage = BTreeMap::<String, u64>::new();
    let mut model_readiness = BTreeMap::<String, u64>::new();
    let mut topology = BTreeMap::<String, u64>::new();
    let mut attempts = BTreeMap::<String, u64>::new();
    let mut plans = BTreeMap::<String, (i64, i64)>::new();
    let mut phases = BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    let mut watermark = 0i64;
    let mut ended = false;
    for event in events {
        if let Some(t) = event["recorded_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        {
            watermark = watermark.max(t.timestamp_millis());
        }
        ended |= event["event"] == "run_stopped";
        if event["event"] == "checkpoint_capture" {
            let key = format!(
                "{}/{}",
                event["slug"].as_str().unwrap_or("unknown"),
                event["checkpoint_secs"]
            );
            phases
                .entry(key.clone())
                .or_default()
                .insert(event["status"].as_str().unwrap_or("unknown").into());
            if event["status"] == "planned" {
                plans.insert(
                    key,
                    (
                        event["scheduled_at_ms"].as_i64().unwrap_or(i64::MAX),
                        event["window_end_ms"].as_i64().unwrap_or(i64::MAX),
                    ),
                );
            }
        }
        if event["event"] == "opening_reference_observation" {
            for attempt in event["evidence"]["attempts"]
                .as_array()
                .into_iter()
                .flatten()
            {
                *attempts
                    .entry(format!(
                        "{}/{}/{}",
                        attempt["source"].as_str().unwrap_or("unknown"),
                        attempt["http_status"],
                        attempt["reason"].as_str().unwrap_or("unknown")
                    ))
                    .or_default() += 1;
            }
        }
        if event["event"] == "reference_feed_connection" {
            let key = format!(
                "{}/{}/{}",
                event["connection"].as_str().unwrap_or("unknown"),
                event["phase"].as_str().unwrap_or("unknown"),
                event["reason"].as_str().unwrap_or("unknown")
            );
            *connections.entry(key).or_default() += 1;
        }
        let buckets = if event["event"] == "residual_execution_observation" {
            let signal = &event["signal"];
            checkpoints += 1;
            *model_readiness
                .entry(format!(
                    "{}/{}/{}",
                    event["asset"].as_str().unwrap_or("unknown"),
                    signal["model_status"]
                        .as_str()
                        .unwrap_or("legacy_unspecified"),
                    signal["model_unavailable_reason"]
                        .as_str()
                        .unwrap_or("none")
                ))
                .or_default() += 1;
            for phase in ["signal", "delayed"] {
                *topology
                    .entry(format!(
                        "{}/{}/{}",
                        event["asset"].as_str().unwrap_or("unknown"),
                        phase,
                        book_topology(&event[phase]["books"])
                    ))
                    .or_default() += 1;
            }
            if signal["market_fee_audit"]["verified"] == true {
                fees += 1;
            }
            if signal["opening_reference_audit"]["verified_before_signal"] == true
                && signal["opening_reference_audit"]["within_tolerance"] == true
            {
                opening += 1;
            } else {
                *opening_reasons
                    .entry(
                        signal["opening_reference_audit"]["reason"]
                            .as_str()
                            .unwrap_or("legacy_missing_or_unverified_opening")
                            .into(),
                    )
                    .or_default() += 1;
            }
            let source = signal["baseline"]["source"].as_str().unwrap_or("unknown");
            if matches!(source, "up_midpoint" | "down_midpoint") {
                midpoints += 1;
            }
            *baseline
                .entry(format!(
                    "{}/{}",
                    event["asset"].as_str().unwrap_or("unknown"),
                    source
                ))
                .or_default() += 1;
            &signal["reference_buckets"]
        } else if event["event"] == "terminal_reference_window" {
            &event["reference_buckets"]
        } else {
            continue;
        };
        for stream in buckets["streams"].as_array().into_iter().flatten() {
            let eligible = if event["event"] == "terminal_reference_window" {
                let final_window = &stream["coverage"]["known_final_window"];
                (final_window["expected_seconds"].as_i64() == Some(60))
                    .then(|| final_window["observed_seconds"].as_i64().unwrap_or(0) >= 54)
            } else {
                stream["coverage"]["trailing_proxy_eligible"].as_bool()
            };
            let status = match eligible {
                Some(true) => "proxy_eligible",
                Some(false) => "insufficient_54_of_60_seconds",
                None => "legacy_coverage_not_embedded",
            };
            *coverage
                .entry(format!(
                    "{}/{}/{}",
                    event["event"].as_str().unwrap_or("unknown"),
                    stream["topic"].as_str().unwrap_or("unknown"),
                    status
                ))
                .or_default() += 1;
        }
    }
    let mut schedule = BTreeMap::<String, u64>::new();
    for (key, (due, end)) in &plans {
        if *due == i64::MAX || *end == i64::MAX {
            *schedule.entry("invalid_plan".into()).or_default() += 1;
            continue;
        }
        if *due > watermark {
            *schedule.entry("not_yet_due".into()).or_default() += 1;
            continue;
        }
        *schedule.entry("due".into()).or_default() += 1;
        let seen = &phases[key];
        let status = if seen.contains("captured") {
            "captured"
        } else if seen.contains("failed") {
            "failed"
        } else if ended || watermark >= end.saturating_add(20000) {
            "missed_or_interrupted"
        } else {
            "in_progress_or_snapshot_incomplete"
        };
        *schedule.entry(status.into()).or_default() += 1;
    }
    json!({"checkpoints":checkpoints,"fee_metadata_verified":fees,"causal_opening_verified":opening,
        "market_midpoint_available":midpoints,"baseline_sources":baseline,"opening_reasons":opening_reasons,
        "connection_audit":{"available":!connections.is_empty(),"events":connections},
        "reference_coverage":coverage,"model_readiness":model_readiness,"book_topology":topology,
        "opening_source_attempts":attempts,
        "checkpoint_schedule":{"available":!plans.is_empty(),"planned":(!plans.is_empty()).then_some(plans.len()),
            "snapshot_watermark_ms":watermark,"counts":schedule},
        "scope":"capture quality, not profit; legacy missing fields are unknown"})
}

pub fn book_topology(pair: &Value) -> &'static str {
    let books = [&pair["up"], &pair["down"]];
    if books.iter().any(|b| b["valid"] != true) {
        return "invalid";
    }
    if books
        .iter()
        .any(|b| b["best_bid"].as_f64().is_some() && b["best_ask"].as_f64().is_some())
    {
        return "two_sided";
    }
    if books
        .iter()
        .any(|b| b["best_bid"].as_f64().is_some() || b["best_ask"].as_f64().is_some())
    {
        "single_sided"
    } else {
        "empty"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event() -> Value {
        json!({"slug":"btc-updown-5m-0","closed":false,
        "markets":[{"slug":"btc-updown-5m-0","conditionId":"0xabc","closed":false}],
        "eventMetadata":{"priceToBeat":"100.0","finalPrice":999.0}})
    }
    fn evidence(event: &Value, now: i64) -> Value {
        event_evidence(event, "btc-updown-5m-0", "0xabc", 300000, now, "test")
    }
    fn page_fixture(mut event: Value, key: Value, data: Value, updated: i64) -> String {
        if event["markets"][0]["cryptoMarketConfig"].is_null() {
            event["markets"][0]["cryptoMarketConfig"] =
                json!({"asset":"btc","duration":"5m","twapEnabled":true,"twapLookbackSeconds":60});
        }
        if event["markets"][0]["resolution_source"].is_null() {
            event["markets"][0]["resolution_source"] =
                json!("https://data.chain.link/streams/btc-usd-twap-60s-streams");
        }
        event["markets"][0]["endDate"] = json!("1970-01-01T00:05:00Z");
        let q = json!({"queries":[{"queryKey":["/api/event/slug","btc-updown-5m-0","en"],"state":{"status":"success","data":event}},
            {"queryKey":key,"state":{"status":"success","data":data,"dataUpdatedAt":updated}}]});
        let flight = format!("62:[\"$\",null,{}]\n", q);
        let mid = flight.len() / 2;
        format!(
            "<script>self.__next_f.push({})</script><script>self.__next_f.push({})</script>",
            json!([1, &flight[..mid]]),
            json!([1, &flight[mid..]])
        )
    }
    fn price_key() -> Value {
        json!([
            "crypto-prices",
            "price",
            "BTC",
            "1970-01-01T00:00:00Z",
            "fiveminute",
            "1970-01-01T00:05:00Z",
            true,
            60
        ])
    }
    #[test]
    fn page_opening_requires_qualified_live_twap_query_and_market_identity() {
        let html = page_fixture(
            event(),
            price_key(),
            json!({"openPrice":100,"closePrice":null}),
            500,
        );
        let a = page_evidence(&html, "btc-updown-5m-0", "0xabc", 300000, 1000);
        assert_eq!(a["verified"], true);
        assert_eq!(a["official_price_to_beat"], 100.0);
        assert_eq!(a["source_data_updated_at_ms"], 500);
        assert!(!a.to_string().contains("closePrice"));
        assert_eq!(
            page_evidence(&html, "btc-updown-5m-0", "wrong", 300000, 1000)["verified"],
            false
        );
        assert_eq!(
            page_evidence(&html, "btc-updown-5m-0", "0xabc", 300000, 300000)["verified"],
            false
        );
        for (i, v) in [
            (2, json!("ETH")),
            (3, json!("1970-01-01T00:01:00Z")),
            (4, json!("fifteenminute")),
            (6, json!(false)),
            (7, json!(30)),
        ] {
            let mut k = price_key();
            k[i] = v;
            assert_eq!(
                page_evidence(
                    &page_fixture(event(), k, json!({"openPrice":100}), 500),
                    "btc-updown-5m-0",
                    "0xabc",
                    300000,
                    1000
                )["verified"],
                false
            );
        }
    }
    #[test]
    fn page_opening_rejects_terminal_future_invalid_and_closed_payloads() {
        for (data, updated) in [
            (json!({"openPrice":100,"closePrice":101}), 500),
            (json!({"openPrice":100}), 1001),
            (json!({"openPrice":0}), 500),
        ] {
            assert_eq!(
                page_evidence(
                    &page_fixture(event(), price_key(), data, updated),
                    "btc-updown-5m-0",
                    "0xabc",
                    300000,
                    1000
                )["verified"],
                false
            );
        }
        let mut closed = event();
        closed["closed"] = json!(true);
        assert_eq!(
            page_evidence(
                &page_fixture(closed, price_key(), json!({"openPrice":100}), 500),
                "btc-updown-5m-0",
                "0xabc",
                300000,
                1000
            )["verified"],
            false
        );
        let mut wrong_rule = event();
        wrong_rule["markets"][0]["cryptoMarketConfig"] = json!({"twapEnabled":false});
        assert_eq!(
            page_evidence(
                &page_fixture(wrong_rule, price_key(), json!({"openPrice":100}), 500),
                "btc-updown-5m-0",
                "0xabc",
                300000,
                1000
            )["reason"],
            "opening_page_reference_rule_mismatch"
        );
        assert_eq!(
            page_evidence(
                &page_fixture(
                    event(),
                    price_key(),
                    json!({"openPrice":101,"closePrice":null}),
                    500
                ),
                "btc-updown-5m-0",
                "0xabc",
                300000,
                1000
            )["reason"],
            "opening_page_conflicting_event_opening"
        );
    }
    #[test]
    fn capture_schedule_does_not_call_future_or_inflight_windows_missing() {
        let planned = |slug: &str, due: i64, end: i64| {
            json!({"event":"checkpoint_capture","slug":slug,"checkpoint_secs":30,
            "scheduled_at_ms":due,"window_end_ms":end,"status":"planned","recorded_at":"1970-01-01T00:00:01Z"})
        };
        let summary = capture_summary(&[
            planned("old", 1000, 30000),
            planned("tail", 100000, 130000),
            planned("inflight", 59900, 90000),
            json!({"event":"terminal_reference_window","recorded_at":"1970-01-01T00:01:00Z"}),
        ]);
        assert_eq!(
            summary["checkpoint_schedule"]["counts"]["missed_or_interrupted"],
            1
        );
        assert_eq!(summary["checkpoint_schedule"]["counts"]["not_yet_due"], 1);
        assert_eq!(
            summary["checkpoint_schedule"]["counts"]["in_progress_or_snapshot_incomplete"],
            1
        );
    }
    #[test]
    fn accepts_only_live_identity_matched_positive_opening_and_never_final() {
        let mut e = event();
        let a = evidence(&e, 1000);
        assert_eq!(a["verified"], true);
        assert_eq!(a["official_price_to_beat"], 100.0);
        assert!(!a.to_string().contains("finalPrice"));
        for field in ["slug", "conditionId"] {
            e = event();
            e["markets"][0][field] = json!("wrong");
            assert_eq!(
                evidence(&e, 1000)["reason"],
                "opening_market_identity_mismatch"
            );
        }
        e = event();
        e["closed"] = json!(true);
        assert_eq!(evidence(&e, 1000)["verified"], false);
        e = event();
        e["eventMetadata"]["priceToBeat"] = Value::Null;
        assert_eq!(evidence(&e, 1000)["reason"], "opening_field_missing");
        e["eventMetadata"]["priceToBeat"] = json!(-1);
        assert_eq!(evidence(&e, 1000)["reason"], "opening_field_invalid");
        assert_eq!(evidence(&event(), 300000)["verified"], false);
    }
    #[test]
    fn cache_does_not_refresh_receive_time_or_allow_future_stale_or_conflicting_openings() {
        let mut cache = OpeningCache::default();
        cache.record(evidence(&event(), 1000));
        assert_eq!(
            cache.snapshot("btc-updown-5m-0", "0xabc", 300000, 1500)["observed_at_ms"],
            1000
        );
        assert_eq!(
            cache.snapshot("btc-updown-5m-0", "0xabc", 300000, 999)["verified"],
            false
        );
        assert_eq!(
            cache.snapshot("btc-updown-5m-0", "0xabc", 300000, 12000)["verified"],
            false
        );
        assert_eq!(
            cache.snapshot("btc-updown-5m-0", "wrong", 300000, 1500)["verified"],
            false
        );
        let mut changed = event();
        changed["eventMetadata"]["priceToBeat"] = json!(101);
        cache.record(evidence(&changed, 2000));
        cache.record(evidence(&event(), 3000));
        assert_eq!(
            cache.snapshot("btc-updown-5m-0", "0xabc", 300000, 3500)["reason"],
            "opening_value_conflict"
        );
    }
    #[test]
    fn malformed_cache_evidence_fails_closed_and_terminal_coverage_uses_final_window() {
        let mut bad = evidence(&event(), 1000);
        bad["official_price_to_beat"] = json!("not-a-price");
        let mut cache = OpeningCache::default();
        cache.record(bad);
        assert_eq!(
            cache.snapshot("btc-updown-5m-0", "0xabc", 300000, 1500)["verified"],
            false
        );
        let summary = capture_summary(&[json!({"event":"terminal_reference_window",
            "reference_buckets":{"streams":[{"topic":"test","coverage":{
                "trailing_proxy_eligible":true,"known_final_window":{"expected_seconds":60,"observed_seconds":40}}}]}})]);
        assert_eq!(
            summary["reference_coverage"]
                ["terminal_reference_window/test/insufficient_54_of_60_seconds"],
            1
        );
    }
}
