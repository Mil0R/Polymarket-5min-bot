use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

use super::client::{
    BookFetchDiagnostics, CrossMarketProbabilityResearchClient, DiscoveryDiagnostics,
};
use super::config::CrossMarketProbabilityArbitrageConfig;
use super::model::{
    evaluate_maker_taker_relationship, evaluate_relationship_with_buffers, MakerTakerEvaluation,
    MarketRelationship, RelationshipEvaluation,
};

struct ResearchJournal {
    writer: Mutex<RotatingJournalWriter>,
    run_id: String,
}

impl ResearchJournal {
    fn open(path: &Path, max_bytes: u64, retention: usize, run_id: String) -> Result<Self> {
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
            run_id,
        })
    }

    fn record(&self, action: &'static str, data: &impl Serialize) -> Result<()> {
        let event = json!({
            "action": action,
            "version": 3,
            "run_id": self.run_id,
            "timestamp": Utc::now(),
            "data": data,
        });
        let mut encoded = serde_json::to_vec(&event)?;
        encoded.push(b'\n');
        let mut writer = self
            .writer
            .lock()
            .expect("cross-market probability journal lock poisoned");
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

pub async fn run_scan(
    client: &CrossMarketProbabilityResearchClient,
) -> Result<Vec<RelationshipEvaluation>> {
    let config = client.config();
    let run_id = format!("scan-{}", uuid::Uuid::new_v4());
    let journal = ResearchJournal::open(
        &config.results_file,
        config.results_max_bytes,
        config.results_retention_files,
        run_id,
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
    let cycle = evaluate_cycle(client, &relationships).await;
    journal.record("book_fetch_summary", &cycle.fetch_diagnostics)?;
    for evaluation in &cycle.evaluations {
        journal.record("relationship_evaluation", evaluation)?;
        if evaluation.qualifies {
            journal.record("shadow_opportunity", evaluation)?;
        }
    }
    for evaluation in &cycle.maker_taker_evaluations {
        journal.record("conditional_maker_taker_evaluation", evaluation)?;
        if evaluation.research_candidate {
            journal.record("conditional_maker_taker_candidate", evaluation)?;
        }
    }
    journal.record(
        "scan_completed",
        &json!({
            "relationships": relationships.len(),
            "evaluations": cycle.evaluations.len(),
            "opportunities": cycle.evaluations.iter().filter(|item| item.qualifies).count(),
            "book_fetch": cycle.fetch_diagnostics,
            "conditional_maker_taker_evaluations": cycle.maker_taker_evaluations.len(),
            "conditional_maker_taker_candidates": cycle.maker_taker_evaluations.iter().filter(|item| item.research_candidate).count(),
        }),
    )?;
    print_evaluations(&cycle.evaluations);
    Ok(cycle.evaluations)
}

pub async fn run_shadow(client: CrossMarketProbabilityResearchClient) -> Result<()> {
    let config = client.config().clone();
    let discovery = client.discover_snapshot().await?;
    let mut relationships = discovery.relationships;
    if relationships.is_empty() {
        bail!("no cross-market probability relationships were discovered");
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let journal = ResearchJournal::open(
        &config.results_file,
        config.results_max_bytes,
        config.results_retention_files,
        run_id.clone(),
    )?;
    let summary_journal = ResearchJournal::open(
        &config.summary_file,
        config.results_max_bytes,
        config.results_retention_files,
        run_id.clone(),
    )?;
    journal.record(
        "run_started",
        &json!({
            "mode": "continuous_shadow",
            "run_id": run_id,
            "relationship_count": relationships.len(),
            "config": config_summary(&config),
        }),
    )?;
    summary_journal.record(
        "run_started",
        &json!({
            "mode": "continuous_shadow",
            "run_id": run_id,
            "relationship_count": relationships.len(),
            "config": config_summary(&config),
        }),
    )?;
    journal.record("discovery_summary", &discovery.diagnostics)?;
    summary_journal.record("discovery_summary", &discovery.diagnostics)?;
    for relationship in &relationships {
        journal.record("relationship_discovered", relationship)?;
    }
    tracing::info!(
        relationships = relationships.len(),
        poll_seconds = config.poll_seconds,
        output = %config.results_file.display(),
        "Cross-Market Probability shadow scanner started"
    );

    let started_at = Utc::now();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut ticker = interval(Duration::from_secs(config.poll_seconds));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_logged = HashMap::<String, (String, DateTime<Utc>)>::new();
    let mut last_maker_taker_logged = HashMap::<String, (String, DateTime<Utc>)>::new();
    let mut opportunity_episodes = HashMap::<String, (String, DateTime<Utc>)>::new();
    let mut near_miss_relationships = HashSet::<String>::new();
    let mut last_book_signatures = HashMap::<String, String>::new();
    let mut cycles = 0_u64;
    let mut evaluations_seen = 0_u64;
    let mut evaluations_logged = 0_u64;
    let mut opportunities_logged = 0_u64;
    let mut pricing_opportunities_rejected_by_sync = 0_u64;
    let mut maker_taker_evaluations_logged = 0_u64;
    let mut maker_taker_candidates_logged = 0_u64;
    let mut complete_evaluations = 0_u64;
    let mut missing_or_shallow_evaluations = 0_u64;
    let mut best_net_edge = None::<Decimal>;
    let mut best_gross_edge = None::<Decimal>;
    let mut best_post_fee_edge = None::<Decimal>;
    let mut max_book_age_ms = None::<i64>;
    let mut max_observed_at_skew_ms = None::<i64>;
    let mut max_exchange_timestamp_skew_ms = None::<i64>;
    let mut fetch_diagnostics = BookFetchDiagnostics::default();
    let mut last_relationship_refresh = started_at;
    let mut last_summary = started_at;
    let mut last_full_scan = started_at
        - chrono::Duration::seconds(i64::try_from(config.full_scan_seconds).unwrap_or(i64::MAX));

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
            match refresh_relationships(
                &client,
                &mut relationships,
                &mut last_logged,
                &journal,
                &summary_journal,
            )
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
        let force_full_scan = now.signed_duration_since(last_full_scan).num_seconds()
            >= i64::try_from(config.full_scan_seconds).unwrap_or(i64::MAX);
        let cycle = evaluate_changed_cycle(
            &client,
            &relationships,
            &last_book_signatures,
            force_full_scan,
        )
        .await;
        last_book_signatures = cycle.book_signatures;
        fetch_diagnostics.merge(&cycle.fetch_diagnostics);
        if force_full_scan {
            last_full_scan = now;
        }
        evaluations_seen += cycle.evaluations.len() as u64;
        let cycle_best_edge = cycle
            .evaluations
            .iter()
            .filter_map(|item| item.net_edge)
            .max();
        let cycle_complete = cycle
            .evaluations
            .iter()
            .filter(|item| item.net_edge.is_some())
            .count();
        let cycle_missing_or_shallow = cycle.evaluations.len().saturating_sub(cycle_complete);
        complete_evaluations += cycle_complete as u64;
        missing_or_shallow_evaluations += cycle_missing_or_shallow as u64;
        update_max(&mut best_net_edge, cycle_best_edge);
        for evaluation in &cycle.evaluations {
            update_max(&mut max_book_age_ms, evaluation.max_book_age_ms);
            update_max(
                &mut max_observed_at_skew_ms,
                evaluation.max_observed_at_skew_ms,
            );
            update_max(
                &mut max_exchange_timestamp_skew_ms,
                evaluation.max_exchange_timestamp_skew_ms,
            );
            for point in &evaluation.size_evaluations {
                let Some(raw_cost) = point.raw_cost_usdc else {
                    continue;
                };
                if point.guaranteed_payout_usdc <= Decimal::ZERO {
                    continue;
                }
                let gross =
                    (point.guaranteed_payout_usdc - raw_cost) / point.guaranteed_payout_usdc;
                update_max(&mut best_gross_edge, Some(gross));
                if let Some(fee) = point.taker_fee_usdc {
                    let post_fee = (point.guaranteed_payout_usdc - raw_cost - fee)
                        / point.guaranteed_payout_usdc;
                    update_max(&mut best_post_fee_edge, Some(post_fee));
                }
            }
        }
        for evaluation in cycle.evaluations {
            let pricing_opportunity =
                evaluation
                    .rejection_reason
                    .as_deref()
                    .is_some_and(|reason| {
                        reason == "book_age_exceeds_limit"
                            || reason == "book_age_unavailable"
                            || reason == "observed_at_skew_exceeds_limit"
                            || reason == "exchange_timestamp_skew_exceeds_limit"
                            || reason == "exchange_timestamp_unavailable"
                    });
            let signature = evaluation_signature(&evaluation);
            let is_near_miss = !evaluation.qualifies
                && evaluation
                    .net_edge
                    .is_some_and(|edge| edge >= config.near_miss_edge);
            let was_near_miss = near_miss_relationships.contains(&evaluation.relationship_id);
            if is_near_miss != was_near_miss {
                let action = if is_near_miss {
                    near_miss_relationships.insert(evaluation.relationship_id.clone());
                    "near_miss_entered"
                } else {
                    near_miss_relationships.remove(&evaluation.relationship_id);
                    "near_miss_exited"
                };
                journal.record(action, &evaluation)?;
                summary_journal.record(
                    action,
                    &json!({
                        "relationship_id": &evaluation.relationship_id,
                        "kind": evaluation.kind,
                        "semantic_status": &evaluation.semantic_status,
                        "net_edge": evaluation.net_edge,
                        "size_shares": evaluation.size_shares,
                    }),
                )?;
            }
            let heartbeat_seconds = if is_near_miss || evaluation.qualifies {
                config.near_miss_log_seconds
            } else {
                config.ordinary_log_seconds
            };
            let should_log =
                last_logged
                    .get(&evaluation.relationship_id)
                    .is_none_or(|(previous, logged_at)| {
                        (is_near_miss && previous != &signature)
                            || evaluation.qualifies
                            || now.signed_duration_since(*logged_at).num_seconds()
                                >= i64::try_from(heartbeat_seconds).unwrap_or(i64::MAX)
                    });
            if !should_log {
                continue;
            }
            journal.record("relationship_evaluation", &evaluation)?;
            evaluations_logged += 1;
            if pricing_opportunity {
                journal.record("pricing_opportunity_rejected_by_sync", &evaluation)?;
                pricing_opportunities_rejected_by_sync += 1;
            }
            if evaluation.qualifies {
                journal.record("shadow_opportunity", &evaluation)?;
                opportunities_logged += 1;
                let new_episode = opportunity_episodes
                    .get(&evaluation.relationship_id)
                    .is_none_or(|(_, last_seen)| {
                        now.signed_duration_since(*last_seen).num_seconds()
                            > i64::try_from(config.opportunity_episode_gap_seconds)
                                .unwrap_or(i64::MAX)
                    });
                let episode_id = if new_episode {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    opportunity_episodes
                        .get(&evaluation.relationship_id)
                        .map(|(id, _)| id.clone())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
                };
                opportunity_episodes.insert(
                    evaluation.relationship_id.clone(),
                    (episode_id.clone(), now),
                );
                if new_episode {
                    journal.record(
                        "opportunity_episode_started",
                        &json!({"episode_id": episode_id, "evaluation": &evaluation}),
                    )?;
                    if let Some(relationship) = relationships
                        .iter()
                        .find(|item| item.relationship_id == evaluation.relationship_id)
                    {
                        record_delayed_rechecks(&client, relationship, &episode_id, &journal)
                            .await?;
                    }
                }
                tracing::info!(
                    relationship = %evaluation.relationship_id,
                    event = %evaluation.event_slug,
                    size = %evaluation.size_shares.unwrap_or_default(),
                    edge = %evaluation.net_edge.unwrap_or_default().round_dp(4),
                    profit = %evaluation.net_profit_usdc.unwrap_or_default().round_dp(4),
                    semantic_status = %evaluation.semantic_status,
                    "Cross-market probability shadow opportunity"
                );
            }
            last_logged.insert(evaluation.relationship_id.clone(), (signature, now));
        }
        for evaluation in cycle.maker_taker_evaluations {
            if evaluation.conditional_edge < config.near_miss_edge {
                continue;
            }
            let signature = maker_taker_signature(&evaluation);
            let should_log = last_maker_taker_logged
                .get(&evaluation.relationship_id)
                .is_none_or(|(previous, logged_at)| {
                    previous != &signature
                        || evaluation.research_candidate
                        || now.signed_duration_since(*logged_at).num_seconds()
                            >= i64::try_from(config.maker_taker_log_seconds).unwrap_or(i64::MAX)
                });
            if !should_log {
                continue;
            }
            journal.record("conditional_maker_taker_evaluation", &evaluation)?;
            maker_taker_evaluations_logged += 1;
            if evaluation.research_candidate {
                journal.record("conditional_maker_taker_candidate", &evaluation)?;
                maker_taker_candidates_logged += 1;
                tracing::info!(
                    relationship = %evaluation.relationship_id,
                    event = %evaluation.event_slug,
                    size = %evaluation.size_shares,
                    edge = %evaluation.conditional_edge.round_dp(4),
                    queue_ahead = %evaluation.visible_queue_ahead_shares,
                    "Conditional maker/taker research candidate (maker fill not simulated)"
                );
            }
            last_maker_taker_logged.insert(evaluation.relationship_id.clone(), (signature, now));
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
                    "near_miss_relationships": near_miss_relationships.len(),
                    "evaluations_seen": evaluations_seen,
                    "evaluations_logged": evaluations_logged,
                    "opportunities_logged": opportunities_logged,
                    "pricing_opportunities_rejected_by_sync": pricing_opportunities_rejected_by_sync,
                    "opportunity_episodes": opportunity_episodes.len(),
                    "conditional_maker_taker_evaluations_logged": maker_taker_evaluations_logged,
                    "conditional_maker_taker_candidates_logged": maker_taker_candidates_logged,
                    "complete_evaluations": complete_evaluations,
                    "missing_or_shallow_evaluations": missing_or_shallow_evaluations,
                    "best_net_edge": best_net_edge,
                    "best_gross_edge": best_gross_edge,
                    "best_post_fee_edge": best_post_fee_edge,
                    "book_fetch": fetch_diagnostics,
                    "rest_latency_average_ms": fetch_diagnostics.rest_latency_average_ms(),
                    "max_book_age_ms": max_book_age_ms,
                    "max_observed_at_skew_ms": max_observed_at_skew_ms,
                    "max_exchange_timestamp_skew_ms": max_exchange_timestamp_skew_ms,
                }),
            )?;
            summary_journal.record(
                "cycle_summary",
                &json!({
                    "cycles": cycles,
                    "relationships": relationships.len(),
                    "cycle_complete_evaluations": cycle_complete,
                    "cycle_missing_or_shallow": cycle_missing_or_shallow,
                    "cycle_best_edge": cycle_best_edge,
                    "near_miss_relationships": near_miss_relationships.len(),
                    "evaluations_seen": evaluations_seen,
                    "evaluations_logged": evaluations_logged,
                    "opportunities_logged": opportunities_logged,
                    "pricing_opportunities_rejected_by_sync": pricing_opportunities_rejected_by_sync,
                    "opportunity_episodes": opportunity_episodes.len(),
                    "conditional_maker_taker_evaluations_logged": maker_taker_evaluations_logged,
                    "conditional_maker_taker_candidates_logged": maker_taker_candidates_logged,
                    "complete_evaluations": complete_evaluations,
                    "missing_or_shallow_evaluations": missing_or_shallow_evaluations,
                    "best_net_edge": best_net_edge,
                    "best_gross_edge": best_gross_edge,
                    "best_post_fee_edge": best_post_fee_edge,
                    "book_fetch": fetch_diagnostics,
                    "rest_latency_average_ms": fetch_diagnostics.rest_latency_average_ms(),
                    "max_book_age_ms": max_book_age_ms,
                    "max_observed_at_skew_ms": max_observed_at_skew_ms,
                    "max_exchange_timestamp_skew_ms": max_exchange_timestamp_skew_ms,
                }),
            )?;
            last_summary = now;
        }
    }
    let stopped_at = Utc::now();
    let final_summary = json!({
        "started_at": started_at,
        "stopped_at": stopped_at,
        "duration_seconds": stopped_at.signed_duration_since(started_at).num_seconds(),
        "cycles": cycles,
        "relationships": relationships.len(),
        "evaluations_seen": evaluations_seen,
        "evaluations_logged": evaluations_logged,
        "opportunities_logged": opportunities_logged,
        "pricing_opportunities_rejected_by_sync": pricing_opportunities_rejected_by_sync,
        "opportunity_episodes": opportunity_episodes.len(),
        "conditional_maker_taker_evaluations_logged": maker_taker_evaluations_logged,
        "conditional_maker_taker_candidates_logged": maker_taker_candidates_logged,
        "active_near_misses": near_miss_relationships.len(),
        "complete_evaluations": complete_evaluations,
        "missing_or_shallow_evaluations": missing_or_shallow_evaluations,
        "best_net_edge": best_net_edge,
        "best_gross_edge": best_gross_edge,
        "best_post_fee_edge": best_post_fee_edge,
        "book_fetch": fetch_diagnostics,
        "rest_latency_average_ms": fetch_diagnostics.rest_latency_average_ms(),
        "max_book_age_ms": max_book_age_ms,
        "max_observed_at_skew_ms": max_observed_at_skew_ms,
        "max_exchange_timestamp_skew_ms": max_exchange_timestamp_skew_ms,
    });
    journal.record("final_cycle_summary", &final_summary)?;
    summary_journal.record("final_cycle_summary", &final_summary)?;
    journal.record("run_stopped", &final_summary)?;
    summary_journal.record("run_stopped", &final_summary)?;
    Ok(())
}

