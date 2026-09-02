use super::client::BayesianBookClient;
use super::config::BayesianConsistencyConfig;
use super::evaluator::{evaluate_bundle, BundleEvaluation};
use super::registry::{load_registry, validate_registry};
use super::review::{
    candidate_digest, prepare_review, promote_reviewed, PromotionReport, ReviewFile,
};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct MonitorSnapshot {
    pub observed_at: chrono::DateTime<Utc>,
    pub discovery_scope: String,
    pub candidate_digests: BTreeMap<String, String>,
    pub boundary_digests: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct MonitorState {
    pub version: u32,
    pub last_snapshot: Option<MonitorSnapshot>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MonitorDiff {
    pub new_candidates: Vec<String>,
    pub removed_candidates: Vec<String>,
    pub changed_candidates: Vec<String>,
    pub new_boundaries: Vec<String>,
    pub removed_boundaries: Vec<String>,
    pub changed_boundaries: Vec<String>,
}

fn monitor_diff(previous: Option<&MonitorSnapshot>, current: &MonitorSnapshot) -> MonitorDiff {
    let empty = BTreeMap::new();
    let previous_candidates = previous.map_or(&empty, |snapshot| &snapshot.candidate_digests);
    let previous_boundaries = previous.map_or(&empty, |snapshot| &snapshot.boundary_digests);
    let new_candidates = current
        .candidate_digests
        .keys()
        .filter(|key| !previous_candidates.contains_key(*key))
        .cloned()
        .collect();
    let removed_candidates = previous_candidates
        .keys()
        .filter(|key| !current.candidate_digests.contains_key(*key))
        .cloned()
        .collect();
    let changed_candidates = current
        .candidate_digests
        .iter()
        .filter(|(key, digest)| {
            previous_candidates
                .get(*key)
                .is_some_and(|old| old != *digest)
        })
        .map(|(key, _)| key.clone())
        .collect();
    let new_boundaries = current
        .boundary_digests
        .keys()
        .filter(|key| !previous_boundaries.contains_key(*key))
        .cloned()
        .collect();
    let removed_boundaries = previous_boundaries
        .keys()
        .filter(|key| !current.boundary_digests.contains_key(*key))
        .cloned()
        .collect();
    let changed_boundaries = current
        .boundary_digests
        .iter()
        .filter(|(key, digest)| {
            previous_boundaries
                .get(*key)
                .is_some_and(|old| old != *digest)
        })
        .map(|(key, _)| key.clone())
        .collect();
    MonitorDiff {
        new_candidates,
        removed_candidates,
        changed_candidates,
        new_boundaries,
        removed_boundaries,
        changed_boundaries,
    }
}

struct Journal {
    file: Mutex<File>,
    run_id: String,
}

pub async fn run_discover(config: BayesianConsistencyConfig) -> Result<super::DiscoveryReport> {
    let client = BayesianBookClient::new(config.clone())?;
    let report = client.discover_candidates().await?;
    if let Some(parent) = config
        .discovery_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = config.discovery_file.with_extension("json.tmp");
    {
        let mut file = File::create(&temporary).with_context(|| {
            format!(
                "failed to create Bayesian discovery temporary file {}",
                temporary.display()
            )
        })?;
        serde_json::to_writer_pretty(&mut file, &report)?;
        writeln!(file)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, &config.discovery_file).with_context(|| {
        format!(
            "failed to publish Bayesian discovery file {}",
            config.discovery_file.display()
        )
    })?;
    println!(
        "Bayesian candidate discovery: events={} tradable_markets={} candidates={} seed_errors={} output={}",
        report.events_seen,
        report.tradable_markets_seen,
        report.candidates.len(),
        report.seed_errors.len(),
        config.discovery_file.display()
    );
    for error in &report.seed_errors {
        eprintln!("  rejected seed: {error}");
    }
    for candidate in report.candidates.iter().take(20) {
        println!(
            "  pending candidate={} source={} score={:.3} parent={} children={} event={}",
            candidate.candidate_id,
            candidate.discovery_source,
            candidate.lexical_score,
            candidate.parent.question,
            candidate.children.len(),
            candidate.parent.event_title
        );
    }
    Ok(report)
}

pub async fn run_discover_history(
    config: BayesianConsistencyConfig,
) -> Result<super::DiscoveryReport> {
    let client = BayesianBookClient::new(config.clone())?;
    let report = client.discover_historical_candidates().await?;
    write_discovery_report(&config.historical_discovery_file, &report)?;
    println!(
        "Bayesian historical discovery: events={} binary_markets={} candidates={} output={}",
        report.events_seen,
        report.tradable_markets_seen,
        report.candidates.len(),
        config.historical_discovery_file.display()
    );
    for candidate in report.candidates.iter().take(20) {
        println!(
            "  research-only candidate={} score={:.3} parent={} children={}",
            candidate.candidate_id,
            candidate.lexical_score,
            candidate.parent.question,
            candidate.children.len()
        );
    }
    Ok(report)
}

pub async fn run_boundary_analysis(
    config: BayesianConsistencyConfig,
) -> Result<super::BoundaryReport> {
    let report = BayesianBookClient::new(config.clone())?
        .analyze_boundary_candidates()
        .await?;
    write_json_report(&config.boundary_report_file, &report)?;
    println!(
        "Bayesian boundary analysis: events={} numeric_partitions={} diagnostics={} numerically_representable={} output={}",
        report.events_seen,
        report.numeric_partition_events,
        report.diagnostics.len(),
        report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.numerically_representable)
            .count(),
        config.boundary_report_file.display()
    );
    for diagnostic in report.diagnostics.iter().take(20) {
        println!(
            "  parent={} partition={} boundary={} exact={} blockers={:?}",
            diagnostic.parent.question,
            diagnostic.partition_event_title,
            diagnostic
                .inferred_parent_boundary
                .as_deref()
                .unwrap_or("unknown"),
            diagnostic.numerically_representable,
            diagnostic.numeric_blockers
        );
    }
    Ok(report)
}

pub async fn run_monitor(config: BayesianConsistencyConfig) -> Result<()> {
    let client = BayesianBookClient::new(config.clone())?;
    if let Some(parent) = config
        .monitor_results_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut state = if config.monitor_state_file.exists() {
        let bytes = std::fs::read(&config.monitor_state_file).with_context(|| {
            format!(
                "failed to read monitor state {}",
                config.monitor_state_file.display()
            )
        })?;
        serde_json::from_slice::<MonitorState>(&bytes).with_context(|| {
            format!(
                "invalid monitor state {}",
                config.monitor_state_file.display()
            )
        })?
    } else {
        MonitorState {
            version: 1,
            last_snapshot: None,
        }
    };
    if state.version != 1 {
        bail!("unsupported monitor state version {}", state.version);
    }
    let mut journal = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.monitor_results_file)
        .with_context(|| {
            format!(
                "failed to open monitor journal {}",
                config.monitor_results_file.display()
            )
        })?;
    monitor_record(
        &mut journal,
        "monitor_started",
        &json!({
            "interval_seconds": config.monitor_interval_seconds,
            "duration_seconds": config.monitor_duration_seconds,
            "read_only": true,
        }),
    )?;
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let started = std::time::Instant::now();
    let mut cycles = 0_u64;
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed)
        && (config.monitor_duration_seconds == 0
            || started.elapsed().as_secs() < config.monitor_duration_seconds
            || cycles == 0)
    {
        cycles = cycles.saturating_add(1);
        let discovery = match client.discover_candidates().await {
            Ok(report) => report,
            Err(error) => {
                monitor_record(
                    &mut journal,
                    "monitor_error",
                    &json!({
                        "cycle": cycles,
                        "stage": "discover",
                        "error": format!("{error:#}"),
                    }),
                )?;
                wait_monitor_cycle(&shutdown, monitor_wait_seconds(&started, &config)).await;
                if monitor_finished(&started, config.monitor_duration_seconds, cycles) {
                    break;
                }
                continue;
            }
        };
        let boundary = match client.analyze_boundary_candidates().await {
            Ok(report) => report,
            Err(error) => {
                monitor_record(
                    &mut journal,
                    "monitor_error",
                    &json!({
                        "cycle": cycles,
                        "stage": "analyze_boundaries",
                        "error": format!("{error:#}"),
                    }),
                )?;
                wait_monitor_cycle(&shutdown, monitor_wait_seconds(&started, &config)).await;
                if monitor_finished(&started, config.monitor_duration_seconds, cycles) {
                    break;
                }
                continue;
            }
        };
        write_discovery_report(&config.discovery_file, &discovery)?;
        write_json_report(&config.boundary_report_file, &boundary)?;
        let current = MonitorSnapshot {
            observed_at: Utc::now(),
            discovery_scope: discovery.scope,
            candidate_digests: discovery
                .candidates
                .iter()
                .map(|candidate| Ok((candidate.candidate_id.clone(), candidate_digest(candidate)?)))
                .collect::<Result<BTreeMap<_, _>>>()?,
            boundary_digests: boundary
                .diagnostics
                .iter()
                .map(|diagnostic| {
                    Ok((
                        diagnostic.diagnostic_id.clone(),
                        serde_json::to_string(diagnostic)?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        };
        let diff = monitor_diff(state.last_snapshot.as_ref(), &current);
        monitor_record(
            &mut journal,
            "monitor_snapshot",
            &json!({
                "cycle": cycles,
                "snapshot": &current,
                "diff": &diff,
                "events_seen": discovery.events_seen,
                "tradable_markets_seen": discovery.tradable_markets_seen,
                "boundary_events": boundary.numeric_partition_events,
            }),
        )?;
        if config.monitor_alert_on_new_candidate && !diff.new_candidates.is_empty() {
            eprintln!(
                "BAYESIAN MONITOR ALERT new candidates: {:?}",
                diff.new_candidates
            );
        }
        if config.monitor_alert_on_rule_drift
            && (!diff.changed_candidates.is_empty() || !diff.changed_boundaries.is_empty())
        {
            eprintln!(
                "BAYESIAN MONITOR ALERT drift: candidates={:?} boundaries={:?}",
                diff.changed_candidates, diff.changed_boundaries
            );
        }
        state.last_snapshot = Some(current);
        write_json_report(&config.monitor_state_file, &state)?;
        println!(
            "Bayesian monitor cycle={} candidates={} boundaries={} new={} changed={}",
            cycles,
            state
                .last_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.candidate_digests.len()),
            state
                .last_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.boundary_digests.len()),
            diff.new_candidates.len(),
            diff.changed_candidates.len() + diff.changed_boundaries.len()
        );
        if monitor_finished(&started, config.monitor_duration_seconds, cycles) {
            break;
        }
        wait_monitor_cycle(&shutdown, monitor_wait_seconds(&started, &config)).await;
    }
    monitor_record(
        &mut journal,
        "monitor_stopped",
        &json!({
            "cycles": cycles,
            "duration_seconds": started.elapsed().as_secs(),
        }),
    )?;
    Ok(())
}

fn monitor_finished(started: &std::time::Instant, duration_seconds: u64, cycles: u64) -> bool {
    duration_seconds > 0 && cycles > 0 && started.elapsed().as_secs() >= duration_seconds
}

fn monitor_record(file: &mut File, action: &str, data: &impl Serialize) -> Result<()> {
    serde_json::to_writer(
        &mut *file,
        &json!({
            "version": 1,
            "timestamp": Utc::now(),
            "action": action,
            "data": data,
        }),
    )?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}

async fn wait_monitor_cycle(
    shutdown: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    seconds: u64,
) {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(seconds)) => {},
        _ = wait_for_shutdown(shutdown.clone()) => {},
    }
}

