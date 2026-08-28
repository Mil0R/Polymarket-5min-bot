use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

use super::client::{BayesianResearchClient, DiscoveryDiagnostics};
use super::config::BayesianArbConfig;
use super::model::{evaluate_relationship, MarketRelationship, RelationshipEvaluation};

struct ResearchJournal {
    writer: Mutex<RotatingJournalWriter>,
}

impl ResearchJournal {
    fn open(path: &Path, max_bytes: u64, retention: usize) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            writer: Mutex::new(RotatingJournalWriter::open(
                path.to_path_buf(),
                max_bytes,
                retention,
            )?),
        })
    }

    fn record(&self, action: &'static str, data: &impl Serialize) -> Result<()> {
        let event = json!({
            "action": action,
            "version": 2,
            "timestamp": Utc::now(),
            "data": data,
        });
        let mut encoded = serde_json::to_vec(&event)?;
        encoded.push(b'\n');
        let mut writer = self.writer.lock().expect("Bayesian journal lock poisoned");
        writer.write_record(&encoded)?;
        Ok(())
    }
}

struct RotatingJournalWriter {
    path: std::path::PathBuf,
    file: File,
    bytes_written: u64,
    max_bytes: u64,
    retention: usize,
}

impl RotatingJournalWriter {
    fn open(path: std::path::PathBuf, max_bytes: u64, retention: usize) -> Result<Self> {
        if path
            .metadata()
            .is_ok_and(|metadata| metadata.len() >= max_bytes)
        {
            rotate_result_files(&path, retention)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            bytes_written,
            max_bytes,
            retention,
        })
    }

    fn write_record(&mut self, record: &[u8]) -> Result<()> {
        if self.bytes_written > 0
            && self.bytes_written.saturating_add(record.len() as u64) > self.max_bytes
        {
            self.file.flush()?;
            rotate_result_files(&self.path, self.retention)?;
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            self.bytes_written = 0;
        }
        self.file.write_all(record)?;
        self.file.flush()?;
        self.bytes_written = self.bytes_written.saturating_add(record.len() as u64);
        Ok(())
    }
}

fn rotated_result_path(path: &Path, index: usize) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.{}", path.display(), index))
}