struct EvaluationCycle {
    evaluations: Vec<RelationshipEvaluation>,
    maker_taker_evaluations: Vec<MakerTakerEvaluation>,
    book_signatures: HashMap<String, String>,
    fetch_diagnostics: BookFetchDiagnostics,
}

async fn evaluate_cycle(
    client: &CrossMarketProbabilityResearchClient,
    relationships: &[MarketRelationship],
) -> EvaluationCycle {
    let batch = client.fetch_books_with_diagnostics(relationships).await;
    let book_signatures = batch
        .books
        .iter()
        .map(|(token_id, book)| (token_id.clone(), book_signature(book)))
        .collect();
    EvaluationCycle {
        evaluations: evaluate_relationships(client, relationships.iter(), &batch.books),
        maker_taker_evaluations: evaluate_maker_taker_relationships(
            client,
            relationships.iter(),
            &batch.books,
        ),
        book_signatures,
        fetch_diagnostics: batch.diagnostics,
    }
}

async fn evaluate_changed_cycle(
    client: &CrossMarketProbabilityResearchClient,
    relationships: &[MarketRelationship],
    previous_signatures: &HashMap<String, String>,
    force_full_scan: bool,
) -> EvaluationCycle {
    let batch = client.fetch_books_with_diagnostics(relationships).await;
    let signatures = batch
        .books
        .iter()
        .map(|(token_id, book)| (token_id.clone(), book_signature(book)))
        .collect::<HashMap<_, _>>();
    let mut changed_tokens = signatures
        .iter()
        .filter(|(token_id, signature)| previous_signatures.get(*token_id) != Some(*signature))
        .map(|(token_id, _)| token_id.clone())
        .collect::<HashSet<_>>();
    // A disappeared snapshot is also a material change. Re-evaluate now so
    // the relationship fails closed instead of retaining a stale complete
    // evaluation until the next forced full scan.
    changed_tokens.extend(
        previous_signatures
            .keys()
            .filter(|token_id| !signatures.contains_key(*token_id))
            .cloned(),
    );
    let selected = relationships
        .iter()
        .filter(|relationship| {
            force_full_scan
                || relationship
                    .legs
                    .iter()
                    .any(|leg| changed_tokens.contains(&leg.token_id))
        })
        .collect::<Vec<_>>();
    EvaluationCycle {
        evaluations: evaluate_relationships(client, selected.iter().copied(), &batch.books),
        maker_taker_evaluations: evaluate_maker_taker_relationships(
            client,
            selected.iter().copied(),
            &batch.books,
        ),
        book_signatures: signatures,
        fetch_diagnostics: batch.diagnostics,
    }
}