fn monitor_wait_seconds(started: &std::time::Instant, config: &BayesianConsistencyConfig) -> u64 {
    if config.monitor_duration_seconds == 0 {
        config.monitor_interval_seconds
    } else {
        config.monitor_interval_seconds.min(
            config
                .monitor_duration_seconds
                .saturating_sub(started.elapsed().as_secs()),
        )
    }
}

fn write_json_report(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = File::create(&temporary)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        writeln!(file)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn write_discovery_report(path: &Path, report: &super::DiscoveryReport) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = File::create(&temporary).with_context(|| {
            format!(
                "failed to create Bayesian discovery temporary file {}",
                temporary.display()
            )
        })?;
        serde_json::to_writer_pretty(&mut file, report)?;
        writeln!(file)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path).with_context(|| {
        format!(
            "failed to publish Bayesian discovery file {}",
            path.display()
        )
    })?;
    Ok(())
}

pub fn run_prepare_review(config: &BayesianConsistencyConfig) -> Result<ReviewFile> {
    let review = prepare_review(&config.discovery_file, &config.review_file)?;
    println!(
        "Bayesian review template: candidates={} output={}",
        review.reviews.len(),
        config.review_file.display()
    );
    Ok(review)
}

pub fn run_promote(config: &BayesianConsistencyConfig) -> Result<PromotionReport> {
    let report = promote_reviewed(
        &config.discovery_file,
        &config.review_file,
        &config.registry_file,
        config.max_children,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(report)
}

pub async fn run_audit(config: &BayesianConsistencyConfig) -> Result<super::RegistryAuditReport> {
    let registry = load_registry(&config.registry_file)?;
    let (validation, relationships) = validate_registry(&registry, config.max_children);
    if !validation.invalid.is_empty() {
        bail!("relationship registry contains invalid approved entries");
    }
    if relationships.is_empty() {
        bail!("no manually approved relationships to audit");
    }
    let report = BayesianBookClient::new(config.clone())?
        .audit_relationships(&relationships)
        .await;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if report.failed > 0 {
        bail!(
            "live registry audit failed for {} contract(s)",
            report.failed
        );
    }
    Ok(report)
}

impl Journal {
    fn open(path: &Path, run_id: String) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            file: Mutex::new(OpenOptions::new().create(true).append(true).open(path)?),
            run_id,
        })
    }

    fn record(&self, action: &str, data: &impl Serialize) -> Result<()> {
        let value = json!({
            "version": 1,
            "run_id": self.run_id,
            "action": action,
            "timestamp": Utc::now(),
            "data": data,
        });
        let mut file = self.file.lock().expect("Bayesian journal lock poisoned");
        serde_json::to_writer(&mut *file, &value)?;
        writeln!(file)?;
        file.flush()?;
        Ok(())
    }
}

