//! Standalone Winner PAPER dashboard. Explicit controls target only the independent operator journal.
pub(crate) mod control;
mod i18n;
pub(crate) mod ledger;
pub(crate) mod live;
mod render;

use anyhow::{bail, ensure, Context, Result};
use chrono::{Duration as Days, NaiveDate};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use i18n::Language;
use ledger::{Ledger, LedgerView};
use live::Snapshot;
use ratatui::{
    backend::{CrosstermBackend, TestBackend},
    Terminal,
};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Query {
    date: NaiveDate,
    arm: usize,
    asset: Option<usize>,
    page: usize,
    limit: usize,
}
#[derive(Clone, Debug, Default)]
struct Data {
    query: Option<Query>,
    ledger: LedgerView,
    live: Option<Snapshot>,
    live_error: Option<String>,
    generation: u64,
}
#[derive(Clone)]
struct ControlDialog {
    action: control::Action,
    input: String,
    run_id: String,
    journal: String,
    revision: u64,
}
struct Ui {
    language: Language,
    query: Query,
    tab: usize,
    selected: usize,
    detail: bool,
    detail_scroll: u16,
    status_scroll: u16,
    status_scroll_max: std::cell::Cell<u16>,
    date_input: Option<String>,
    control_dialog: Option<ControlDialog>,
    outgoing: Option<control::Command>,
    command_busy: bool,
    message: String,
    highlights: bool,
    pulse_until: Option<Instant>,
    changed: std::collections::HashSet<String>,
    previous: std::collections::HashMap<String, String>,
    context: (NaiveDate, usize),
}
impl Ui {
    fn new(q: Query, highlights: bool) -> Self {
        Self {
            language: Language::default(),
            context: (q.date, q.arm),
            query: q,
            tab: 0,
            selected: 0,
            detail: false,
            detail_scroll: 0,
            status_scroll: 0,
            status_scroll_max: std::cell::Cell::new(200),
            date_input: None,
            control_dialog: None,
            outgoing: None,
            command_busy: false,
            message: String::new(),
            highlights,
            pulse_until: None,
            changed: Default::default(),
            previous: Default::default(),
        }
    }
    fn text<'a>(&self, english: &'a str, chinese: &'a str) -> &'a str {
        self.language.text(english, chinese)
    }
    fn update(&mut self, data: &Data) {
        let mut values = std::collections::HashMap::new();
        for (i, s) in data.ledger.daily.iter().enumerate() {
            values.insert(format!("day{i}"), s.pnl.round_dp(4).to_string());
        }
        for (i, s) in data.ledger.total.iter().enumerate() {
            values.insert(format!("total{i}"), s.pnl.round_dp(4).to_string());
        }
        if let Some(s) = &data.live {
            for q in &s.quotes {
                if let Some(v) = q.twap {
                    values.insert(format!("twap{}", q.asset), v.round_dp(2).to_string());
                }
            }
        }
        if self.context != (self.query.date, self.query.arm) {
            self.previous.clear();
            self.context = (self.query.date, self.query.arm);
        }
        self.changed = values
            .iter()
            .filter(|(k, v)| self.previous.get(*k).is_some_and(|old| old != *v))
            .map(|(k, _)| k.clone())
            .collect();
        self.pulse_until = if self.highlights && !self.changed.is_empty() {
            Some(Instant::now() + Duration::from_millis(450))
        } else {
            None
        };
        self.previous = values;
        self.query.page = data.ledger.page;
        self.selected = self.selected.min(data.ledger.rows.len().saturating_sub(1));
    }
    fn pulse(&self, key: &str) -> bool {
        self.pulse_until.is_some_and(|t| t > Instant::now()) && self.changed.contains(key)
    }
    fn key(&mut self, key: KeyCode, data: &Data) -> bool {
        if matches!(key, KeyCode::Char('l' | 'L')) {
            self.language.toggle();
            return false;
        }
        if let Some(dialog) = &mut self.control_dialog {
            match key {
                KeyCode::Esc => {
                    self.control_dialog = None;
                    self.message.clear();
                }
                KeyCode::Enter => {
                    if matches!(
                        dialog.action,
                        control::Action::SetBudget { .. } | control::Action::Deposit { .. }
                    ) {
                        match dialog
                            .input
                            .parse::<rust_decimal::Decimal>()
                            .map_err(anyhow::Error::from)
                            .and_then(|v| {
                                if matches!(dialog.action, control::Action::Deposit { .. }) {
                                    control::valid_deposit(v)?;
                                } else {
                                    control::valid_budget(v)?;
                                }
                                Ok(v)
                            }) {
                            Ok(amount) => {
                                dialog.action =
                                    if matches!(dialog.action, control::Action::Deposit { .. }) {
                                        control::Action::Deposit { amount }
                                    } else {
                                        control::Action::SetBudget { amount }
                                    }
                            }
                            Err(e) => {
                                self.message = e.to_string();
                                return false;
                            }
                        }
                    }
                    self.outgoing = Some(control::Command {
                        id: uuid::Uuid::new_v4().to_string(),
                        run_id: dialog.run_id.clone(),
                        journal: dialog.journal.clone(),
                        expected_revision: dialog.revision,
                        action: dialog.action.clone(),
                    });
                    self.command_busy = true;
                    self.control_dialog = None;
                    self.message = "Waiting for durable engine acknowledgement...".into();
                }
                KeyCode::Backspace => {
                    dialog.input.pop();
                }
                KeyCode::Char(c)
                    if matches!(
                        dialog.action,
                        control::Action::SetBudget { .. } | control::Action::Deposit { .. }
                    ) && (c.is_ascii_digit() || c == '.')
                        && dialog.input.len() < 10 =>
                {
                    dialog.input.push(c)
                }
                _ => {}
            }
            return false;
        }
        if let Some(input) = &mut self.date_input {
            match key {
                KeyCode::Esc => {
                    self.date_input = None;
                    self.message.clear();
                }
                KeyCode::Enter => match NaiveDate::parse_from_str(input, "%Y-%m-%d") {
                    Ok(d) if d <= ledger::today() => {
                        self.query.date = d;
                        self.date_input = None;
                        self.reset();
                        self.message.clear();
                    }
                    _ => self.message = "请输入有效日期 YYYY-MM-DD，不能晚于 UTC 今天".into(),
                },
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if (c.is_ascii_digit() || c == '-') && input.len() < 10 => {
                    input.push(c)
                }
                _ => {}
            }
            return false;
        }
        if self.detail {
            match key {
                KeyCode::Esc | KeyCode::Enter => self.detail = false,
                KeyCode::Char('q' | 'Q') => return true,
                KeyCode::Down | KeyCode::Char('j') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1).min(200)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => {
                    self.detail_scroll = self.detail_scroll.saturating_add(8).min(200)
                }
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(8),
                _ => {}
            }
            return false;
        }
        if self.tab == 2 {
            self.status_scroll = self.status_scroll.min(self.status_scroll_max.get());
            match key {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.status_scroll = self
                        .status_scroll
                        .saturating_add(1)
                        .min(self.status_scroll_max.get());
                    return false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.status_scroll = self.status_scroll.saturating_sub(1);
                    return false;
                }
                KeyCode::PageDown => {
                    self.status_scroll = self
                        .status_scroll
                        .saturating_add(8)
                        .min(self.status_scroll_max.get());
                    return false;
                }
                KeyCode::PageUp => {
                    self.status_scroll = self.status_scroll.saturating_sub(8);
                    return false;
                }
                _ => {}
            }
        }
        match key {
            KeyCode::Char('q' | 'Q') => return true,
            KeyCode::Tab => {
                self.tab = (self.tab + 1) % 3;
                self.reset();
            }
            KeyCode::BackTab => {
                self.tab = (self.tab + 2) % 3;
                self.reset();
            }
            KeyCode::Char('1') => self.tab = 0,
            KeyCode::Char('2') => self.tab = 1,
            KeyCode::Char('3') => self.tab = 2,
            KeyCode::Char('[') => {
                if let Some(d) = self.query.date.checked_sub_signed(Days::days(1)) {
                    self.query.date = d;
                    self.reset();
                }
            }
            KeyCode::Char(']') => {
                if self.query.date < ledger::today() {
                    if let Some(d) = self.query.date.checked_add_signed(Days::days(1)) {
                        self.query.date = d;
                        self.reset();
                    }
                }
            }
            KeyCode::Char('t' | 'T') => {
                self.query.date = ledger::today();
                self.reset();
            }
            KeyCode::Char('d' | 'D') => {
                self.date_input = Some(self.query.date.to_string());
            }
            KeyCode::Char('a' | 'A') => {
                self.query.asset = match self.query.asset {
                    None => Some(0),
                    Some(0) => Some(1),
                    _ => None,
                };
                self.reset();
            }
            KeyCode::Char('o' | 'O') => {
                self.query.arm = 2;
                self.reset();
            }
            KeyCode::Char('p' | 'P' | 'e' | 'E' | 'f' | 'F') => self.open_control(key, data),
            KeyCode::Char('r' | 'R') => {
                self.query.arm = (self.query.arm + 1)
                    % if data.live.as_ref().is_some_and(|s| s.operator.is_some()) {
                        3
                    } else {
                        2
                    };
                self.reset();
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(data.ledger.rows.len().saturating_sub(1))
            }
            KeyCode::PageDown => {
                if (self.query.page + 1) * self.query.limit < data.ledger.row_count {
                    self.query.page += 1;
                    self.selected = 0;
                }
            }
            KeyCode::PageUp => {
                self.query.page = self.query.page.saturating_sub(1);
                self.selected = 0;
            }
            KeyCode::Enter => {
                if !data.ledger.rows.is_empty() {
                    self.detail = true;
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Esc => {
                self.tab = 0;
                self.message.clear();
            }
            _ => {}
        }
        false
    }
    fn open_control(&mut self, key: KeyCode, data: &Data) {
        let valid = data.live.as_ref().filter(|s| {
            data.live_error.is_none()
                && chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(s.captured_ms)
                    <= 3000
        });
        let Some((engine, status)) = valid.and_then(|s| s.operator.as_ref().map(|o| (s, o))) else {
            self.message = self
                .text(
                    "Controls unavailable: connect to an upgraded engine",
                    "控制不可用：请连接已升级引擎",
                )
                .into();
            return;
        };
        if self.query.arm != 2 {
            self.message = self
                .text(
                    "Press O to select Operator PAPER; frozen arms are read-only",
                    "先按O选择Operator模拟轨道，冻结轨道只读",
                )
                .into();
            return;
        }
        if self.command_busy || status.failed {
            self.message = self
                .text(
                    "Control pending or operator journal failed",
                    "控制处理中或Operator账本故障",
                )
                .into();
            return;
        }
        let action = if matches!(key, KeyCode::Char('f' | 'F')) {
            if !status.funding_supported {
                self.message = self
                    .text(
                        "PAPER funding unavailable: upgrade the engine first",
                        "模拟入金不可用：请先升级后端",
                    )
                    .into();
                return;
            }
            control::Action::Deposit {
                amount: rust_decimal::Decimal::ZERO,
            }
        } else if matches!(key, KeyCode::Char('e' | 'E')) {
            control::Action::SetBudget {
                amount: status.settings.budget,
            }
        } else if status.settings.paused {
            control::Action::Resume
        } else {
            control::Action::Pause
        };
        self.message.clear();
        self.control_dialog = Some(ControlDialog {
            action,
            input: String::new(),
            run_id: engine.run_id.clone(),
            journal: status.journal.clone(),
            revision: status.settings.revision,
        });
    }
    fn follow_utc_day(&mut self, previous: NaiveDate, current: NaiveDate) -> bool {
        if current == previous || self.query.date != previous {
            return false;
        }
        self.query.date = current;
        self.reset();
        true
    }
    fn reset(&mut self) {
        self.query.page = 0;
        self.selected = 0;
        self.detail = false;
        self.detail_scroll = 0;
    }
}