fn evaluate_maker_taker_relationships<'a>(
    client: &CrossMarketProbabilityResearchClient,
    relationships: impl Iterator<Item = &'a MarketRelationship>,
    books: &HashMap<String, super::model::BookSnapshot>,
) -> Vec<MakerTakerEvaluation> {
    let config = client.config();
    if !config.maker_taker_shadow {
        return Vec::new();
    }
    let now = Utc::now();
    relationships
        .filter_map(|relationship| {
            let mut evaluation = evaluate_maker_taker_relationship(
                relationship,
                books,
                &config.size_ladder_shares,
                config.min_bundle_notional_usdc,
                config.maker_taker_min_net_edge,
                config.maker_taker_execution_buffer_pct,
                now,
            )?;
            if evaluation.max_book_age_ms.is_none()
                || evaluation
                    .max_book_age_ms
                    .is_some_and(|value| value > config.max_book_age_ms)
                || evaluation.max_exchange_timestamp_skew_ms.is_none()
                || evaluation
                    .max_observed_at_skew_ms
                    .is_some_and(|value| value > config.max_observed_at_skew_ms)
                || evaluation
                    .max_exchange_timestamp_skew_ms
                    .is_some_and(|value| value > config.max_exchange_timestamp_skew_ms)
            {
                evaluation.research_candidate = false;
            }
            Some(evaluation)
        })
        .collect()
}

fn update_max<T: Ord + Copy>(target: &mut Option<T>, candidate: Option<T>) {
    if let Some(candidate) = candidate {
        *target = Some(target.map_or(candidate, |current| current.max(candidate)));
    }
}

fn evaluate_relationships<'a>(
    client: &CrossMarketProbabilityResearchClient,
    relationships: impl Iterator<Item = &'a MarketRelationship>,
    books: &HashMap<String, super::model::BookSnapshot>,
) -> Vec<RelationshipEvaluation> {
    let config = client.config();
    let now = Utc::now();
    relationships
        .map(|relationship| {
            let mut evaluation = evaluate_relationship_with_buffers(
                relationship,
                books,
                &config.size_ladder_shares,
                config.min_bundle_notional_usdc,
                config.min_net_edge,
                config.execution_buffer_pct,
                &config.execution_buffer_curve_pct,
                now,
            );
            apply_sync_gate(&mut evaluation, config);
            evaluation
        })
        .collect()
}

fn apply_sync_gate(
    evaluation: &mut RelationshipEvaluation,
    config: &CrossMarketProbabilityArbitrageConfig,
) {
    if !evaluation.qualifies {
        return;
    }
    let rejection = sync_rejection(
        evaluation,
        config.max_book_age_ms,
        config.max_observed_at_skew_ms,
        config.max_exchange_timestamp_skew_ms,
    );
    if let Some(rejection) = rejection {
        evaluation.qualifies = false;
        evaluation.rejection_reason = Some(rejection.into());
        for point in &mut evaluation.size_evaluations {
            if point.qualifies {
                point.qualifies = false;
                point.rejection_reason = Some(rejection.into());
            }
        }
    }
}