pub async fn run_scan(config: BayesianConsistencyConfig) -> Result<Vec<BundleEvaluation>> {
    let registry = load_registry(&config.registry_file)?;
    let (validation, relationships) = validate_registry(&registry, config.max_children);
    if !validation.invalid.is_empty() {
        bail!("relationship registry contains invalid approved entries");
    }
    if relationships.is_empty() {
        bail!(
            "no manually approved exact-partition relationships; validate and populate {} before scan",
            config.registry_file.display()
        );
    }
    let client = BayesianBookClient::new(config.clone())?;
    let audit = client.audit_relationships(&relationships).await;
    if audit.failed > 0 {
        bail!(
            "live registry audit failed for {} contract(s)",
            audit.failed
        );
    }
    let journal = Journal::open(
        &config.results_file,
        format!("scan-{}", uuid::Uuid::new_v4()),
    )?;
    journal.record("scan_started", &validation)?;
    journal.record("live_registry_audit", &audit)?;
    let books = client.fetch_books(&relationships).await;
    let now = Utc::now();
    let mut evaluations = Vec::new();
    for relationship in &relationships {
        journal.record("relationship_validated", relationship)?;
        for bundle in &relationship.bundles {
            let evaluation = evaluate_bundle(bundle, &books, &config, now, Decimal::ONE);
            journal.record("bundle_evaluation", &evaluation)?;
            if evaluation.qualifies {
                journal.record("shadow_opportunity", &evaluation)?;
            }
            evaluations.push(evaluation);
        }
    }
    journal
        .record(
            "scan_completed",
            &json!({
                "relationships": relationships.len(),
                "books": books.len(),
                "evaluations": evaluations.len(),
                "opportunities": evaluations.iter().filter(|item| item.qualifies).count(),
            }),
        )
        .context("failed to persist scan completion")?;
    for evaluation in &evaluations {
        println!(
            "relationship={} direction={:?} qualifies={} edge={} reason={}",
            evaluation.relationship_id,
            evaluation.direction,
            evaluation.qualifies,
            evaluation
                .net_edge
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".into()),
            evaluation.rejection_reason.as_deref().unwrap_or("none")
        );
    }
    Ok(evaluations)
}