struct Options {
    language: Language,
    journal: PathBuf,
    socket: PathBuf,
    date: NaiveDate,
    arm: usize,
    snapshot: bool,
    width: u16,
    height: u16,
    highlights: bool,
}
fn options(args: &[String]) -> Result<Options> {
    dotenvy::dotenv().ok();
    let mut journal=PathBuf::from(std::env::var("LATE_RESOLUTION_WINNER_PRICE_CAP_STUDY_FILE").unwrap_or_else(|_|"results/late-resolution-winner-trading/server-low-latency-007-5m/price-cap-study.ndjson".into()));
    let mut socket = None;
    let mut date = ledger::today();
    let mut arm = 1;
    let mut snapshot = false;
    let mut width = 110;
    let mut height = 34;
    let mut highlights = true;
    let mut language = Language::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        if flag == "--snapshot" {
            snapshot = true;
        } else if flag == "--no-highlight" {
            highlights = false;
        } else {
            i += 1;
            let value = args
                .get(i)
                .with_context(|| format!("Missing value for {flag}"))?;
            match flag {
                "--journal" => journal = value.into(),
                "--socket" => socket = Some(PathBuf::from(value)),
                "--date" => date = NaiveDate::parse_from_str(value, "%Y-%m-%d")?,
                "--arm" => {
                    arm = match value.as_str() {
                        "candidate" | "eth_cap_097" => 1,
                        "control" | "control_096_098" => 0,
                        "operator" => 2,
                        _ => bail!("--arm must be candidate, control or operator"),
                    }
                }
                "--lang" => {
                    language = match value.as_str() {
                        "en" => Language::English,
                        "zh" | "zh-CN" => Language::Chinese,
                        _ => bail!("--lang must be en or zh"),
                    };
                }
                "--width" => width = value.parse()?,
                "--height" => height = value.parse()?,
                _ => bail!("Unknown TUI option {flag}"),
            }
        }
        i += 1;
    }
    ensure!(
        (40..=240).contains(&width) && (12..=100).contains(&height),
        "Snapshot size must be 40..240 columns / 12..100 rows"
    );
    Ok(Options {
        language,
        socket: socket.unwrap_or_else(|| live::socket_path(&journal)),
        journal,
        date,
        arm,
        snapshot,
        width,
        height,
        highlights,
    })
}
pub fn operator_specification() -> serde_json::Value {
    serde_json::json!({"study_id":control::STUDY,"schema_version":2,"paper_only":true,
        "initial_paused":true,"initial_budget":"5","min_budget":"1","max_budget":"100",
        "budget_effective":"next_5min_window","frozen_arms_read_only":true,
        "funding_supported":true,"funding_event":"operator_funded","min_deposit":"0.01","max_deposit":"1000000","funding_affects_pnl":false})
}
pub fn help() -> &'static str {
    "tui [--journal FILE] [--date YYYY-MM-DD] [--arm candidate|control|operator] [--socket PATH] [--no-highlight]\n    Times and ledger dates use UTC; default date is today in UTC\n    [--lang en|zh] Default: English; press L to switch language\n    [--snapshot --width 110 --height 34] Read-only layout export\nKeys: Tab/1/2/3 Pages  [ ] Day  D Date  T Today (UTC)  A Asset  R Arm  O Operator  P Pause/Resume  E Budget  F Add PAPER cash (Enter confirms)  Arrows Select/Scroll  PgUp/PgDn Page  Enter Details  Esc Back  L Language  Q Exit"
}

