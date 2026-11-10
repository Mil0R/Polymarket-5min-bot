//! Read-only incremental index. Money always comes from the study journal, never metrics.
use crate::late_resolution_winner_trading::{WinnerEvaluation, WinnerShadowExecution};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal as D;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const ARMS: [&str; 2] = ["control_096_098", "eth_cap_097"];
pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub attempts: i64,
    pub fills: i64,
    pub wins: i64,
    pub losses: i64,
    pub no_fills: i64,
    pub errors: i64,
    pub pending: i64,
    pub unknown: i64,
    pub exposure: D,
    pub reserved: D,
    pub pnl: D,
    pub cost: D,
}
impl Stats {
    fn accumulate(&mut self, other: &Self, sign: i64) {
        self.attempts += sign * other.attempts;
        self.fills += sign * other.fills;
        self.wins += sign * other.wins;
        self.losses += sign * other.losses;
        self.no_fills += sign * other.no_fills;
        self.errors += sign * other.errors;
        self.pending += sign * other.pending;
        self.unknown += sign * other.unknown;
        let s = D::from(sign);
        self.exposure += s * other.exposure;
        self.reserved += s * other.reserved;
        self.pnl += s * other.pnl;
        self.cost += s * other.cost;
    }
    pub fn sum(rows: &[Self]) -> Self {
        let mut s = Self::default();
        for r in rows {
            s.accumulate(r, 1);
        }
        s
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct Decision {
    pub arm: String,
    pub reason: String,
    pub confirmations: u32,
}
#[derive(Clone, Debug)]
pub struct Observation {
    pub slug: String,
    pub at: DateTime<Utc>,
    pub decisions: Vec<Decision>,
}
#[derive(Deserialize)]
struct Attempt {
    attempt_id: String,
    arm: String,
    evaluation: WinnerEvaluation,
    requested: D,
    reserved_cost: D,
    #[serde(default)]
    parameter_revision: Option<u64>,
    #[serde(default)]
    budget_limit: Option<D>,
    #[serde(default)]
    entry_epoch: Option<u64>,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Event {
    OperatorFunded {
        command: super::control::Command,
        amount: D,
        revision: u64,
        funded_at: DateTime<Utc>,
    },
    OperatorChanged {
        command: Option<super::control::Command>,
        settings: super::control::Settings,
    },
    Registered {
        version: u32,
        study_id: String,
        registered_at: DateTime<Utc>,
        forward_window_start_ms: i64,
        configuration: serde_json::Value,
    },
    RunStarted {
        run_id: String,
        started_at: DateTime<Utc>,
    },
    Observed {
        evaluation: Box<WinnerEvaluation>,
        decisions: Vec<Decision>,
    },
    Attempted {
        attempt: Box<Attempt>,
    },
    Executed {
        attempt_id: String,
        execution: Box<WinnerShadowExecution>,
    },
    Settled {
        attempt_id: String,
        won: bool,
        payout: D,
        pnl: D,
        resolved_at: DateTime<Utc>,
    },
}
#[derive(Clone)]
struct Entry {
    id: String,
    arm: usize,
    asset: usize,
    day: NaiveDate,
    at: DateTime<Utc>,
    end: DateTime<Utc>,
    slug: String,
    evaluation_id: String,
    run_id: String,
    ask: D,
    requested: D,
    reservation: D,
    entry_epoch: Option<u64>,
    filled: D,
    cost: D,
    pnl: Option<D>,
    won: Option<bool>,
    outcome: Option<String>,
    attempt_offset: u64,
    execution_offset: Option<u64>,
    settlement_offset: Option<u64>,
}
impl Entry {
    fn stats(&self) -> Stats {
        let filled = self.filled > D::ZERO;
        Stats {
            attempts: 1,
            fills: i64::from(filled),
            wins: i64::from(filled && self.won == Some(true)),
            losses: i64::from(filled && self.won == Some(false)),
            no_fills: i64::from(self.outcome.as_deref() == Some("no_fill")),
            errors: i64::from(self.outcome.as_deref() == Some("error")),
            pending: i64::from(filled && self.pnl.is_none()),
            unknown: i64::from(self.outcome.is_none()),
            exposure: if filled && self.pnl.is_none() {
                self.cost
            } else {
                D::ZERO
            },
            reserved: if self.outcome.is_none() {
                self.reservation
            } else {
                D::ZERO
            },
            pnl: self.pnl.unwrap_or(D::ZERO),
            cost: if filled && self.pnl.is_some() {
                self.cost
            } else {
                D::ZERO
            },
        }
    }
}
#[derive(Clone, Debug)]
pub struct Trade {
    pub id: String,
    pub parameter_revision: Option<u64>,
    pub budget_limit: Option<D>,
    pub asset: String,
    pub slug: String,
    pub at: DateTime<Utc>,
    pub direction: bool,
    pub ask: D,
    pub limit: D,
    pub average: Option<D>,
    pub filled: D,
    pub cost: D,
    pub reserved: D,
    pub execution_error: Option<String>,
    pub fee: D,
    pub pnl: Option<D>,
    pub outcome: String,
    pub settled_at: Option<DateTime<Utc>>,
    pub evaluation: WinnerEvaluation,
    pub request_ms: Option<u64>,
    pub total_ms: Option<u64>,
}
#[derive(Clone, Debug)]
pub struct LedgerView {
    pub capital: D,
    pub deposits: D,
    pub budget: D,
    pub ready: bool,
    pub error: Option<String>,
    pub loading: bool,
    pub partial: bool,
    pub start_ms: i64,
    pub as_of_ms: i64,
    pub run_count: usize,
    pub daily: [Stats; 3],
    pub total: [Stats; 3],
    pub rows: Vec<Trade>,
    pub row_count: usize,
    pub page: usize,
    pub oldest_pending_ms: Option<i64>,
    pub observations: [Option<Observation>; 2],
}
impl Default for LedgerView {
    fn default() -> Self {
        Self {
            capital: D::from(100),
            deposits: D::ZERO,
            budget: D::from(5),
            ready: false,
            error: None,
            loading: true,
            partial: false,
            start_ms: 0,
            as_of_ms: 0,
            run_count: 0,
            daily: Default::default(),
            total: Default::default(),
            rows: vec![],
            row_count: 0,
            page: 0,
            oldest_pending_ms: None,
            observations: Default::default(),
        }
    }
}

pub struct Ledger {
    path: PathBuf,
    reader: BufReader<File>,
    identity: (u64, u64),
    offset: u64,
    failed: Option<String>,
    registered: bool,
    operator: Option<super::control::Settings>,
    deposits: D,
    commands: HashSet<String>,
    start_ms: i64,
    as_of_ms: i64,
    runs: HashSet<String>,
    entries: Vec<Entry>,
    ids: HashMap<String, usize>,
    markets: HashSet<(usize, String)>,
    by_day: BTreeMap<(NaiveDate, usize), Vec<usize>>,
    totals: HashMap<(usize, usize), Stats>,
    daily: HashMap<(NaiveDate, usize, usize), Stats>,
    observations: [Option<Observation>; 2],
    pub partial: bool,
    pub loading: bool,
}
fn identity(m: &Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        (0, 0)
    }
}
fn arm(s: &str) -> Result<usize> {
    ARMS.iter()
        .position(|a| *a == s)
        .context("不支持的研究轨道")
}
fn asset(s: &str) -> Result<usize> {
    match s.to_ascii_lowercase().as_str() {
        "btc" => Ok(0),
        "eth" => Ok(1),
        _ => bail!("不支持的资产"),
    }
}
impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("无法读取 {}", path.display()))?;
        let id = identity(&file.metadata()?);
        Ok(Self {
            path: path.into(),
            reader: BufReader::new(file),
            identity: id,
            offset: 0,
            failed: None,
            registered: false,
            operator: None,
            deposits: D::ZERO,
            commands: HashSet::new(),
            start_ms: 0,
            as_of_ms: 0,
            runs: HashSet::new(),
            entries: vec![],
            ids: HashMap::new(),
            markets: HashSet::new(),
            by_day: BTreeMap::new(),
            totals: HashMap::new(),
            daily: HashMap::new(),
            observations: Default::default(),
            partial: false,
            loading: true,
        })
    }
    /// Bounded work per poll; never write, lock exclusively, or repair the source.
    pub fn revision(&self) -> (u64, bool, bool, (u64, u64)) {
        (self.offset, self.loading, self.partial, self.identity)
    }
    pub fn refresh(&mut self) -> Result<()> {
        let m = std::fs::metadata(&self.path)?;
        if identity(&m) != self.identity || m.len() < self.offset {
            *self = Self::open(&self.path)?;
        }
        if let Some(error) = &self.failed {
            bail!("账本验证失败（停止累计）：{error}");
        }
        self.reader.seek(SeekFrom::Start(self.offset))?;
        let mut consumed = 0;
        self.partial = false;
        loop {
            let at = self.offset;
            let mut line = Vec::new();
            // A corrupt unterminated line cannot allocate without limit.
            {
                let mut limited = (&mut self.reader).take(2 * 1024 * 1024 + 1);
                limited.read_until(b'\n', &mut line)?;
            }
            if line.len() > 2 * 1024 * 1024 {
                self.failed = Some("单条记录超过2MiB".into());
                bail!("单条记录超过2MiB");
            }
            if line.is_empty() {
                self.loading = false;
                break;
            }
            if !line.ends_with(b"\n") {
                self.partial = true;
                self.loading = false;
                break;
            }
            let result = serde_json::from_slice::<Event>(&line)
                .map_err(anyhow::Error::from)
                .and_then(|event| self.apply(event, at));
            if let Err(e) = result {
                let error = format!("byte {at}: {e:#}");
                self.failed = Some(error.clone());
                bail!("{error}");
            }
            self.offset += line.len() as u64;
            consumed += line.len();
            if consumed >= 4 * 1024 * 1024 {
                self.loading = self.offset < m.len();
                break;
            }
        }
        Ok(())
    }
    fn adjust(&mut self, e: &Entry, sign: i64) {
        let stat = e.stats();
        self.totals
            .entry((e.arm, e.asset))
            .or_default()
            .accumulate(&stat, sign);
        self.daily
            .entry((e.day, e.arm, e.asset))
            .or_default()
            .accumulate(&stat, sign);
    }
    fn apply(&mut self, event: Event, offset: u64) -> Result<()> {
        match event {
            Event::OperatorFunded {
                command,
                amount,
                revision,
                funded_at,
            } => {
                let old = self.operator.as_ref().context("funding in frozen ledger")?;
                super::control::validate_funding(old, &command, amount, revision)?;
                ensure!(
                    self.runs.contains(&command.run_id) && !self.commands.contains(&command.id),
                    "unregistered/duplicate funding command"
                );
                self.deposits = self
                    .deposits
                    .checked_add(amount)
                    .context("PAPER deposits overflow")?;
                D::from(100)
                    .checked_add(self.deposits)
                    .context("PAPER capital overflow")?;
                self.commands.insert(command.id);
                self.operator.as_mut().expect("checked operator").revision = revision;
                self.as_of_ms = self.as_of_ms.max(funded_at.timestamp_millis());
            }
            Event::OperatorChanged { command, settings } => {
                let old = self
                    .operator
                    .as_ref()
                    .context("operator event in frozen ledger")?;
                ensure!(
                    settings.revision == old.revision + 1,
                    "operator revision discontinuity"
                );
                super::control::valid_budget(settings.budget)?;
                if let Some(value) = settings.pending_budget {
                    super::control::valid_budget(value)?;
                }
                if let Some(c) = command {
                    ensure!(
                        self.runs.contains(&c.run_id)
                            && c.expected_revision == old.revision
                            && !matches!(c.action, super::control::Action::Deposit { .. })
                            && self.commands.insert(c.id),
                        "invalid/duplicate control command"
                    );
                }
                self.operator = Some(settings);
            }
            Event::Registered {
                version,
                study_id,
                registered_at,
                forward_window_start_ms,
                configuration: c,
            } => {
                ensure!(!self.registered && offset == 0, "注册重复或不在文件首行");
                ensure!(
                    (version == 1 && study_id == "eth-ask-cap-097-v1")
                        || (version == 2 && study_id == super::control::STUDY),
                    "只支持price-cap v1研究账本"
                );
                ensure!(
                    c["paper_only"] == true && c["automatic_promotion"] == false,
                    "不是只读PAPER研究账本"
                );
                if version == 2 {
                    self.operator = Some(super::control::Settings::default());
                }
                ensure!(
                    c["initial_cash_per_arm"] == "100"
                        && c["max_order_usdc"] == if version == 2 { "100" } else { "5" },
                    "不支持的资金参数"
                );
                ensure!(
                    (self.operator.is_some() || c["arms"][ARMS[0]]["eth_max_ask"] == "0.98")
                        && c["arms"][ARMS[1]]["eth_max_ask"] == "0.97",
                    "轨道定义不匹配"
                );
                self.registered = true;
                self.start_ms = forward_window_start_ms;
                self.as_of_ms = registered_at.timestamp_millis();
            }
            Event::RunStarted { run_id, started_at } => {
                ensure!(self.registered, "缺少注册记录");
                ensure!(self.runs.insert(run_id), "重复run ID");
                self.as_of_ms = self.as_of_ms.max(started_at.timestamp_millis());
            }
            Event::Observed {
                evaluation: e,
                decisions,
            } => {
                ensure!(
                    self.registered && self.runs.contains(&e.run_id),
                    "未注册run的观察"
                );
                ensure!(
                    if self.operator.is_some() {
                        decisions.len() == 1 && decisions[0].arm == ARMS[1]
                    } else {
                        decisions.len() == 2
                            && decisions.iter().all(|d| ARMS.contains(&d.arm.as_str()))
                            && decisions[0].arm != decisions[1].arm
                    },
                    "观察轨道不完整"
                );
                let i = asset(&e.asset)?;
                self.as_of_ms = self.as_of_ms.max(e.evaluated_at.timestamp_millis());
                self.observations[i] = Some(Observation {
                    slug: e.slug,
                    at: e.evaluated_at,
                    decisions,
                });
            }
            Event::Attempted { attempt: a } => {
                let e = &a.evaluation;
                let arm = arm(&a.arm)?;
                let asset = asset(&e.asset)?;
                ensure!(
                    self.registered
                        && self.runs.contains(&e.run_id)
                        && e.window_start_ms >= self.start_ms,
                    "无效研究尝试范围"
                );
                ensure!(
                    !self.ids.contains_key(&a.attempt_id)
                        && !self.markets.contains(&(arm, e.slug.clone())),
                    "重复尝试"
                );
                ensure!(
                    a.requested > D::ZERO
                        && a.reserved_cost > D::ZERO
                        && a.reserved_cost
                            <= D::from(if self.operator.is_some() { 100 } else { 5 }),
                    "无效资金预留"
                );
                ensure!(
                    e.ask_price >= D::new(96, 2)
                        && e.ask_price <= D::new(if arm == 1 && asset == 1 { 97 } else { 98 }, 2),
                    "尝试超出报价范围"
                );
                if let Some(c) = &self.operator {
                    ensure!(
                        !c.paused
                            && arm == 1
                            && a.budget_limit == Some(c.budget)
                            && a.parameter_revision == Some(c.revision)
                            && a.entry_epoch == Some(c.entry_epoch)
                            && a.reserved_cost <= c.budget,
                        "operator attempt revision/budget mismatch"
                    );
                }
                let sums = Stats::sum(&[
                    self.totals.get(&(arm, 0)).cloned().unwrap_or_default(),
                    self.totals.get(&(arm, 1)).cloned().unwrap_or_default(),
                ]);
                ensure!(
                    D::from(100) + self.deposits + sums.pnl - sums.exposure - sums.reserved
                        >= a.reserved_cost,
                    "模拟现金不足"
                );
                let row = Entry {
                    id: a.attempt_id,
                    arm,
                    asset,
                    day: e.evaluated_at.date_naive(),
                    at: e.evaluated_at,
                    end: e.end_date,
                    slug: e.slug.clone(),
                    evaluation_id: e.evaluation_id.clone(),
                    run_id: e.run_id.clone(),
                    ask: e.ask_price,
                    requested: a.requested,
                    reservation: a.reserved_cost,
                    entry_epoch: a.entry_epoch,
                    filled: D::ZERO,
                    cost: D::ZERO,
                    pnl: None,
                    won: None,
                    outcome: None,
                    attempt_offset: offset,
                    execution_offset: None,
                    settlement_offset: None,
                };
                let i = self.entries.len();
                self.ids.insert(row.id.clone(), i);
                self.markets.insert((arm, row.slug.clone()));
                self.by_day.entry((row.day, arm)).or_default().push(i);
                self.adjust(&row, 1);
                self.as_of_ms = self.as_of_ms.max(row.at.timestamp_millis());
                self.entries.push(row);
            }
            Event::Executed {
                attempt_id,
                execution: x,
            } => {
                let i = *self.ids.get(&attempt_id).context("执行缺少尝试")?;
                let old = self.entries[i].clone();
                ensure!(old.execution_offset.is_none(), "重复执行");
                ensure!(
                    x.slug == old.slug
                        && x.evaluation_id == old.evaluation_id
                        && x.run_id == old.run_id
                        && x.limit_price == old.ask
                        && x.requested == old.requested,
                    "执行关联错误"
                );
                ensure!(
                    x.simulated_at >= old.at
                        && x.filled >= D::ZERO
                        && x.filled <= old.requested
                        && x.cost >= D::ZERO
                        && x.cost <= old.reservation,
                    "执行金额或时序错误"
                );
                match x.outcome.as_str() {
                    "filled" => ensure!(
                        x.filled > D::ZERO && x.cost > D::ZERO && x.simulated_at < old.end,
                        "无效成交"
                    ),
                    "no_fill" | "error" => {
                        ensure!(x.filled == D::ZERO && x.cost == D::ZERO, "未成交存在敞口")
                    }
                    _ => bail!("未知执行状态"),
                }
                if x.filled > D::ZERO {
                    if let Some(c) = &self.operator {
                        ensure!(
                            !c.paused && old.entry_epoch == Some(c.entry_epoch),
                            "operator fill after pause"
                        );
                    }
                }
                let mut new = old.clone();
                new.execution_offset = Some(offset);
                new.cost = x.cost;
                new.filled = x.filled;
                new.outcome = Some(x.outcome);
                self.adjust(&old, -1);
                self.adjust(&new, 1);
                self.as_of_ms = self.as_of_ms.max(x.simulated_at.timestamp_millis());
                self.entries[i] = new;
            }
            Event::Settled {
                attempt_id,
                won,
                payout,
                pnl,
                resolved_at,
            } => {
                let i = *self.ids.get(&attempt_id).context("结算缺少尝试")?;
                let old = self.entries[i].clone();
                ensure!(
                    old.execution_offset.is_some() && old.pnl.is_none(),
                    "结算重复或缺少执行"
                );
                ensure!(
                    resolved_at >= old.end
                        && payout == if won { old.filled } else { D::ZERO }
                        && pnl == payout - old.cost,
                    "无效结算金额或时序"
                );
                let mut new = old.clone();
                new.pnl = Some(pnl);
                new.won = Some(won);
                new.settlement_offset = Some(offset);
                self.adjust(&old, -1);
                self.adjust(&new, 1);
                self.as_of_ms = self.as_of_ms.max(resolved_at.timestamp_millis());
                self.entries[i] = new;
            }
        }
        Ok(())
    }
    fn event_at(&mut self, offset: u64) -> Result<Event> {
        self.reader.seek(SeekFrom::Start(offset))?;
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }
    fn trade(&mut self, i: usize) -> Result<Trade> {
        let row = self.entries[i].clone();
        let Event::Attempted { attempt: a } = self.event_at(row.attempt_offset)? else {
            bail!("历史索引失效");
        };
        let x = if let Some(at) = row.execution_offset {
            let Event::Executed { execution, .. } = self.event_at(at)? else {
                bail!("执行索引失效");
            };
            Some(execution)
        } else {
            None
        };
        let settled_at = if let Some(at) = row.settlement_offset {
            let Event::Settled { resolved_at, .. } = self.event_at(at)? else {
                bail!("结算索引失效");
            };
            Some(resolved_at)
        } else {
            None
        };
        Ok(Trade {
            parameter_revision: a.parameter_revision,
            budget_limit: a.budget_limit,
            id: row.id,
            asset: if row.asset == 0 { "BTC" } else { "ETH" }.into(),
            slug: row.slug,
            at: x.as_ref().map(|x| x.simulated_at).unwrap_or(row.at),
            direction: a.evaluation.buy_yes,
            ask: row.ask,
            limit: x.as_ref().map(|x| x.limit_price).unwrap_or(row.ask),
            average: x
                .as_ref()
                .filter(|x| x.filled > D::ZERO)
                .map(|x| x.average_price),
            filled: row.filled,
            cost: row.cost,
            reserved: row.reservation,
            execution_error: x.as_ref().and_then(|x| x.error.clone()),
            fee: x.as_ref().map(|x| x.estimated_fee).unwrap_or(D::ZERO),
            pnl: row.pnl,
            outcome: row.outcome.unwrap_or_else(|| "unknown".into()),
            settled_at,
            evaluation: a.evaluation,
            request_ms: x.as_ref().and_then(|x| x.request_latency_ms),
            total_ms: x.as_ref().and_then(|x| x.total_latency_ms),
        })
    }
    pub fn view(
        &mut self,
        date: NaiveDate,
        arm: usize,
        filter: Option<usize>,
        page: usize,
        limit: usize,
    ) -> Result<LedgerView> {
        let mut total: [Stats; 3] = Default::default();
        let mut daily: [Stats; 3] = Default::default();
        for a in 0..2 {
            total[a] = self.totals.get(&(arm, a)).cloned().unwrap_or_default();
            daily[a] = self.daily.get(&(date, arm, a)).cloned().unwrap_or_default();
        }
        total[2] = Stats::sum(&total[..2]);
        daily[2] = Stats::sum(&daily[..2]);
        let ids = self
            .by_day
            .get(&(date, arm))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .filter(|i| filter.is_none_or(|a| a == self.entries[*i].asset))
            .collect::<Vec<_>>();
        let page = page.min(ids.len().saturating_sub(1) / limit.max(1));
        let mut rows = vec![];
        for i in ids.iter().skip(page * limit).take(limit) {
            rows.push(self.trade(*i)?);
        }
        Ok(LedgerView {
            capital: D::from(100) + self.deposits,
            deposits: self.deposits,
            budget: self.operator.as_ref().map_or(D::from(5), |c| c.budget),
            ready: self.registered,
            error: self.failed.clone(),
            loading: self.loading,
            partial: self.partial,
            start_ms: self.start_ms,
            as_of_ms: self.as_of_ms,
            run_count: self.runs.len(),
            daily,
            total,
            rows,
            row_count: ids.len(),
            page,
            oldest_pending_ms: self
                .entries
                .iter()
                .filter(|e| {
                    e.arm == arm && e.pnl.is_none() && (e.filled > D::ZERO || e.outcome.is_none())
                })
                .map(|e| e.at.timestamp_millis())
                .min(),
            observations: self.observations.clone(),
        })
    }
}