pub async fn run_shadow(config: BayesianConsistencyConfig) -> Result<()> {
    let registry = load_registry(&config.registry_file)?;
    let (validation, relationships) = validate_registry(&registry, config.max_children);
    if !validation.invalid.is_empty() {
        bail!("relationship registry contains invalid approved entries");
    }
    if relationships.is_empty() {
        bail!(
            "no manually approved exact-partition relationships; populate {} before shadow",
            config.registry_file.display()
        );
    }

    let client = BayesianBookClient::new(config.clone())?;
    let audit = client.audit_relationships(&relationships).await;
    if audit.failed > 0 {
        bail!(
            "live registry audit failed for {} contract(s)",
            audit.failed
        );
    }
    let run_id = uuid::Uuid::new_v4().to_string();
    let journal = Journal::open(&config.results_file, run_id.clone())?;
    journal.record(
        "run_started",
        &json!({
            "mode": "read_only_shadow",
            "live_eligible": false,
            "relationships": relationships.len(),
            "registry": config.registry_file,
            "poll_seconds": config.poll_seconds,
            "duration_seconds": config.duration_seconds,
            "min_net_edge": config.min_net_edge,
            "execution_buffer_pct": config.execution_buffer_pct,
            "shadow_delays_ms": config.shadow_delay_ms,
            "liquidity_haircuts": config.shadow_liquidity_haircuts,
        }),
    )?;
    journal.record("registry_validation", &validation)?;
    journal.record("live_registry_audit", &audit)?;
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let started = std::time::Instant::now();
    let mut cycle = 0_u64;
    let mut active_episodes = HashSet::new();
    let mut opportunities = 0_u64;
    let mut delayed_complete = 0_u64;
    let mut delayed_rejected = 0_u64;

    while !shutdown.load(std::sync::atomic::Ordering::Relaxed)
        && (config.duration_seconds == 0 || started.elapsed().as_secs() < config.duration_seconds)
    {
        cycle = cycle.saturating_add(1);
        let books = client.fetch_books(&relationships).await;
        let now = Utc::now();
        let mut cycle_opportunities = 0_u64;
        for relationship in &relationships {
            for bundle in &relationship.bundles {
                let evaluation = evaluate_bundle(bundle, &books, &config, now, Decimal::ONE);
                journal.record("bundle_evaluation", &evaluation)?;
                let episode_key = format!("{}:{:?}", bundle.relationship_id, bundle.direction);
                if evaluation.qualifies {
                    cycle_opportunities = cycle_opportunities.saturating_add(1);
                    if active_episodes.insert(episode_key.clone()) {
                        opportunities = opportunities.saturating_add(1);
                        let opportunity_id = uuid::Uuid::new_v4().to_string();
                        journal.record(
                            "shadow_opportunity_entered",
                            &json!({
                                "opportunity_id": opportunity_id,
                                "evaluation": evaluation,
                            }),
                        )?;
                        let (complete, rejected) = run_delay_scenarios(
                            &client,
                            &config,
                            bundle,
                            &evaluation,
                            &opportunity_id,
                            &journal,
                        )
                        .await?;
                        delayed_complete = delayed_complete.saturating_add(complete);
                        delayed_rejected = delayed_rejected.saturating_add(rejected);
                    }
                } else if active_episodes.remove(&episode_key) {
                    journal.record(
                        "shadow_opportunity_exited",
                        &json!({
                            "episode_key": episode_key,
                            "last_evaluation": evaluation,
                        }),
                    )?;
                }
            }
        }
        journal.record(
            "cycle_summary",
            &json!({
                "cycle": cycle,
                "books": books.len(),
                "relationships": relationships.len(),
                "active_opportunities": cycle_opportunities,
                "independent_opportunities": opportunities,
                "delayed_complete": delayed_complete,
                "delayed_rejected": delayed_rejected,
            }),
        )?;
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(config.poll_seconds)) => {},
            _ = wait_for_shutdown(shutdown.clone()) => {},
        }
    }
    journal.record(
        "run_stopped",
        &json!({
            "normal": true,
            "cycles": cycle,
            "independent_opportunities": opportunities,
            "delayed_complete": delayed_complete,
            "delayed_rejected": delayed_rejected,
            "duration_seconds": started.elapsed().as_secs(),
        }),
    )?;
    println!(
        "Bayesian consistency shadow stopped: cycles={cycle} opportunities={opportunities} delayed_complete={delayed_complete} delayed_rejected={delayed_rejected} run_id={run_id}"
    );
    Ok(())
}