fn sync_rejection(
    evaluation: &RelationshipEvaluation,
    max_book_age_ms: i64,
    max_observed_at_skew_ms: i64,
    max_exchange_timestamp_skew_ms: i64,
) -> Option<&'static str> {
    if evaluation.max_book_age_ms.is_none() {
        Some("book_age_unavailable")
    } else if evaluation
        .max_book_age_ms
        .is_some_and(|value| value > max_book_age_ms)
    {
        Some("book_age_exceeds_limit")
    } else if evaluation
        .max_observed_at_skew_ms
        .is_some_and(|value| value > max_observed_at_skew_ms)
    {
        Some("observed_at_skew_exceeds_limit")
    } else if evaluation.max_exchange_timestamp_skew_ms.is_none() {
        Some("exchange_timestamp_unavailable")
    } else if evaluation
        .max_exchange_timestamp_skew_ms
        .is_some_and(|value| value > max_exchange_timestamp_skew_ms)
    {
        Some("exchange_timestamp_skew_exceeds_limit")
    } else {
        None
    }
}

async fn record_delayed_rechecks(
    client: &CrossMarketProbabilityResearchClient,
    relationship: &MarketRelationship,
    episode_id: &str,
    journal: &ResearchJournal,
) -> Result<()> {
    let config = client.config();
    let mut elapsed_ms = 0_u64;
    for delay_ms in &config.delayed_recheck_ms {
        tokio::time::sleep(Duration::from_millis(delay_ms.saturating_sub(elapsed_ms))).await;
        elapsed_ms = *delay_ms;
        let batch = client
            .fetch_books_with_diagnostics(std::slice::from_ref(relationship))
            .await;
        let mut evaluation = evaluate_relationship_with_buffers(
            relationship,
            &batch.books,
            &config.size_ladder_shares,
            config.min_bundle_notional_usdc,
            config.min_net_edge,
            config.execution_buffer_pct,
            &config.execution_buffer_curve_pct,
            Utc::now(),
        );
        apply_sync_gate(&mut evaluation, config);
        let stress = execution_stress(&evaluation, &batch.books, config);
        journal.record(
            "delayed_execution_recheck",
            &json!({
                "episode_id": episode_id,
                "delay_ms": delay_ms,
                "still_executable": evaluation.qualifies,
                "deployment_eligible": evaluation.qualifies && evaluation.live_eligible,
                "evaluation": evaluation,
                "book_fetch": batch.diagnostics,
                "stress": stress,
            }),
        )?;
    }
    Ok(())
}

fn execution_stress(
    evaluation: &RelationshipEvaluation,
    books: &HashMap<String, super::model::BookSnapshot>,
    config: &CrossMarketProbabilityArbitrageConfig,
) -> serde_json::Value {
    let ratio = config.partial_fill_ratio;
    let haircut = config.emergency_exit_haircut_pct;
    let mut full_cost = Decimal::ZERO;
    let mut partial_cost = Decimal::ZERO;
    let mut partial_exit = Decimal::ZERO;
    let mut worst_one_leg_pnl = None::<Decimal>;
    for (index, leg) in evaluation.legs.iter().enumerate() {
        let filled_shares = if index + 1 == evaluation.legs.len() {
            leg.shares * ratio
        } else {
            leg.shares
        };
        let cost = (leg.raw_cost_usdc + leg.taker_fee_usdc)
            * if index + 1 == evaluation.legs.len() {
                ratio
            } else {
                Decimal::ONE
            };
        full_cost += leg.raw_cost_usdc + leg.taker_fee_usdc;
        partial_cost += cost;
        let partial_leg_exit = books
            .get(&leg.token_id)
            .and_then(|book| walk_bids(&book.bids, filled_shares))
            .map(|proceeds| proceeds * (Decimal::ONE - haircut));
        if let Some(exit) = partial_leg_exit {
            partial_exit += exit;
        }
        let one_leg_exit = books
            .get(&leg.token_id)
            .and_then(|book| walk_bids(&book.bids, leg.shares))
            .map(|proceeds| proceeds * (Decimal::ONE - haircut));
        if let Some(exit) = one_leg_exit {
            let one_leg_cost = leg.raw_cost_usdc + leg.taker_fee_usdc;
            let pnl = exit - one_leg_cost;
            worst_one_leg_pnl = Some(worst_one_leg_pnl.map_or(pnl, |current| current.min(pnl)));
        } else {
            let one_leg_cost = leg.raw_cost_usdc + leg.taker_fee_usdc;
            worst_one_leg_pnl =
                Some(worst_one_leg_pnl.map_or(-one_leg_cost, |current| current.min(-one_leg_cost)));
        }
    }
    json!({
        "partial_fill_ratio": ratio,
        "partial_fill_emergency_exit_pnl_usdc": partial_exit - partial_cost,
        "worst_single_leg_emergency_exit_pnl_usdc": worst_one_leg_pnl,
        "full_bundle_cost_usdc": full_cost,
        "emergency_exit_haircut_pct": haircut,
        "assumption": "all filled exposure is immediately sold into visible bids; missing bid depth is valued at zero",
    })
}

fn walk_bids(levels: &[super::model::BookLevel], shares: Decimal) -> Option<Decimal> {
    let mut remaining = shares;
    let mut proceeds = Decimal::ZERO;
    let mut levels = levels.iter().collect::<Vec<_>>();
    levels.sort_by_key(|level| std::cmp::Reverse(level.price));
    for level in levels {
        let filled = remaining.min(level.size);
        proceeds += filled * level.price;
        remaining -= filled;
        if remaining <= Decimal::ZERO {
            return Some(proceeds);
        }
    }
    None
}

fn book_signature(book: &super::model::BookSnapshot) -> String {
    // Only executable ask depth is material to this buy-only bundle. Ignore
    // observation/exchange timestamps so HTTP polling cannot manufacture a
    // false change, but retain every level because deep ladder sizes may walk
    // beyond the first few asks.
    book.asks
        .iter()
        .map(|level| format!("{}@{}", level.size, level.price))
        .collect::<Vec<_>>()
        .join("|")
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

fn maker_taker_signature(evaluation: &MakerTakerEvaluation) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        evaluation.maker_token_id,
        evaluation.maker_price,
        evaluation.size_shares,
        evaluation.conditional_edge.round_dp(4),
        evaluation.research_candidate,
        evaluation.visible_queue_ahead_shares.round_dp(2),
    )
}

async fn refresh_relationships(
    client: &CrossMarketProbabilityResearchClient,
    relationships: &mut Vec<MarketRelationship>,
    last_logged: &mut HashMap<String, (String, DateTime<Utc>)>,
    journal: &ResearchJournal,
    summary_journal: &ResearchJournal,
) -> Result<()> {
    let snapshot = client.discover_snapshot().await?;
    journal.record("discovery_summary", &snapshot.diagnostics)?;
    summary_journal.record("discovery_summary", &snapshot.diagnostics)?;
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
    summary_journal.record(
        "relationship_refresh",
        &json!({
            "relationships": relationships.len(),
            "added": added,
            "removed": removed,
        }),
    )?;
    Ok(())
}