/// Does not call Config::from_env, authenticate, install an engine, or modify any journal.
pub fn run(args: &[String]) -> Result<()> {
    if args.iter().any(|s| s == "--help" || s == "-h") {
        println!("{}", help());
        return Ok(());
    }
    let o = options(args)?;
    let query = Query {
        date: o.date,
        arm: o.arm,
        asset: None,
        page: 0,
        limit: render::trade_capacity(o.height, 0, render::header_height(o.height, false, false)),
    };
    if o.snapshot {
        let path = if query.arm == 2 {
            control::journal(&o.journal)
        } else {
            o.journal.clone()
        };
        let mut ledger = Ledger::open(&path)?;
        loop {
            ledger.refresh()?;
            if !ledger.loading {
                break;
            }
        }
        let view = ledger.view(query.date, query.arm.min(1), None, 0, query.limit)?;
        ensure!(view.ready, "Empty journal or study not registered");
        let data = Data {
            query: Some(query.clone()),
            ledger: view,
            live: None,
            live_error: Some("只读账本快照；无实时连接".into()),
            generation: 1,
        };
        let mut ui = Ui::new(query, false);
        ui.language = o.language;
        let mut terminal = Terminal::new(TestBackend::new(o.width, o.height))?;
        terminal.draw(|f| render::draw(f, &ui, &data))?;
        let buffer = terminal.backend().buffer();
        for y in 0..o.height {
            let mut line = String::new();
            let mut x = 0;
            while x < o.width {
                let symbol = buffer[(x, y)].symbol();
                line.push_str(symbol);
                x += ratatui::text::Text::from(symbol).width().max(1) as u16;
            }
            println!("{}", line.trim_end());
        }
        return Ok(());
    }
    ensure!(
        io::stdout().is_terminal() && io::stdin().is_terminal(),
        "TUI requires an interactive terminal; use --snapshot for offline review"
    );
    let query = Arc::new(Mutex::new(query));
    let shared = Arc::new(Mutex::new(Data::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (command_tx, command_rx) = std::sync::mpsc::sync_channel(2);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let worker = {
        let query = query.clone();
        let shared = shared.clone();
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("winner-tui-reader".into())
            .spawn(move || {
                worker(
                    o.journal, o.socket, query, shared, stop, command_rx, reply_tx,
                )
            })?
    };
    let result = interactive(
        query,
        shared,
        stop.clone(),
        o.highlights,
        o.language,
        command_tx,
        reply_rx,
    );
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    result
}
fn worker(
    path: PathBuf,
    socket: PathBuf,
    query: Arc<Mutex<Query>>,
    shared: Arc<Mutex<Data>>,
    stop: Arc<AtomicBool>,
    commands: std::sync::mpsc::Receiver<control::Command>,
    replies: std::sync::mpsc::Sender<Result<control::Reply, String>>,
) {
    let mut active_path = path.clone();
    let mut ledger = None;
    let mut cache = LedgerView::default();
    let mut cached_query = None;
    let mut revision = (0, true, false, (0, 0));
    let mut last_live = Instant::now() - Duration::from_secs(2);
    let mut live: Option<Snapshot> = None;
    let mut live_error = None;
    let mut generation = 0;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(command) = commands.try_recv() {
            let result = control::send(&socket, &command).map_err(|e| {
                format!("{e:#}; outcome unknown, refresh state before another action")
            });
            if let Ok(reply) = &result {
                if let Some(s) = &mut live {
                    if s.run_id == command.run_id {
                        s.operator = reply.status.clone();
                    }
                }
            }
            let _ = replies.send(result);
            last_live = Instant::now() - Duration::from_secs(2);
        }
        let q = query.lock().unwrap().clone();
        let selected_path = if q.arm == 2 {
            control::journal(&path)
        } else {
            path.clone()
        };
        if selected_path != active_path {
            active_path = selected_path;
            ledger = None;
        }
        let mut dirty = cached_query.as_ref() != Some(&q);
        // Never label cached amounts with a newly selected date/arm when a read fails.
        if dirty {
            cache = LedgerView::default();
        }
        if ledger.is_none() {
            match Ledger::open(&active_path) {
                Ok(l) => {
                    ledger = Some(l);
                    dirty = true;
                }
                Err(e) => {
                    cache.error = Some(format!("{e:#}"));
                    cache.loading = false;
                }
            }
        }
        if let Some(l) = &mut ledger {
            let old = l.revision();
            let refreshed = l.refresh();
            if l.revision() != old || revision != l.revision() {
                dirty = true;
            }
            match refreshed {
                Ok(()) => {
                    if dirty {
                        match l.view(q.date, q.arm.min(1), q.asset, q.page, q.limit) {
                            Ok(v) => cache = v,
                            Err(e) => cache.error = Some(format!("{e:#}")),
                        }
                    }
                }
                Err(e) => cache.error = Some(format!("{e:#}")),
            }
            revision = l.revision();
        }
        if last_live.elapsed() >= Duration::from_secs(1) {
            match live::read(&socket, &path) {
                Ok(mut s) => {
                    if let Some(previous) = &live {
                        if previous.run_id == s.run_id
                            && previous.operator.as_ref().is_some_and(|old| {
                                s.operator.as_ref().is_some_and(|new| {
                                    old.settings.revision > new.settings.revision
                                })
                            })
                        {
                            s.operator = previous.operator.clone();
                        }
                    }
                    live = Some(s);
                    live_error = None;
                }
                Err(e) => live_error = Some(format!("{e:#}")),
            }
            last_live = Instant::now();
            dirty = true;
        }
        if dirty || cache.error.is_some() {
            generation += 1;
            *shared.lock().unwrap() = Data {
                query: Some(q.clone()),
                ledger: cache.clone(),
                live: live.clone(),
                live_error: live_error.clone(),
                generation,
            };
        }
        cached_query = Some(q);
        // Interruptible polling; initial replay and file work stay off the draw thread.
        for _ in 0..5 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }
}
fn interactive(
    query: Arc<Mutex<Query>>,
    shared: Arc<Mutex<Data>>,
    stop: Arc<AtomicBool>,
    highlights: bool,
    language: Language,
    commands: std::sync::mpsc::SyncSender<control::Command>,
    replies: std::sync::mpsc::Receiver<Result<control::Reply, String>>,
) -> Result<()> {
    struct TerminalGuard;
    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        }
    }
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut ui = Ui::new(query.lock().unwrap().clone(), highlights);
    ui.language = language;
    let mut data = Data::default();
    let mut tick = Instant::now() - Duration::from_secs(2);
    let mut utc_day = ledger::today();
    let mut dirty = true;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(reply) = replies.try_recv() {
            ui.command_busy = false;
            ui.message = match reply {
                Ok(r) => format!(
                    "{}: {}",
                    if r.accepted { "ACK" } else { "NOT ACK" },
                    r.message
                ),
                Err(e) => e,
            };
            dirty = true;
        }
        let h = terminal.size()?.height;
        let limit = render::trade_capacity(
            h,
            ui.tab,
            render::header_height(
                h,
                data.live.as_ref().is_some_and(|s| s.operator.is_some()),
                !ui.message.is_empty(),
            ),
        );
        if ui.query.limit != limit {
            ui.query.limit = limit;
            ui.query.page = 0;
            ui.selected = 0;
            *query.lock().unwrap() = ui.query.clone();
            data.ledger = LedgerView::default();
            dirty = true;
        }
        let next = shared.lock().unwrap().clone();
        if next.generation != data.generation {
            if next.query.as_ref() == Some(&ui.query) {
                let selected_id = data
                    .ledger
                    .rows
                    .get(ui.selected)
                    .filter(|_| ui.detail || ui.selected > 0)
                    .map(|t| t.id.clone());
                data = next;
                ui.update(&data);
                if let Some(id) = selected_id {
                    if let Some(index) = data.ledger.rows.iter().position(|t| t.id == id) {
                        ui.selected = index;
                    } else {
                        ui.detail = false;
                    }
                }
                *query.lock().unwrap() = ui.query.clone();
            } else {
                data = Data {
                    live: next.live,
                    live_error: next.live_error,
                    generation: next.generation,
                    ..Default::default()
                };
            }
            dirty = true;
        }
        if ui.pulse_until.is_some_and(|t| t <= Instant::now()) {
            ui.pulse_until = None;
            ui.changed.clear();
            dirty = true;
        }
        if tick.elapsed() >= Duration::from_secs(1) {
            tick = Instant::now();
            let current_day = ledger::today();
            if ui.follow_utc_day(utc_day, current_day) {
                *query.lock().unwrap() = ui.query.clone();
                data.ledger = LedgerView::default();
            }
            utc_day = current_day;
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| render::draw(f, &ui, &data))?;
            dirty = false;
        }
        // Keys remain responsive; polling does not cause a redraw.
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if k.code == KeyCode::Char('c')
                        && k.modifiers.contains(event::KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    let size = terminal.size()?;
                    if (size.width < 80 || size.height < 24)
                        && (ui.control_dialog.is_some()
                            || matches!(k.code, KeyCode::Char('p' | 'P' | 'e' | 'E' | 'f' | 'F')))
                    {
                        if k.code == KeyCode::Esc {
                            ui.control_dialog = None;
                        }
                        ui.message = "Resize to at least 80x24 before confirming controls".into();
                        dirty = true;
                        continue;
                    }
                    let old_query = ui.query.clone();
                    if ui.key(k.code, &data) {
                        break;
                    }
                    if let Some(command) = ui.outgoing.take() {
                        if commands.try_send(command).is_err() {
                            ui.command_busy = false;
                            ui.message = "Command queue unavailable".into();
                        }
                    }
                    if old_query != ui.query {
                        data.ledger = LedgerView::default();
                    }
                    *query.lock().unwrap() = ui.query.clone();
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
    terminal.show_cursor()?;
    io::stdout().flush()?;
    Ok(())
}