async fn wait_for_shutdown(shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn run_delay_scenarios(
    client: &BayesianBookClient,
    config: &BayesianConsistencyConfig,
    bundle: &super::model::HedgeBundle,
    signal: &BundleEvaluation,
    opportunity_id: &str,
    journal: &Journal,
) -> Result<(u64, u64)> {
    let Some(signal_size) = signal.size_shares else {
        return Ok((0, 0));
    };
    let mut scenario_config = config.clone();
    scenario_config.size_ladder_shares = vec![signal_size];
    let mut delays = config.shadow_delay_ms.clone();
    delays.sort_unstable();
    delays.dedup();
    let mut previous_delay = 0_u64;
    let mut complete = 0_u64;
    let mut rejected = 0_u64;
    for delay_ms in delays {
        tokio::time::sleep(std::time::Duration::from_millis(
            delay_ms.saturating_sub(previous_delay),
        ))
        .await;
        previous_delay = delay_ms;
        let books = client.fetch_bundle_sequential(bundle).await;
        for haircut in &config.shadow_liquidity_haircuts {
            let evaluation =
                evaluate_bundle(bundle, &books, &scenario_config, Utc::now(), *haircut);
            if evaluation.qualifies {
                complete = complete.saturating_add(1);
            } else {
                rejected = rejected.saturating_add(1);
            }
            journal.record(
                "delayed_shadow_evaluation",
                &json!({
                    "opportunity_id": opportunity_id,
                    "delay_ms": delay_ms,
                    "liquidity_haircut": haircut,
                    "execution_model": "sequential_rest_snapshot_proxy",
                    "signal_net_edge": signal.net_edge,
                    "signal_size_shares": signal_size,
                    "evaluation": evaluation,
                }),
            )?;
        }
    }
    Ok((complete, rejected))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BayesianReport {
    pub files: usize,
    pub records: u64,
    pub runs: usize,
    pub actions: BTreeMap<String, u64>,
    pub independent_opportunities: u64,
    pub delayed_complete: u64,
    pub delayed_rejected: u64,
    pub best_delayed_net_edge: Option<Decimal>,
    pub rejection_reasons: BTreeMap<String, u64>,
}

pub fn run_report(path: &Path) -> Result<BayesianReport> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Bayesian result {}", path.display()))?;
    let mut report = BayesianReport {
        files: 1,
        ..BayesianReport::default()
    };
    let mut runs = HashSet::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)?;
        report.records = report.records.saturating_add(1);
        if let Some(run_id) = value.get("run_id").and_then(|value| value.as_str()) {
            runs.insert(run_id.to_string());
        }
        let action = value
            .get("action")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        *report.actions.entry(action.to_string()).or_default() += 1;
        if action == "shadow_opportunity_entered" {
            report.independent_opportunities = report.independent_opportunities.saturating_add(1);
        }
        if action == "delayed_shadow_evaluation" {
            let evaluation = &value["data"]["evaluation"];
            if evaluation
                .get("qualifies")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                report.delayed_complete = report.delayed_complete.saturating_add(1);
            } else {
                report.delayed_rejected = report.delayed_rejected.saturating_add(1);
                if let Some(reason) = evaluation
                    .get("rejection_reason")
                    .and_then(|value| value.as_str())
                {
                    *report
                        .rejection_reasons
                        .entry(reason.to_string())
                        .or_default() += 1;
                }
            }
            if let Some(edge) = evaluation.get("net_edge").and_then(json_decimal) {
                report.best_delayed_net_edge = Some(
                    report
                        .best_delayed_net_edge
                        .map_or(edge, |current| current.max(edge)),
                );
            }
        }
    }
    report.runs = runs.len();
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(report)
}