fn config_summary(config: &CrossMarketProbabilityArbitrageConfig) -> serde_json::Value {
    json!({
        "results_file": config.results_file,
        "summary_file": config.summary_file,
        "report_file": config.report_file,
        "semantic_review_file": config.semantic_review_file,
        "event_pages": config.event_pages,
        "events_per_page": config.events_per_page,
        "max_relationships": config.max_relationships,
        "max_partition_legs": config.max_partition_legs,
        "poll_seconds": config.poll_seconds,
        "full_scan_seconds": config.full_scan_seconds,
        "unchanged_log_seconds": config.unchanged_log_seconds,
        "relationship_refresh_seconds": config.relationship_refresh_seconds,
        "summary_seconds": config.summary_seconds,
        "duration_seconds": config.duration_seconds,
        "results_max_bytes": config.results_max_bytes,
        "results_retention_files": config.results_retention_files,
        "size_ladder_shares": config.size_ladder_shares,
        "min_bundle_notional_usdc": config.min_bundle_notional_usdc,
        "min_net_edge": config.min_net_edge,
        "near_miss_edge": config.near_miss_edge,
        "execution_buffer_pct": config.execution_buffer_pct,
        "execution_buffer_curve_pct": config.execution_buffer_curve_pct,
        "near_miss_log_seconds": config.near_miss_log_seconds,
        "ordinary_log_seconds": config.ordinary_log_seconds,
        "websocket_books": config.websocket_books,
        "websocket_book_max_age_ms": config.websocket_book_max_age_ms,
        "max_book_age_ms": config.max_book_age_ms,
        "max_observed_at_skew_ms": config.max_observed_at_skew_ms,
        "max_exchange_timestamp_skew_ms": config.max_exchange_timestamp_skew_ms,
        "opportunity_episode_gap_seconds": config.opportunity_episode_gap_seconds,
        "delayed_recheck_ms": config.delayed_recheck_ms,
        "partial_fill_ratio": config.partial_fill_ratio,
        "emergency_exit_haircut_pct": config.emergency_exit_haircut_pct,
        "semantic_allowlist": config.semantic_allowlist,
        "maker_taker_shadow": config.maker_taker_shadow,
        "maker_taker_min_net_edge": config.maker_taker_min_net_edge,
        "maker_taker_execution_buffer_pct": config.maker_taker_execution_buffer_pct,
        "maker_taker_log_seconds": config.maker_taker_log_seconds,
        "execution": "read_only_shadow_never_submits_orders",
    })
}