fn age(now: i64, at: Option<i64>, language: Language) -> String {
    match at {
        None => "--".into(),
        Some(at) if at > now + 1000 => language.text("Clock skew", "时钟异常").into(),
        Some(at) => {
            let secs = now.saturating_sub(at).max(0) / 1000;
            if secs < 60 {
                format!("{secs}s")
            } else if secs < 3600 {
                format!("{}m{}s", secs / 60, secs % 60)
            } else {
                format!("{}h{}m", secs / 3600, secs % 3600 / 60)
            }
        }
    }
}
fn health(data: &Data, now: i64, language: Language) -> (String, bool) {
    let tr = |en, zh| language.text(en, zh).to_owned();
    if let Some(e) = &data.ledger.error {
        return (
            format!(
                "{}: {} ({})",
                tr("Ledger error", "账本错误"),
                language.diagnostic(e),
                tr("updates stopped", "停止更新统计")
            ),
            true,
        );
    }
    if !data.ledger.ready || data.ledger.loading {
        return (
            tr(
                "Loading ledger; PnL is incomplete",
                "账本加载中；尚不作为完整收益统计",
            ),
            true,
        );
    }
    let Some(s) = &data.live else {
        return (
            tr(
                "Offline; ledger only, engine health unavailable",
                "离线 / 未连接策略；仅显示账本，不推断运行健康",
            ),
            true,
        );
    };
    if data.live_error.is_some() || now - s.captured_ms > 3500 {
        return (
            format!(
                "{} | {} {}",
                tr("Live connection interrupted", "实时连接中断"),
                tr("Snapshot age", "快照年龄"),
                age(now, Some(s.captured_ms), language)
            ),
            true,
        );
    }
    if now - s.loop_ms > 30_000 {
        return (
            format!(
                "{} {} | {} | {} {}",
                tr("Loop stalled", "策略未推进"),
                age(now, Some(s.loop_ms), language),
                language.diagnostic(&s.phase),
                tr("Target", "目标窗口"),
                s.target_window
            ),
            true,
        );
    }
    if s.phase == "发现市场" && s.target_window > 0 && s.target_window + 300 < now / 1000 {
        return (
            tr(
                "Market discovery is retrying an expired window",
                "发现市场正在重试已过期窗口",
            ),
            true,
        );
    }
    if data.ledger.total[2].unknown > 0 {
        return (
            tr(
                "Unknown executions; reserved funds remain held",
                "存在执行未知记录；预留资金未释放，请核查",
            ),
            true,
        );
    }
    if data.ledger.total[2].pending > 0 && s.settlement_ms.is_none_or(|at| now - at > 30_000) {
        return (
            tr(
                "Pending positions; settlement checks are stalled",
                "待结算持仓存在，但结算检查未推进",
            ),
            true,
        );
    }
    if s.phase == "发现市场" {
        return (
            tr(
                "Waiting for market discovery; sampling is not confirmed",
                "等待市场发现；不等同于正常采样",
            ),
            true,
        );
    }
    if !s.market_ws_connected {
        return (
            tr(
                "Market feed is not ready; entry paused",
                "行情连接未就绪，入场暂停",
            ),
            true,
        );
    }
    if s.quotes.iter().any(|q| {
        q.reference_ms
            .is_none_or(|t| t > now + 1000 || now - t > s.feed_max_age_ms)
    }) {
        return (
            tr(
                "Loop advancing; reference feed is missing or stale",
                "策略循环推进，但参考行情缺失/陈旧",
            ),
            true,
        );
    }
    (tr("Strategy loop advancing", "策略循环推进中"), false)
}

#[cfg(test)]
mod tests;