fn json_decimal(value: &serde_json::Value) -> Option<Decimal> {
    use std::str::FromStr;
    value
        .as_str()
        .and_then(|value| Decimal::from_str(value).ok())
        .or_else(|| Decimal::from_str(&value.to_string()).ok())
}

#[cfg(test)]
mod monitor_tests {
    use super::*;

    fn snapshot(candidate: (&str, &str), boundary: (&str, &str)) -> MonitorSnapshot {
        MonitorSnapshot {
            observed_at: Utc::now(),
            discovery_scope: "active_tradable".into(),
            candidate_digests: BTreeMap::from([(candidate.0.into(), candidate.1.into())]),
            boundary_digests: BTreeMap::from([(boundary.0.into(), boundary.1.into())]),
        }
    }

    #[test]
    fn monitor_diff_detects_new_removed_and_changed_items() {
        let previous = snapshot(("candidate-old", "a"), ("boundary-old", "a"));
        let current = snapshot(("candidate-new", "b"), ("boundary-old", "changed"));
        let diff = monitor_diff(Some(&previous), &current);
        assert_eq!(diff.new_candidates, vec!["candidate-new"]);
        assert_eq!(diff.removed_candidates, vec!["candidate-old"]);
        assert!(diff.changed_candidates.is_empty());
        assert!(diff.new_boundaries.is_empty());
        assert!(diff.removed_boundaries.is_empty());
        assert_eq!(diff.changed_boundaries, vec!["boundary-old"]);
    }

    #[test]
    fn first_monitor_snapshot_reports_new_items() {
        let current = snapshot(("candidate", "a"), ("boundary", "b"));
        let diff = monitor_diff(None, &current);
        assert_eq!(diff.new_candidates, vec!["candidate"]);
        assert_eq!(diff.new_boundaries, vec!["boundary"]);
    }
}