fn rotate_result_files(path: &Path, retention: usize) -> Result<()> {
    let oldest = rotated_result_path(path, retention);
    if oldest.exists() {
        fs::remove_file(&oldest)?;
    }
    for index in (1..retention).rev() {
        let source = rotated_result_path(path, index);
        if source.exists() {
            fs::rename(&source, rotated_result_path(path, index + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, rotated_result_path(path, 1))?;
    }
    Ok(())
}

pub async fn run_scan(client: &BayesianResearchClient) -> Result<Vec<RelationshipEvaluation>> {
    let config = client.config();
    let journal = ResearchJournal::open(
        &config.results_file,
        config.results_max_bytes,
        config.results_retention_files,
    )?;
    let discovery = client.discover_snapshot().await?;
    let relationships = discovery.relationships;
    journal.record(
        "scan_started",
        &json!({
            "mode": "one_shot_shadow",
            "relationship_count": relationships.len(),
            "config": config_summary(config),
        }),
    )?;
    journal.record("discovery_summary", &discovery.diagnostics)?;
    for relationship in &relationships {
        journal.record("relationship_discovered", relationship)?;
    }
    let evaluations = evaluate_cycle(client, &relationships).await;
    for evaluation in &evaluations {
        journal.record("relationship_evaluation", evaluation)?;
        if evaluation.qualifies {
            journal.record("shadow_opportunity", evaluation)?;
        }
    }
    journal.record(
        "scan_completed",
        &json!({
            "relationships": relationships.len(),
            "evaluations": evaluations.len(),
            "opportunities": evaluations.iter().filter(|item| item.qualifies).count(),
        }),
    )?;
    print_evaluations(&evaluations);
    Ok(evaluations)
}

pub async fn run_shadow(client: BayesianResearchClient) -> Result<()> {
    let config = client.config().clone();
    let discovery = client.discover_snapshot().await?;
    let mut relationships = discovery.relationships;
    if relationships.is_empty() {
        bail!("no Bayesian coherence relationships were discovered");
    }
    let journal = ResearchJournal::open(
        &config.results_file,
        config.results_max_bytes,
        config.results_retention_files,
    )?;
    journal.record(
        "run_started",
        &json!({
            "mode": "continuous_shadow",
            "relationship_count": relationships.len(),
            "config": config_summary(&config),
        }),
    )?;
    journal.record("discovery_summary", &discovery.diagnostics)?;
    for relationship in &relationships {
        journal.record("relationship_discovered", relationship)?;
    }
    tracing::info!(
        relationships = relationships.len(),
        poll_seconds = config.poll_seconds,
        output = %config.results_file.display(),
        "Bayesian Coherence shadow scanner started"
    );

    let started_at = Utc::now();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut ticker = interval(Duration::from_secs(config.poll_seconds));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_logged = HashMap::<String, (String, DateTime<Utc>)>::new();
    let mut cycles = 0_u64;
    let mut evaluations_seen = 0_u64;
    let mut evaluations_logged = 0_u64;
    let mut opportunities_logged = 0_u64;
    let mut last_relationship_refresh = started_at;
    let mut last_summary = started_at;

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("failed to listen for Ctrl-C")?;
                break;
            }
            _ = ticker.tick() => {}
        }
        let now = Utc::now();
        if config.duration_seconds > 0
            && now.signed_duration_since(started_at).num_seconds()
                >= i64::try_from(config.duration_seconds).unwrap_or(i64::MAX)
        {
            break;
        }
        if now
            .signed_duration_since(last_relationship_refresh)
            .num_seconds()
            >= i64::try_from(config.relationship_refresh_seconds).unwrap_or(i64::MAX)
        {
            match refresh_relationships(&client, &mut relationships, &mut last_logged, &journal)
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    journal.record(
                        "relationship_refresh_error",
                        &json!({"error": error.to_string()}),
                    )?;
                }
            }
            last_relationship_refresh = now;
        }
        cycles += 1;
        let evaluations = evaluate_cycle(&client, &relationships).await;
        evaluations_seen += evaluations.len() as u64;
        let cycle_best_edge = evaluations.iter().filter_map(|item| item.net_edge).max();
        let cycle_complete = evaluations
            .iter()
            .filter(|item| item.net_edge.is_some())
            .count();
        let cycle_missing_or_shallow = evaluations.len().saturating_sub(cycle_complete);
        for evaluation in evaluations {
            let signature = evaluation_signature(&evaluation);
            let should_log =
                last_logged
                    .get(&evaluation.relationship_id)
                    .is_none_or(|(previous, logged_at)| {
                        previous != &signature
                            || now.signed_duration_since(*logged_at).num_seconds()
                                >= i64::try_from(config.unchanged_log_seconds).unwrap_or(i64::MAX)
                    });
            if !should_log {
                continue;
            }
            journal.record("relationship_evaluation", &evaluation)?;
            evaluations_logged += 1;
            if evaluation.qualifies {
                journal.record("shadow_opportunity", &evaluation)?;
                opportunities_logged += 1;
                tracing::info!(
                    relationship = %evaluation.relationship_id,
                    event = %evaluation.event_slug,
                    size = %evaluation.size_shares.unwrap_or_default(),
                    edge = %evaluation.net_edge.unwrap_or_default().round_dp(4),
                    profit = %evaluation.net_profit_usdc.unwrap_or_default().round_dp(4),
                    semantic_status = %evaluation.semantic_status,
                    "Bayesian shadow opportunity"
                );
            }
            last_logged.insert(evaluation.relationship_id.clone(), (signature, now));
        }
        if now.signed_duration_since(last_summary).num_seconds()
            >= i64::try_from(config.summary_seconds).unwrap_or(i64::MAX)
        {
            journal.record(
                "cycle_summary",
                &json!({
                    "cycles": cycles,
                    "relationships": relationships.len(),
                    "cycle_complete_evaluations": cycle_complete,
                    "cycle_missing_or_shallow": cycle_missing_or_shallow,
                    "cycle_best_edge": cycle_best_edge,
                    "evaluations_seen": evaluations_seen,
                    "evaluations_logged": evaluations_logged,
                    "opportunities_logged": opportunities_logged,
                }),
            )?;
            last_summary = now;
        }
    }
    journal.record(
        "run_stopped",
        &json!({
            "started_at": started_at,
            "stopped_at": Utc::now(),
            "cycles": cycles,
            "evaluations_seen": evaluations_seen,
            "evaluations_logged": evaluations_logged,
            "opportunities_logged": opportunities_logged,
        }),
    )?;
    Ok(())
}