fn print_evaluations(evaluations: &[RelationshipEvaluation]) {
    let mut ranked = evaluations.iter().collect::<Vec<_>>();
    ranked.sort_by_key(|evaluation| std::cmp::Reverse(evaluation.net_edge.unwrap_or(Decimal::MIN)));
    println!(
        "Cross-Market Probability scan: relationships={} opportunities={}",
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
    max_gross_edge: Option<Decimal>,
    max_post_fee_edge: Option<Decimal>,
    gross_positive: u64,
    post_fee_positive: u64,
    net_positive: u64,
}

#[derive(Default)]
struct SizeReportSummary {
    observations: u64,
    complete: u64,
    opportunities: u64,
    raw_positive: u64,
    post_fee_positive: u64,
    net_positive: u64,
    max_gross_edge: Option<Decimal>,
    max_post_fee_edge: Option<Decimal>,
    max_edge: Option<Decimal>,
}

#[derive(Default)]
struct BufferReportSummary {
    observations: u64,
    opportunities: u64,
    positive: u64,
    max_edge: Option<Decimal>,
}

#[derive(Default)]
struct RunMetadata {
    mode: String,
    started_at: Option<DateTime<Utc>>,
    stopped_at: Option<DateTime<Utc>>,
    latest_at: Option<DateTime<Utc>>,
    completed: bool,
    evaluations_logged: u64,
    evaluations_seen: Option<u64>,
    unique_relationships: HashSet<String>,
    near_miss_entries: u64,
    opportunities: u64,
    maker_taker_evaluations: u64,
    maker_taker_candidates: u64,
    final_summary: Option<serde_json::Value>,
}

struct ReportEvent {
    run_id: String,
    action: String,
    timestamp: DateTime<Utc>,
    data: serde_json::Value,
}

pub fn run_report(path: &Path, config: &CrossMarketProbabilityArbitrageConfig) -> Result<()> {
    let report_paths = report_input_paths(path);
    if report_paths.is_empty() {
        bail!(
            "no cross-market probability result files found for {}",
            path.display()
        );
    }
    let mut events = Vec::<ReportEvent>::new();
    let mut runs = BTreeMap::<String, RunMetadata>::new();
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
            let Some(action) = event
                .get("action")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
            else {
                continue;
            };
            let run_id = event
                .get("run_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("legacy-unscoped")
                .to_string();
            let timestamp = event
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let data = event.get("data").cloned().unwrap_or_default();
            let run = runs.entry(run_id.clone()).or_default();
            run.latest_at = Some(
                run.latest_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
            match action.as_str() {
                "run_started" | "scan_started" => {
                    run.started_at = Some(timestamp);
                    run.mode = data
                        .get("mode")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(if action == "scan_started" {
                            "scan"
                        } else {
                            "shadow"
                        })
                        .to_string();
                }
                "run_stopped" | "scan_completed" => {
                    run.stopped_at = Some(timestamp);
                    run.completed = true;
                    run.evaluations_seen = data
                        .get("evaluations_seen")
                        .or_else(|| data.get("evaluations"))
                        .and_then(serde_json::Value::as_u64);
                    run.final_summary = Some(data.clone());
                }
                "final_cycle_summary" => run.final_summary = Some(data.clone()),
                "relationship_evaluation" => {
                    run.evaluations_logged += 1;
                    if let Some(id) = data
                        .get("relationship_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        run.unique_relationships.insert(id.to_string());
                    }
                }
                "relationship_discovered" => {
                    if let Some(id) = data
                        .get("relationship_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        run.unique_relationships.insert(id.to_string());
                    }
                }
                "near_miss_entered" => run.near_miss_entries += 1,
                "shadow_opportunity" => run.opportunities += 1,
                "conditional_maker_taker_evaluation" => run.maker_taker_evaluations += 1,
                "conditional_maker_taker_candidate" => run.maker_taker_candidates += 1,
                _ => {}
            }
            events.push(ReportEvent {
                run_id,
                action,
                timestamp,
                data,
            });
        }
    }
    if runs.is_empty() {
        bail!("no scoped journal events found in {}", path.display());
    }
    let selected_run_id = runs
        .iter()
        .filter(|(_, run)| run.completed && run.mode != "one_shot_shadow" && run.mode != "scan")
        .max_by_key(|(_, run)| run.stopped_at.or(run.latest_at))
        .or_else(|| {
            runs.iter()
                .filter(|(_, run)| run.completed)
                .max_by_key(|(_, run)| run.stopped_at.or(run.latest_at))
        })
        .or_else(|| runs.iter().max_by_key(|(_, run)| run.latest_at))
        .map(|(run_id, _)| run_id.clone())
        .context("result journal contains no selectable run")?;

    println!("Cross-Market Probability Shadow report: {}", path.display());
    println!(
        "files={} runs={} selected_run={selected_run_id}",
        report_paths.len(),
        runs.len()
    );
    println!("runs:");
    println!("  completed  start_utc             duration  logged/seen  relationships  near_miss  taker_opps  maker_quotes/candidates  run_id");
    let mut ordered_runs = runs.iter().collect::<Vec<_>>();
    ordered_runs.sort_by_key(|(_, run)| run.started_at.or(run.latest_at));
    for (run_id, run) in ordered_runs {
        let end = run.stopped_at.or(run.latest_at);
        let duration = run
            .started_at
            .zip(end)
            .map(|(start, end)| end.signed_duration_since(start).num_seconds())
            .unwrap_or_default();
        println!(
            "  {:>9}  {:<20} {:>8}s  {:>6}/{:<6} {:>13} {:>10} {:>10} {:>10}/{:<10}  {}{}",
            if run.completed { "yes" } else { "no" },
            run.started_at
                .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "-".into()),
            duration,
            run.evaluations_logged,
            run.evaluations_seen
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into()),
            run.unique_relationships.len(),
            run.near_miss_entries,
            run.opportunities,
            run.maker_taker_evaluations,
            run.maker_taker_candidates,
            run_id,
            if run_id == &selected_run_id {
                "  [selected]"
            } else {
                ""
            },
        );
    }

    let selected = runs
        .get(&selected_run_id)
        .context("selected run metadata disappeared")?;
    let mut relationships = HashSet::new();
    let mut by_kind = BTreeMap::<String, ReportSummary>::new();
    let mut by_semantic = BTreeMap::<String, ReportSummary>::new();
    let mut by_size = BTreeMap::<Decimal, SizeReportSummary>::new();
    let mut by_buffer = BTreeMap::<Decimal, BufferReportSummary>::new();
    let mut rejections = BTreeMap::<String, u64>::new();
    let mut latest_discovery: Option<DiscoveryDiagnostics> = None;
    let mut discovery_summaries = 0_u64;
    let mut near_miss_entries = 0_u64;
    let mut unique_economic_snapshots = HashSet::new();
    let mut net_edges = Vec::new();
    let mut gross_edges = Vec::new();
    let mut post_fee_edges = Vec::new();
    let mut source_observations = BTreeMap::<String, u64>::new();
    let mut max_book_age_ms = None::<i64>;
    let mut max_observed_skew_ms = None::<i64>;
    let mut max_exchange_skew_ms = None::<i64>;
    let mut active_near_misses = HashMap::<String, DateTime<Utc>>::new();
    let mut near_miss_duration_seconds = 0_i64;
    let mut maker_taker_evaluations = 0_u64;
    let mut maker_taker_candidates = 0_u64;
    let mut maker_taker_edges = Vec::<Decimal>::new();
    let mut maker_taker_queue_ahead = Vec::<Decimal>::new();
    for event in events
        .iter()
        .filter(|event| event.run_id == selected_run_id)
    {
        match event.action.as_str() {
            "near_miss_entered" => {
                near_miss_entries += 1;
                if let Some(id) = event
                    .data
                    .get("relationship_id")
                    .and_then(serde_json::Value::as_str)
                {
                    active_near_misses.insert(id.to_string(), event.timestamp);
                }
            }
            "near_miss_exited" => {
                if let Some(id) = event
                    .data
                    .get("relationship_id")
                    .and_then(serde_json::Value::as_str)
                {
                    if let Some(entered_at) = active_near_misses.remove(id) {
                        near_miss_duration_seconds += event
                            .timestamp
                            .signed_duration_since(entered_at)
                            .num_seconds()
                            .max(0);
                    }
                }
            }
            "discovery_summary" => {
                latest_discovery = Some(serde_json::from_value(event.data.clone())?);
                discovery_summaries += 1;
            }
            "relationship_evaluation" => {
                let evaluation: RelationshipEvaluation = serde_json::from_value(event.data.clone())
                    .with_context(|| {
                        format!("invalid evaluation in selected run {selected_run_id}")
                    })?;
                relationships.insert(evaluation.relationship_id.clone());
                *source_observations.entry("websocket".into()).or_default() +=
                    evaluation.websocket_books as u64;
                *source_observations.entry("rest".into()).or_default() +=
                    evaluation.rest_books as u64;
                *source_observations.entry("unknown".into()).or_default() +=
                    evaluation.unknown_source_books as u64;
                update_max(&mut max_book_age_ms, evaluation.max_book_age_ms);
                update_max(
                    &mut max_observed_skew_ms,
                    evaluation.max_observed_at_skew_ms,
                );
                update_max(
                    &mut max_exchange_skew_ms,
                    evaluation.max_exchange_timestamp_skew_ms,
                );
                unique_economic_snapshots.insert(format!(
                    "{}:{}:{}:{}:{}",
                    evaluation.relationship_id,
                    evaluation.size_shares.unwrap_or_default(),
                    evaluation.raw_cost_usdc.unwrap_or_default(),
                    evaluation.taker_fee_usdc.unwrap_or_default(),
                    evaluation.net_edge.unwrap_or_default(),
                ));
                let kind = format!("{:?}", evaluation.kind);
                let summary = by_kind.entry(kind).or_default();
                summary.evaluations += 1;
                summary.opportunities += u64::from(evaluation.qualifies);
                update_max(&mut summary.max_edge, evaluation.net_edge);
                if let Some(edge) = evaluation.net_edge {
                    summary.net_positive += u64::from(edge > Decimal::ZERO);
                    net_edges.push(edge);
                }
                if let (Some(payout), Some(raw)) =
                    (evaluation.guaranteed_payout_usdc, evaluation.raw_cost_usdc)
                {
                    if payout > Decimal::ZERO {
                        let gross = (payout - raw) / payout;
                        summary.gross_positive += u64::from(gross > Decimal::ZERO);
                        update_max(&mut summary.max_gross_edge, Some(gross));
                        gross_edges.push(gross);
                        if let Some(fee) = evaluation.taker_fee_usdc {
                            let post_fee = (payout - raw - fee) / payout;
                            summary.post_fee_positive += u64::from(post_fee > Decimal::ZERO);
                            update_max(&mut summary.max_post_fee_edge, Some(post_fee));
                            post_fee_edges.push(post_fee);
                        }
                    }
                }
                let semantic = by_semantic
                    .entry(evaluation.semantic_status.clone())
                    .or_default();
                semantic.evaluations += 1;
                semantic.opportunities += u64::from(evaluation.qualifies);
                update_max(&mut semantic.max_edge, evaluation.net_edge);
                if let Some(reason) = evaluation.rejection_reason.clone() {
                    *rejections.entry(reason).or_default() += 1;
                }
                for point in evaluation.size_evaluations {
                    let size_summary = by_size.entry(point.size_shares).or_default();
                    size_summary.observations += 1;
                    if let Some(edge) = point.net_edge {
                        size_summary.complete += 1;
                        size_summary.net_positive += u64::from(edge > Decimal::ZERO);
                        update_max(&mut size_summary.max_edge, Some(edge));
                    }
                    size_summary.opportunities += u64::from(point.qualifies);
                    if let Some(raw) = point.raw_cost_usdc {
                        let payout = point.guaranteed_payout_usdc;
                        if payout > Decimal::ZERO {
                            let gross = (payout - raw) / payout;
                            size_summary.raw_positive += u64::from(gross > Decimal::ZERO);
                            update_max(&mut size_summary.max_gross_edge, Some(gross));
                            if let Some(fee) = point.taker_fee_usdc {
                                let post_fee = (payout - raw - fee) / payout;
                                size_summary.post_fee_positive +=
                                    u64::from(post_fee > Decimal::ZERO);
                                update_max(&mut size_summary.max_post_fee_edge, Some(post_fee));
                            }
                        }
                    }
                    for scenario in point.buffer_evaluations {
                        let buffer_summary =
                            by_buffer.entry(scenario.execution_buffer_pct).or_default();
                        buffer_summary.observations += 1;
                        buffer_summary.opportunities += u64::from(scenario.qualifies);
                        buffer_summary.positive +=
                            u64::from(scenario.net_profit_usdc > Decimal::ZERO);
                        update_max(&mut buffer_summary.max_edge, Some(scenario.net_edge));
                    }
                }
            }
            "conditional_maker_taker_evaluation" => {
                let evaluation: MakerTakerEvaluation = serde_json::from_value(event.data.clone())
                    .with_context(|| {
                    format!("invalid maker/taker evaluation in selected run {selected_run_id}")
                })?;
                maker_taker_evaluations += 1;
                maker_taker_edges.push(evaluation.conditional_edge);
                maker_taker_queue_ahead.push(evaluation.visible_queue_ahead_shares);
            }
            "conditional_maker_taker_candidate" => maker_taker_candidates += 1,
            _ => {}
        }
    }
    let selected_end = selected
        .stopped_at
        .or(selected.latest_at)
        .unwrap_or_else(Utc::now);
    for entered_at in active_near_misses.values() {
        near_miss_duration_seconds += selected_end
            .signed_duration_since(*entered_at)
            .num_seconds()
            .max(0);
    }
    println!("selected_run_summary:");
    println!(
        "  unique_relationships={} logged_evaluations={} unique_economic_snapshots={} discovery_summaries={} near_miss_entries={} near_miss_exposure_seconds={}",
        relationships.len(),
        selected.evaluations_logged,
        unique_economic_snapshots.len(),
        discovery_summaries,
        near_miss_entries,
        near_miss_duration_seconds,
    );
    println!("kind                          evals  gross+  post_fee+  net+  opportunities  gross_max  post_fee_max  net_max");
    for (kind, summary) in by_kind {
        println!(
            "{kind:<29} {:>5} {:>7} {:>10} {:>5} {:>14} {:>10} {:>13} {:>8}",
            summary.evaluations,
            summary.gross_positive,
            summary.post_fee_positive,
            summary.net_positive,
            summary.opportunities,
            format_edge(summary.max_gross_edge),
            format_edge(summary.max_post_fee_edge),
            format_edge(summary.max_edge),
        );
    }
    println!(
        "edge_percentiles (logged selected-size snapshots): gross[p10={} p50={} p90={} max={}] post_fee[p10={} p50={} p90={} max={}] net[p10={} p50={} p90={} max={}]",
        format_edge(percentile(&gross_edges, 10)),
        format_edge(percentile(&gross_edges, 50)),
        format_edge(percentile(&gross_edges, 90)),
        format_edge(gross_edges.iter().copied().max()),
        format_edge(percentile(&post_fee_edges, 10)),
        format_edge(percentile(&post_fee_edges, 50)),
        format_edge(percentile(&post_fee_edges, 90)),
        format_edge(post_fee_edges.iter().copied().max()),
        format_edge(percentile(&net_edges, 10)),
        format_edge(percentile(&net_edges, 50)),
        format_edge(percentile(&net_edges, 90)),
        format_edge(net_edges.iter().copied().max()),
    );
    println!(
        "conditional_maker_taker: evaluations={} candidates={} edge[p50={} p90={} max={}] visible_queue_ahead_shares[p50={} p90={}]; conditional only, no maker fill is inferred",
        maker_taker_evaluations,
        maker_taker_candidates,
        format_edge(percentile(&maker_taker_edges, 50)),
        format_edge(percentile(&maker_taker_edges, 90)),
        format_edge(maker_taker_edges.iter().copied().max()),
        percentile(&maker_taker_queue_ahead, 50).map_or_else(|| "-".into(), |value| value.round_dp(2).to_string()),
        percentile(&maker_taker_queue_ahead, 90).map_or_else(|| "-".into(), |value| value.round_dp(2).to_string()),
    );
    println!("semantic_status:");
    for (status, summary) in by_semantic {
        println!(
            "  {status}: evals={} opportunities={} max_edge={}",
            summary.evaluations,
            summary.opportunities,
            summary
                .max_edge
                .map(|value| format!("{:.2}%", value * Decimal::ONE_HUNDRED))
                .unwrap_or_else(|| "-".into()),
        );
    }
    println!("rejections:");
    for (reason, count) in rejections {
        println!("  {reason}: {count}");
    }
    if !by_size.is_empty() {
        println!("size_curve:");
        println!("  shares  observations  complete  gross+  post_fee+  net+  opportunities  gross_max  post_fee_max  net_max");
        for (size, summary) in by_size {
            println!(
                "  {:>6} {:>13} {:>9} {:>7} {:>10} {:>5} {:>14} {:>10} {:>13} {:>8}",
                size,
                summary.observations,
                summary.complete,
                summary.raw_positive,
                summary.post_fee_positive,
                summary.net_positive,
                summary.opportunities,
                format_edge(summary.max_gross_edge),
                format_edge(summary.max_post_fee_edge),
                format_edge(summary.max_edge),
            );
        }
    }
    println!(
        "book_quality: source_observations={:?} max_book_age_ms={} max_local_snapshot_skew_ms={} max_exchange_snapshot_skew_ms={}",
        source_observations,
        max_book_age_ms.map_or_else(|| "-".into(), |value| value.to_string()),
        max_observed_skew_ms.map_or_else(|| "-".into(), |value| value.to_string()),
        max_exchange_skew_ms.map_or_else(|| "-".into(), |value| value.to_string()),
    );
    if let Some(final_summary) = &selected.final_summary {
        if let Some(fetch) = final_summary.get("book_fetch") {
            let fetch: BookFetchDiagnostics = serde_json::from_value(fetch.clone())?;
            println!(
                "book_fetch: requested={} returned={} websocket={} rest_success={} rest_failure={} rest_latency_avg_ms={} rest_latency_max_ms={}",
                fetch.requested_tokens,
                fetch.returned_books,
                fetch.websocket_hits,
                fetch.rest_successes,
                fetch.rest_failures,
                fetch.rest_latency_average_ms().map_or_else(|| "-".into(), |value| value.to_string()),
                fetch.rest_latency_max_ms,
            );
        } else {
            println!("book_fetch: unavailable in this legacy run; collect a new run with journal version 3 telemetry");
        }
    }
    if !by_buffer.is_empty() {
        println!("execution_buffer_curve:");
        println!("  buffer_pct  observations  positive  opportunities  max_edge");
        for (buffer, summary) in by_buffer {
            println!(
                "  {:>10} {:>13} {:>9} {:>14} {:>9}",
                format!("{:.2}%", buffer * Decimal::ONE_HUNDRED),
                summary.observations,
                summary.positive,
                summary.opportunities,
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
        for (reason, count) in discovery.neg_risk_constraint_failures {
            println!("  neg_risk_constraint_failure {reason}: {count}");
        }
        for (reason, examples) in discovery.neg_risk_rejection_examples {
            for example in examples {
                println!(
                    "  neg_risk_example reason={} event={} deployed={} tradable={} tradable_other={} market_rejections={:?} title={}",
                    reason,
                    example.event_id,
                    example.deployed_markets,
                    example.tradable_markets,
                    example.tradable_other_markets,
                    example.market_rejections,
                    example.event_title,
                );
            }
        }
    }
    write_execution_report(&events, &selected_run_id, path, config)?;
    println!("machine_report: {}", config.report_file.display());
    println!("Note: opportunity counts are synchronized executable-book snapshots, not atomic multi-market fills or realized PnL.");
    Ok(())
}

fn write_execution_report(
    events: &[ReportEvent],
    selected_run_id: &str,
    source_path: &Path,
    config: &CrossMarketProbabilityArbitrageConfig,
) -> Result<()> {
    let selected = events
        .iter()
        .filter(|event| event.run_id == selected_run_id)
        .collect::<Vec<_>>();
    let explicit_pricing_rejected_by_sync = selected
        .iter()
        .filter(|event| event.action == "pricing_opportunity_rejected_by_sync")
        .count();
    let mut synchronized_snapshots = 0_usize;
    let mut raw_pricing_snapshots = explicit_pricing_rejected_by_sync;
    let mut strict_opportunity_relationships = HashSet::<String>::new();
    let mut fallback_episodes = 0_usize;
    let mut last_strict_opportunity = HashMap::<String, DateTime<Utc>>::new();
    let mut episode_relationships = HashMap::<String, String>::new();
    let mut delayed_by_episode = HashMap::<String, Vec<(u64, bool, bool)>>::new();
    let mut delayed_by_ms = BTreeMap::<u64, (u64, u64, u64)>::new();
    let mut worst_single_leg_pnl = None::<Decimal>;
    let mut worst_partial_fill_pnl = None::<Decimal>;
    for event in selected {
        if event.action == "shadow_opportunity" {
            raw_pricing_snapshots += 1;
            if let Ok(evaluation) =
                serde_json::from_value::<RelationshipEvaluation>(event.data.clone())
            {
                if sync_rejection(
                    &evaluation,
                    config.max_book_age_ms,
                    config.max_observed_at_skew_ms,
                    config.max_exchange_timestamp_skew_ms,
                )
                .is_none()
                {
                    synchronized_snapshots += 1;
                    strict_opportunity_relationships.insert(evaluation.relationship_id.clone());
                    let is_new = last_strict_opportunity
                        .get(&evaluation.relationship_id)
                        .is_none_or(|last_seen| {
                            event
                                .timestamp
                                .signed_duration_since(*last_seen)
                                .num_seconds()
                                > i64::try_from(config.opportunity_episode_gap_seconds)
                                    .unwrap_or(i64::MAX)
                        });
                    fallback_episodes += usize::from(is_new);
                    last_strict_opportunity
                        .insert(evaluation.relationship_id.clone(), event.timestamp);
                }
            }
        } else if event.action == "opportunity_episode_started" {
            if let (Some(episode_id), Some(relationship_id)) = (
                event
                    .data
                    .get("episode_id")
                    .and_then(serde_json::Value::as_str),
                event
                    .data
                    .pointer("/evaluation/relationship_id")
                    .and_then(serde_json::Value::as_str),
            ) {
                episode_relationships.insert(episode_id.to_string(), relationship_id.to_string());
            }
        } else if event.action == "delayed_execution_recheck" {
            let Some(episode_id) = event
                .data
                .get("episode_id")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let delay_ms = event
                .data
                .get("delay_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default();
            let executable = event
                .data
                .get("still_executable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let deployment_eligible = event
                .data
                .get("deployment_eligible")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            delayed_by_episode
                .entry(episode_id.to_string())
                .or_default()
                .push((delay_ms, executable, deployment_eligible));
            let checkpoint = delayed_by_ms.entry(delay_ms).or_default();
            checkpoint.0 += 1;
            checkpoint.1 += u64::from(executable);
            checkpoint.2 += u64::from(deployment_eligible);
            update_min_decimal_from_json(
                &mut worst_single_leg_pnl,
                event
                    .data
                    .pointer("/stress/worst_single_leg_emergency_exit_pnl_usdc"),
            );
            update_min_decimal_from_json(
                &mut worst_partial_fill_pnl,
                event
                    .data
                    .pointer("/stress/partial_fill_emergency_exit_pnl_usdc"),
            );
        }
    }
    let episodes_all_rechecks_positive = delayed_by_episode
        .values()
        .filter(|checks| !checks.is_empty() && checks.iter().all(|(_, executable, _)| *executable))
        .count();
    let episodes_deployment_eligible_at_all_rechecks = delayed_by_episode
        .values()
        .filter(|checks| {
            !checks.is_empty()
                && checks
                    .iter()
                    .all(|(_, _, deployment_eligible)| *deployment_eligible)
        })
        .count();
    let unique_opportunity_relationships = strict_opportunity_relationships.len();
    let independent_episodes = if episode_relationships.is_empty() {
        fallback_episodes
    } else {
        episode_relationships.len()
    };
    let delayed_checkpoints = delayed_by_ms
        .into_iter()
        .map(|(delay_ms, (observations, executable, deployment_eligible))| {
            json!({
                "delay_ms": delay_ms,
                "observations": observations,
                "executable": executable,
                "deployment_eligible": deployment_eligible,
                "survival_rate": if observations > 0 { Some(executable as f64 / observations as f64) } else { None },
            })
        })
        .collect::<Vec<_>>();
    let report = json!({
        "generated_at": Utc::now(),
        "source": source_path,
        "selected_run_id": selected_run_id,
        "raw_pricing_opportunity_snapshots": raw_pricing_snapshots,
        "pricing_opportunities_rejected_by_sync": raw_pricing_snapshots.saturating_sub(synchronized_snapshots),
        "explicit_sync_rejection_events": explicit_pricing_rejected_by_sync,
        "synchronized_opportunity_snapshots": synchronized_snapshots,
        "unique_opportunity_relationships": unique_opportunity_relationships,
        "independent_opportunity_episodes": independent_episodes,
        "episodes_with_delayed_rechecks": delayed_by_episode.len(),
        "episodes_executable_at_all_delayed_checkpoints": episodes_all_rechecks_positive,
        "episodes_deployment_eligible_at_all_delayed_checkpoints": episodes_deployment_eligible_at_all_rechecks,
        "delayed_checkpoints": delayed_checkpoints,
        "worst_single_leg_emergency_exit_pnl_usdc": worst_single_leg_pnl,
        "worst_partial_fill_emergency_exit_pnl_usdc": worst_partial_fill_pnl,
        "interpretation": "Shadow observations only. Delayed survival and emergency-exit stress are not realized fills or realized PnL.",
    });
    if let Some(parent) = config
        .report_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config.report_file, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("failed to write {}", config.report_file.display()))?;
    println!(
        "execution_validation: synchronized_snapshots={} unique_relationships={} episodes={} all_delays_positive={} sync_rejected={}",
        synchronized_snapshots,
        unique_opportunity_relationships,
        independent_episodes,
        episodes_all_rechecks_positive,
        raw_pricing_snapshots.saturating_sub(synchronized_snapshots),
    );
    Ok(())
}

fn update_min_decimal_from_json(target: &mut Option<Decimal>, value: Option<&serde_json::Value>) {
    let parsed = value.and_then(|value| match value {
        serde_json::Value::String(value) => Decimal::from_str(value).ok(),
        serde_json::Value::Number(value) => Decimal::from_str(&value.to_string()).ok(),
        _ => None,
    });
    if let Some(parsed) = parsed {
        *target = Some(target.map_or(parsed, |current| current.min(parsed)));
    }
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

fn format_edge(edge: Option<Decimal>) -> String {
    edge.map(|value| format!("{:.2}%", value * Decimal::ONE_HUNDRED))
        .unwrap_or_else(|| "-".into())
}

fn percentile(values: &[Decimal], percentile: usize) -> Option<Decimal> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort();
    let index = percentile
        .min(100)
        .saturating_mul(sorted.len().saturating_sub(1))
        / 100;
    sorted.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::super::model::{BookLevel, BookSnapshot, RelationshipKind};
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
            websocket_books: 0,
            rest_books: 0,
            unknown_source_books: 0,
            max_book_age_ms: None,
            max_observed_at_skew_ms: None,
            max_exchange_timestamp_skew_ms: None,
            max_rest_latency_ms: None,
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
        changed_time.max_book_age_ms = Some(100);
        changed_time.max_observed_at_skew_ms = Some(1_001);
        changed_time.max_exchange_timestamp_skew_ms = Some(100);
        assert_eq!(
            sync_rejection(&changed_time, 2_000, 1_000, 2_000),
            Some("observed_at_skew_exceeds_limit")
        );
        changed_time.max_observed_at_skew_ms = Some(100);
        changed_time.max_exchange_timestamp_skew_ms = Some(2_001);
        assert_eq!(
            sync_rejection(&changed_time, 2_000, 1_000, 2_000),
            Some("exchange_timestamp_skew_exceeds_limit")
        );
        changed_time.max_exchange_timestamp_skew_ms = Some(2_000);
        assert_eq!(sync_rejection(&changed_time, 2_000, 1_000, 2_000), None);
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

    #[test]
    fn book_signature_ignores_timestamps_but_tracks_depth_changes() {
        let mut book = BookSnapshot {
            token_id: "1".into(),
            observed_at: Utc::now(),
            exchange_timestamp_ms: Some(100),
            source: super::super::model::BookSource::Unknown,
            request_latency_ms: None,
            bids: vec![],
            asks: vec![BookLevel {
                price: Decimal::new(45, 2),
                size: Decimal::TEN,
            }],
        };
        let original = book_signature(&book);
        book.observed_at = Utc::now() + chrono::Duration::seconds(1);
        book.exchange_timestamp_ms = Some(101);
        assert_eq!(original, book_signature(&book));
        book.asks[0].size = Decimal::new(11, 0);
        assert_ne!(original, book_signature(&book));
    }
}