async fn evaluate_cycle(
    client: &BayesianResearchClient,
    relationships: &[MarketRelationship],
) -> Vec<RelationshipEvaluation> {
    let books = client.fetch_books(relationships).await;
    let config = client.config();
    let now = Utc::now();
    relationships
        .iter()
        .map(|relationship| {
            evaluate_relationship(
                relationship,
                &books,
                &config.size_ladder_shares,
                config.min_bundle_notional_usdc,
                config.min_net_edge,
                config.execution_buffer_pct,
                now,
            )
        })
        .collect()
}

fn evaluation_signature(evaluation: &RelationshipEvaluation) -> String {
    let size_curve = evaluation
        .size_evaluations
        .iter()
        .map(|point| {
            format!(
                "{}:{}:{}",
                point.size_shares,
                point
                    .net_edge
                    .map(|value| value.round_dp(4).to_string())
                    .unwrap_or_default(),
                point.rejection_reason.as_deref().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    format!(
        "{}:{}:{}:{}:{}",
        evaluation.qualifies,
        evaluation
            .net_edge
            .map(|value| value.round_dp(4).to_string())
            .unwrap_or_default(),
        evaluation
            .size_shares
            .map(|value| value.to_string())
            .unwrap_or_default(),
        evaluation.rejection_reason.as_deref().unwrap_or_default(),
        size_curve,
    )
}

async fn refresh_relationships(
    client: &BayesianResearchClient,
    relationships: &mut Vec<MarketRelationship>,
    last_logged: &mut HashMap<String, (String, DateTime<Utc>)>,
    journal: &ResearchJournal,
) -> Result<()> {
    let snapshot = client.discover_snapshot().await?;
    journal.record("discovery_summary", &snapshot.diagnostics)?;
    if snapshot.relationships.is_empty() {
        journal.record(
            "relationship_refresh_rejected",
            &json!({"reason": "empty_discovery_kept_previous_relationships"}),
        )?;
        return Ok(());
    }
    let previous_ids = relationships
        .iter()
        .map(|relationship| relationship.relationship_id.clone())
        .collect::<HashSet<_>>();
    let next_ids = snapshot
        .relationships
        .iter()
        .map(|relationship| relationship.relationship_id.clone())
        .collect::<HashSet<_>>();
    for relationship in snapshot
        .relationships
        .iter()
        .filter(|relationship| !previous_ids.contains(&relationship.relationship_id))
    {
        journal.record("relationship_added", relationship)?;
    }
    for relationship_id in previous_ids.difference(&next_ids) {
        journal.record(
            "relationship_removed",
            &json!({"relationship_id": relationship_id}),
        )?;
    }
    last_logged.retain(|relationship_id, _| next_ids.contains(relationship_id));
    let added = next_ids.difference(&previous_ids).count();
    let removed = previous_ids.difference(&next_ids).count();
    *relationships = snapshot.relationships;
    journal.record(
        "relationship_refresh",
        &json!({
            "relationships": relationships.len(),
            "added": added,
            "removed": removed,
        }),
    )?;
    Ok(())
}

fn config_summary(config: &BayesianArbConfig) -> serde_json::Value {
    json!({
        "results_file": config.results_file,
        "event_pages": config.event_pages,
        "events_per_page": config.events_per_page,
        "max_relationships": config.max_relationships,
        "max_partition_legs": config.max_partition_legs,
        "poll_seconds": config.poll_seconds,
        "unchanged_log_seconds": config.unchanged_log_seconds,
        "relationship_refresh_seconds": config.relationship_refresh_seconds,
        "summary_seconds": config.summary_seconds,
        "duration_seconds": config.duration_seconds,
        "results_max_bytes": config.results_max_bytes,
        "results_retention_files": config.results_retention_files,
        "size_ladder_shares": config.size_ladder_shares,
        "min_bundle_notional_usdc": config.min_bundle_notional_usdc,
        "min_net_edge": config.min_net_edge,
        "execution_buffer_pct": config.execution_buffer_pct,
        "semantic_allowlist": config.semantic_allowlist,
        "execution": "read_only_shadow_never_submits_orders",
    })
}

fn print_evaluations(evaluations: &[RelationshipEvaluation]) {
    let mut ranked = evaluations.iter().collect::<Vec<_>>();
    ranked.sort_by_key(|evaluation| std::cmp::Reverse(evaluation.net_edge.unwrap_or(Decimal::MIN)));
    println!(
        "Bayesian Coherence scan: relationships={} opportunities={}",
        evaluations.len(),
        evaluations.iter().filter(|item| item.qualifies).count()
    );
    println!("edge      profit     size  kind                         event");
    for evaluation in ranked.into_iter().take(20) {
        println!(
            "{:>8} {:>10} {:>8} {:<28} {}",
            evaluation
                .net_edge
                .map(|value| format!("{:.2}%", value * Decimal::ONE_HUNDRED))
                .unwrap_or_else(|| "-".into()),
            evaluation
                .net_profit_usdc
                .map(|value| value.round_dp(4).to_string())
                .unwrap_or_else(|| "-".into()),
            evaluation
                .size_shares
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into()),
            format!("{:?}", evaluation.kind),
            evaluation.event_title,
        );
    }
    println!("All relationships remain shadow-only; template-inferred deadline chains require manual rule review.");
}

#[derive(Default)]
struct ReportSummary {
    evaluations: u64,
    opportunities: u64,
    max_edge: Option<Decimal>,
    total_positive_profit: Decimal,
}

#[derive(Default)]
struct SizeReportSummary {
    observations: u64,
    complete: u64,
    opportunities: u64,
    raw_positive: u64,
    max_edge: Option<Decimal>,
}

pub fn run_report(path: &Path) -> Result<()> {
    let report_paths = report_input_paths(path);
    if report_paths.is_empty() {
        bail!("no Bayesian result files found for {}", path.display());
    }
    let mut relationships = HashSet::new();
    let mut by_kind = BTreeMap::<String, ReportSummary>::new();
    let mut by_size = BTreeMap::<Decimal, SizeReportSummary>::new();
    let mut rejections = BTreeMap::<String, u64>::new();
    let mut latest_discovery: Option<DiscoveryDiagnostics> = None;
    let mut discovery_summaries = 0_u64;
    for input_path in &report_paths {
        let reader = BufReader::new(
            File::open(input_path)
                .with_context(|| format!("failed to open {}", input_path.display()))?,
        );
        for (line_number, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!(
                    "failed reading {} line {}",
                    input_path.display(),
                    line_number + 1
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let event: serde_json::Value = serde_json::from_str(&line).with_context(|| {
                format!(
                    "invalid NDJSON in {} at line {}",
                    input_path.display(),
                    line_number + 1
                )
            })?;
            match event.get("action").and_then(serde_json::Value::as_str) {
                Some("discovery_summary") => {
                    latest_discovery = Some(serde_json::from_value(event["data"].clone())?);
                    discovery_summaries += 1;
                }
                Some("relationship_evaluation") => {
                    let evaluation: RelationshipEvaluation =
                        serde_json::from_value(event["data"].clone()).with_context(|| {
                            format!(
                                "invalid evaluation in {} at line {}",
                                input_path.display(),
                                line_number + 1
                            )
                        })?;
                    relationships.insert(evaluation.relationship_id.clone());
                    let summary = by_kind.entry(format!("{:?}", evaluation.kind)).or_default();
                    summary.evaluations += 1;
                    if evaluation.qualifies {
                        summary.opportunities += 1;
                    }
                    if let Some(edge) = evaluation.net_edge {
                        summary.max_edge =
                            Some(summary.max_edge.map_or(edge, |current| current.max(edge)));
                    }
                    if let Some(profit) = evaluation
                        .net_profit_usdc
                        .filter(|value| *value > Decimal::ZERO)
                    {
                        summary.total_positive_profit += profit;
                    }
                    if let Some(reason) = evaluation.rejection_reason {
                        *rejections.entry(reason).or_default() += 1;
                    }
                    for point in evaluation.size_evaluations {
                        let size_summary = by_size.entry(point.size_shares).or_default();
                        size_summary.observations += 1;
                        if let Some(edge) = point.net_edge {
                            size_summary.complete += 1;
                            size_summary.max_edge = Some(
                                size_summary
                                    .max_edge
                                    .map_or(edge, |current| current.max(edge)),
                            );
                        }
                        size_summary.opportunities += u64::from(point.qualifies);
                        size_summary.raw_positive += u64::from(
                            point
                                .raw_cost_usdc
                                .is_some_and(|cost| cost < point.guaranteed_payout_usdc),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    println!("Bayesian Coherence Shadow report: {}", path.display());
    println!(
        "files={} unique_relationships={} discovery_summaries={}",
        report_paths.len(),
        relationships.len(),
        discovery_summaries,
    );
    println!("kind                          evals  opportunities  max_edge  positive_profit_sum");
    for (kind, summary) in by_kind {
        println!(
            "{kind:<29} {:>5} {:>14} {:>9} {:>20}",
            summary.evaluations,
            summary.opportunities,
            summary
                .max_edge
                .map(|value| format!("{:.2}%", value * Decimal::ONE_HUNDRED))
                .unwrap_or_else(|| "-".into()),
            summary.total_positive_profit.round_dp(4),
        );
    }
    println!("rejections:");
    for (reason, count) in rejections {
        println!("  {reason}: {count}");
    }
    if !by_size.is_empty() {
        println!("size_curve:");
        println!("  shares  observations  complete  opportunities  raw_positive  max_edge");
        for (size, summary) in by_size {
            println!(
                "  {:>6} {:>13} {:>9} {:>14} {:>13} {:>9}",
                size,
                summary.observations,
                summary.complete,
                summary.opportunities,
                summary.raw_positive,
                summary
                    .max_edge
                    .map(|value| format!("{:.2}%", value * Decimal::ONE_HUNDRED))
                    .unwrap_or_else(|| "-".into()),
            );
        }
    }
    if let Some(discovery) = latest_discovery {
        println!(
            "latest_discovery: events={} relationships={}/{} truncated={} neg_risk_events={} emitted={}",
            discovery.events_scanned,
            discovery.relationships_emitted,
            discovery.relationships_before_limit,
            discovery.relationships_truncated,
            discovery.neg_risk_events_seen,
            discovery.neg_risk_events_emitted,
        );
        for (reason, count) in discovery.neg_risk_rejections {
            println!("  neg_risk_rejected {reason}: {count}");
        }
        for (reason, count) in discovery.neg_risk_market_rejections {
            println!("  neg_risk_market_rejected {reason}: {count}");
        }
    }
    println!("Note: opportunity counts are executable-book snapshots, not atomic multi-market fills or realized PnL.");
    Ok(())
}

fn report_input_paths(path: &Path) -> Vec<std::path::PathBuf> {
    let mut paths = (1..=100)
        .rev()
        .map(|index| rotated_result_path(path, index))
        .filter(|candidate| candidate.exists())
        .collect::<Vec<_>>();
    if path.exists() {
        paths.push(path.to_path_buf());
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::super::model::RelationshipKind;
    use super::*;

    #[test]
    fn signature_ignores_timestamp_but_tracks_material_change() {
        let base = RelationshipEvaluation {
            relationship_id: "r".into(),
            event_id: "e".into(),
            event_slug: "e".into(),
            event_title: "e".into(),
            kind: RelationshipKind::DeadlineImplication,
            semantic_status: "shadow".into(),
            live_eligible: false,
            observed_at: Utc::now(),
            minimum_payout_per_share: Decimal::ONE,
            size_shares: Some(Decimal::TEN),
            guaranteed_payout_usdc: Some(Decimal::TEN),
            raw_cost_usdc: Some(Decimal::TEN),
            taker_fee_usdc: Some(Decimal::ZERO),
            execution_buffer_usdc: Some(Decimal::ZERO),
            total_cost_usdc: Some(Decimal::TEN),
            net_profit_usdc: Some(Decimal::ZERO),
            net_edge: Some(Decimal::ZERO),
            qualifies: false,
            rejection_reason: Some("net_edge_below_threshold".into()),
            legs: vec![],
            size_evaluations: vec![],
        };
        let mut changed_time = base.clone();
        changed_time.observed_at = Utc::now() + chrono::Duration::seconds(10);
        assert_eq!(
            evaluation_signature(&base),
            evaluation_signature(&changed_time)
        );
        changed_time.qualifies = true;
        assert_ne!(
            evaluation_signature(&base),
            evaluation_signature(&changed_time)
        );
    }

    #[test]
    fn journal_rotation_preserves_bounded_generations() {
        let directory = std::env::temp_dir().join(format!(
            "bayesian-rotate-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("events.ndjson");
        let mut writer = RotatingJournalWriter::open(path.clone(), 16, 2).unwrap();
        writer.write_record(b"0123456789\n").unwrap();
        writer.write_record(b"abcdefghij\n").unwrap();
        writer.write_record(b"ABCDEFGHIJ\n").unwrap();
        assert!(path.exists());
        assert!(rotated_result_path(&path, 1).exists());
        assert!(rotated_result_path(&path, 2).exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
