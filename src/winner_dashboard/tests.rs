use super::ledger::{Stats, ARMS};
use super::*;
use rust_decimal::Decimal as D;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
};

struct Fixture {
    dir: PathBuf,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("winner-tui-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("study.ndjson");
        fs::write(&path, b"").unwrap();
        Self { dir, path }
    }
    fn append(&self, v: Value) {
        let mut f = OpenOptions::new().append(true).open(&self.path).unwrap();
        writeln!(f, "{v}").unwrap();
    }
    fn header(&self) {
        self.append(json!({"action":"registered","version":1,"study_id":"eth-ask-cap-097-v1","registered_at":"2026-10-26T03:11:20Z","forward_window_start_ms":1792984500000_i64,"configuration":{"paper_only":true,"automatic_promotion":false,"initial_cash_per_arm":"100","max_order_usdc":"5","arms":{ARMS[0]:{"eth_max_ask":"0.98"},ARMS[1]:{"eth_max_ask":"0.97"}}}}));
        self.restart("run1");
    }
    fn restart(&self, id: &str) {
        self.append(
            json!({"action":"run_started","run_id":id,"started_at":"2026-10-27T03:11:20Z"}),
        );
    }
    fn evaluation(&self, id: &str) -> Value {
        json!({"run_id":"run1","evaluation_id":id,"candidate_stage":"independent_price_cap_paper","asset":"eth","slug":format!("eth-updown-5m-{id}"),"end_date":"2026-11-03T00:00:00Z","evaluated_at":"2026-11-02T23:59:58Z","window_start_ms":1793663700000_i64,"seconds_remaining":2,"buy_yes":true,"bid_price":"0.96","ask_price":"0.97","ask_size":"50","fee_per_share":"0.002037","rejection":null,"failed_checks":["ReversalBoundary"],"opening_twap":"2200","current_twap":"2201","model_observed_at_ms":1793663997000_i64,"model_probability":"0.6","model_net_ev":"-0.3","directional_margin_bps":1.0,"reversal_boundary_bps":10.0,"volatility_bps_per_sqrt_second":1.2,"spread":"0.01","depth_usdc":"48.5"})
    }
    fn attempt(&self, id: &str) {
        self.append(json!({"action":"attempted","attempt":{"attempt_id":id,"arm":ARMS[1],"evaluation":self.evaluation(id),"requested":"5.14","reserved_cost":"4.99627018"}}));
    }
    fn execute(&self, id: &str, outcome: &str) {
        let filled = outcome == "filled";
        self.append(json!({"action":"executed","attempt_id":id,"execution":{"run_id":"run1","evaluation_id":id,"slug":format!("eth-updown-5m-{id}"),"simulated_at":"2026-11-02T23:59:58.3Z","delay_ms":250,"candidate_stage":"independent_price_cap_paper","research_cohorts":[],"outcome":outcome,"limit_price":"0.97","requested":"5.14","filled":if filled{"5.14"}else{"0"},"cost":if filled{"4.99627018"}else{"0"},"average_price":if filled{"0.972037"}else{"0"},"estimated_fee":if filled{"0.01047018"}else{"0"}}}));
    }
    fn settle(&self, id: &str, won: bool, filled: bool) {
        let cost = if filled {
            D::from_str_exact("4.99627018").unwrap()
        } else {
            D::ZERO
        };
        let payout = if filled && won {
            D::new(514, 2)
        } else {
            D::ZERO
        };
        self.append(json!({"action":"settled","attempt_id":id,"won":won,"payout":payout.to_string(),"pnl":(payout-cost).to_string(),"resolved_at":"2026-11-03T00:05:00Z"}));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn query() -> Query {
    Query {
        date: NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(),
        arm: 1,
        asset: None,
        page: 0,
        limit: 20,
    }
}

#[test]
fn money_unknown_pending_no_fill_and_late_settlement_survive_restart() {
    let f = Fixture::new();
    f.header();
    f.attempt("a");
    let original = fs::read(&f.path).unwrap();
    let mut l = Ledger::open(&f.path).unwrap();
    l.refresh().unwrap();
    let q = query();
    let v = l.view(q.date, 1, None, 0, 20).unwrap();
    assert_eq!(v.total[2].unknown, 1);
    assert_eq!(v.total[2].pnl, D::ZERO);
    assert_eq!(
        v.total[2].reserved,
        D::from_str_exact("4.99627018").unwrap()
    );
    assert_eq!(original, fs::read(&f.path).unwrap());
    f.execute("a", "filled");
    l.refresh().unwrap();
    let v = l.view(q.date, 1, None, 0, 20).unwrap();
    assert_eq!(v.total[2].pending, 1);
    assert_eq!(v.total[2].unknown, 0);
    assert_eq!(v.total[2].pnl, D::ZERO);
    f.restart("run2");
    f.settle("a", true, true);
    f.attempt("b");
    f.execute("b", "no_fill");
    f.settle("b", true, false);
    f.attempt("c");
    f.execute("c", "error");
    f.settle("c", false, false);
    l.refresh().unwrap();
    let v = l.view(q.date, 1, None, 0, 20).unwrap();
    assert_eq!(v.run_count, 2);
    assert_eq!(v.total[2].wins, 1);
    assert_eq!(v.total[2].losses, 0);
    assert_eq!(v.total[2].no_fills, 1);
    assert_eq!(v.total[2].errors, 1);
    assert_eq!(v.total[2].pending, 0);
    assert_eq!(v.daily[2].pnl, D::from_str_exact("0.14372982").unwrap());
    assert_eq!(v.daily[2].wins, 1);
    assert_eq!(v.row_count, 3);
    assert_eq!(v.total[2].cost, D::from_str_exact("4.99627018").unwrap());
    assert_eq!(
        l.view(q.date.succ_opt().unwrap(), 1, None, 0, 20)
            .unwrap()
            .daily[2]
            .pnl,
        D::ZERO,
        "settlement on the next UTC date must not move entry-day PnL"
    );
    let mut reopened = Ledger::open(&f.path).unwrap();
    reopened.refresh().unwrap();
    assert_eq!(
        reopened.view(q.date, 1, None, 0, 20).unwrap().total,
        v.total
    );
}
#[test]
fn utc_midnight_advances_today_but_leaves_history_selected() {
    let q = query();
    let previous = q.date;
    let current = previous.succ_opt().unwrap();
    let mut ui = Ui::new(q, false);
    ui.query.page = 3;
    ui.detail = true;
    assert!(ui.follow_utc_day(previous, current));
    assert_eq!(ui.query.date, current);
    assert_eq!(ui.query.page, 0);
    assert!(!ui.detail);

    ui.query.date = previous;
    assert!(!ui.follow_utc_day(current, current.succ_opt().unwrap()));
    assert_eq!(ui.query.date, previous);
}
#[test]
fn incomplete_tail_is_deferred_duplicate_settlement_fails_closed() {
    let f = Fixture::new();
    f.header();
    f.attempt("a");
    f.execute("a", "filled");
    let mut l = Ledger::open(&f.path).unwrap();
    l.refresh().unwrap();
    let line=json!({"action":"settled","attempt_id":"a","won":false,"payout":"0","pnl":"-4.99627018","resolved_at":"2026-11-03T00:05:00Z"}).to_string();
    let half = line.len() / 2;
    let mut file = OpenOptions::new().append(true).open(&f.path).unwrap();
    file.write_all(&line.as_bytes()[..half]).unwrap();
    file.flush().unwrap();
    l.refresh().unwrap();
    assert!(l.partial);
    assert_eq!(
        l.view(query().date, 1, None, 0, 20).unwrap().total[2].losses,
        0
    );
    writeln!(file, "{}", &line[half..]).unwrap();
    file.flush().unwrap();
    l.refresh().unwrap();
    assert!(!l.partial);
    assert_eq!(
        l.view(query().date, 1, None, 0, 20).unwrap().total[2].losses,
        1
    );
    f.settle("a", false, true);
    assert!(l.refresh().is_err());
    assert!(l.refresh().is_err());
}
#[test]
fn replacement_rebuilds_without_double_counting() {
    let f = Fixture::new();
    f.header();
    f.attempt("a");
    f.execute("a", "filled");
    f.settle("a", true, true);
    let raw = fs::read(&f.path).unwrap();
    let mut l = Ledger::open(&f.path).unwrap();
    l.refresh().unwrap();
    let replacement = f.dir.join("replacement");
    fs::write(&replacement, &raw).unwrap();
    fs::rename(replacement, &f.path).unwrap();
    l.refresh().unwrap();
    assert_eq!(
        l.view(query().date, 1, None, 0, 20).unwrap().total[2].wins,
        1
    );
}
#[test]
fn ui_navigation_is_read_only_and_pulses_do_not_start_on_load_or_date_switch() {
    let mut ui = Ui::new(query(), true);
    let mut d = Data::default();
    ui.update(&d);
    assert!(ui.pulse_until.is_none());
    d.ledger.daily[0].pnl = D::ONE;
    ui.update(&d);
    assert!(ui.pulse("day0"));
    ui.key(KeyCode::Char('['), &d);
    ui.update(&d);
    assert!(!ui.pulse("day0"));
    let before = ui.query.clone();
    assert!(ui.key(KeyCode::Char('q'), &d));
    assert_eq!(before, ui.query);
    assert!(!ui.key(KeyCode::Char(' '), &d));
    assert!(!ui.key(KeyCode::Char('p'), &d));
    ui.key(KeyCode::Char('d'), &d);
    ui.date_input = Some("2026-02-30".into());
    ui.key(KeyCode::Enter, &d);
    assert!(ui.date_input.is_some());
    assert!(!ui.message.is_empty());
}
#[test]
fn expired_discovery_target_and_loop_stall_are_not_healthy() {
    let now = chrono::Utc::now().timestamp_millis();
    let mut d = Data::default();
    d.ledger.ready = true;
    d.ledger.loading = false;
    let s = Snapshot {
        operator: None,
        version: 1,
        pid: 1,
        run_id: "r".into(),
        journal: "j".into(),
        started_ms: now - 1000,
        captured_ms: now,
        loop_ms: now - 40_000,
        scan_ms: Some(now - 40_000),
        settlement_ms: Some(now - 40_000),
        target_window: now / 300_000 * 300 - 600,
        phase: "发现市场".into(),
        scans: 1,
        market_ws_connected: true,
        feed_max_age_ms: 5000,
        book_max_age_ms: 1000,
        rest_calls: 0,
        rest_successes: 0,
        rest_total_ms: 0,
        ws_reconnects: 0,
        quotes: vec![],
    };
    d.live = Some(s);
    assert!(health(&d, now, Language::Chinese).0.contains("未推进"));
    d.live.as_mut().unwrap().loop_ms = now;
    assert!(health(&d, now, Language::Chinese).0.contains("过期"));
    d.live_error = Some("断开".into());
    assert!(health(&d, now, Language::Chinese).0.contains("中断"));
}
#[test]
fn aligned_layouts_include_financial_headers_and_small_terminal_warning() {
    let f = Fixture::new();
    f.header();
    f.attempt("a");
    f.execute("a", "filled");
    f.settle("a", true, true);
    let mut l = Ledger::open(&f.path).unwrap();
    l.refresh().unwrap();
    let d = Data {
        ledger: l.view(query().date, 1, None, 0, 20).unwrap(),
        ..Default::default()
    };
    let mut ui = Ui::new(query(), false);
    ui.language = Language::Chinese;
    fn text_of(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x += ratatui::text::Text::from(symbol).width().max(1) as u16;
            }
            text.push('\n');
        }
        text
    }
    for (w, h) in [(80, 24), (110, 34), (160, 45)] {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render::draw(f, &ui, &d)).unwrap();
        let text = text_of(t.backend().buffer());
        assert!(text.contains("+0.1437"));
        assert!(text.contains("TWAP / USD"));
        assert!(text.contains("累计时间"), "{w}x{h}\n{text}");
        assert!(!text.contains("累计起点"));
        assert!(!text.contains("前后一天"));
        ui.tab = 1;
        t.draw(|f| render::draw(f, &ui, &d)).unwrap();
        let text = text_of(t.backend().buffer());
        assert!(text.contains("含费均价"), "{w}x{h}\n{text}");
        assert!(text.contains("净盈亏USDC"));
        ui.tab = 0;
    }
    let mut t = Terminal::new(TestBackend::new(60, 18)).unwrap();
    t.draw(|f| render::draw(f, &ui, &d)).unwrap();
    assert!(text_of(t.backend().buffer()).contains("80×24"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_is_read_only_batch_scoped_and_lives_after_viewer_disconnect() {
    let f = Fixture::new();
    f.header();
    let raw = fs::read(&f.path).unwrap();
    let state = live::LiveState::new("read-only-test", &f.path);
    state.loop_tick();
    let stop = Arc::new(AtomicBool::new(false));
    let socket = live::start(
        state,
        Arc::new(crate::late_resolution_winner_model::ChainlinkTwapStore::default()),
        Arc::new(crate::runtime_metrics::RuntimeMetrics::default()),
        stop.clone(),
        5000,
        1000,
        1000,
    )
    .unwrap();
    let s = live::read(&socket, &f.path).unwrap();
    assert_eq!(s.run_id, "read-only-test");
    assert!(!stop.load(Ordering::Relaxed));
    assert_eq!(raw, fs::read(&f.path).unwrap());
    assert!(live::read(&socket, &f.dir.join("wrong.ndjson")).is_err());
    assert!(live::read(&socket, &f.path).is_ok());
    stop.store(true, Ordering::Relaxed);
    for _ in 0..30 {
        if !socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(!socket.exists());
}
#[test]
fn all_daily_stats_sum_exactly_without_fee_double_counting() {
    let s = Stats {
        pnl: D::new(14372982, 8),
        cost: D::new(499627018, 8),
        wins: 1,
        ..Default::default()
    };
    let sum = Stats::sum(&[s.clone(), s]);
    assert_eq!(sum.pnl, D::new(28745964, 8));
    assert_eq!(sum.wins, 2);
}

fn operator_data() -> Data {
    let now = chrono::Utc::now().timestamp_millis();
    let snapshot = serde_json::from_value(json!({"version":1,"pid":1,"run_id":"engine-run","journal":"baseline",
        "started_ms":now,"captured_ms":now,"loop_ms":now,"scan_ms":now,"settlement_ms":now,
        "target_window":now/300000*300,"phase":"监控窗口","scans":10,"market_ws_connected":true,
        "feed_max_age_ms":5000,"book_max_age_ms":1000,"rest_calls":0,"rest_successes":0,"rest_total_ms":0,
        "ws_reconnects":0,"quotes":[],"operator":{"journal":"operator-paper.ndjson","settings":{
            "revision":0,"entry_epoch":0,"paused":true,"budget":"5","pending_budget":null,"effective_window_ms":null},
            "failed":false,"inflight":0,"pending_fills":0,"cash":"100","capital":"100","deposits":"0","funding_supported":true}})).unwrap();
    Data {
        live: Some(snapshot),
        ..Default::default()
    }
}

#[test]
fn operator_controls_require_explicit_target_confirmation_and_fresh_engine() {
    let mut ui = Ui::new(query(), false);
    let mut data = operator_data();
    ui.key(KeyCode::Char('p'), &data);
    assert!(ui.control_dialog.is_none());
    ui.key(KeyCode::Char('o'), &data);
    assert_eq!(ui.query.arm, 2);
    ui.key(KeyCode::Char('p'), &data);
    assert!(ui.outgoing.is_none());
    assert!(ui.control_dialog.is_some());
    ui.key(KeyCode::Esc, &data);
    assert!(ui.outgoing.is_none());
    ui.key(KeyCode::Char('p'), &data);
    ui.key(KeyCode::Enter, &data);
    let c = ui.outgoing.take().unwrap();
    assert_eq!(c.run_id, "engine-run");
    assert!(matches!(c.action, control::Action::Resume));
    assert!(ui.command_busy);
    ui.command_busy = false;
    data.live_error = Some("offline".into());
    ui.key(KeyCode::Char('p'), &data);
    assert!(ui.control_dialog.is_none());
    data.live_error = None;
    data.live.as_mut().unwrap().captured_ms -= 10_000;
    ui.key(KeyCode::Char('p'), &data);
    assert!(ui.control_dialog.is_none());
}

#[test]
fn operator_budget_dialog_validates_and_layout_remains_aligned_in_both_languages() {
    let data = operator_data();
    let mut ui = Ui::new(query(), false);
    ui.query.arm = 2;
    ui.key(KeyCode::Char('e'), &data);
    for c in "1.001".chars() {
        ui.key(KeyCode::Char(c), &data);
    }
    ui.key(KeyCode::Enter, &data);
    assert!(ui.outgoing.is_none());
    assert!(ui.control_dialog.is_some());
    for lang in [Language::English, Language::Chinese] {
        ui.language = lang;
        for (w, h) in [(80, 24), (110, 34), (160, 45)] {
            let text = buffer_text(&draw_buffer(&ui, &data, w, h));
            assert!(text.contains("Operator"));
            assert!(text.contains(ui.text("Enter Confirm", "Enter确认")));
            assert!(text
                .lines()
                .all(|l| ratatui::text::Text::from(l).width() <= w as usize));
        }
    }
    ui.key(KeyCode::Esc, &data);
    ui.key(KeyCode::Char('e'), &data);
    for c in "10".chars() {
        ui.key(KeyCode::Char(c), &data);
    }
    ui.key(KeyCode::Enter, &data);
    assert!(
        matches!(ui.outgoing.take().unwrap().action, control::Action::SetBudget { amount } if amount == D::from(10))
    );
}

#[test]
fn unknown_execution_does_not_claim_zero_cost_or_an_execution_time() {
    let f = Fixture::new();
    f.header();
    f.attempt("unknown");
    let mut l = Ledger::open(&f.path).unwrap();
    l.refresh().unwrap();
    let view = l.view(query().date, 1, None, 0, 20).unwrap();
    assert_eq!(
        view.rows[0].reserved,
        D::from_str_exact("4.99627018").unwrap()
    );
    let mut ui = Ui::new(query(), false);
    ui.detail = true;
    ui.language = Language::Chinese;
    let d = Data {
        ledger: view,
        ..Default::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
    terminal.draw(|f| render::draw(f, &ui, &d)).unwrap();
    let b = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..34 {
        let mut x = 0;
        while x < 110 {
            let symbol = b[(x, y)].symbol();
            text.push_str(symbol);
            x += ratatui::text::Text::from(symbol).width().max(1) as u16;
        }
    }
    assert!(text.contains("实际成本 未知"), "{text}");
    assert!(text.contains("预留成本 4.99627018"));
    assert!(text.contains("执行 未知"));
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let mut text = String::new();
    for y in 0..buffer.area.height {
        let mut x = 0;
        while x < buffer.area.width {
            let symbol = buffer[(x, y)].symbol();
            text.push_str(symbol);
            x += ratatui::text::Text::from(symbol).width().max(1) as u16;
        }
        text.push('\n');
    }
    text
}
fn draw_buffer(ui: &Ui, data: &Data, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render::draw(f, ui, data)).unwrap();
    terminal.backend().buffer().clone()
}

#[test]
fn language_defaults_to_english_and_switches_in_every_view_without_changing_query() {
    assert_eq!(options(&[]).unwrap().language, Language::English);
    for (arg, expected) in [
        ("en", Language::English),
        ("zh", Language::Chinese),
        ("zh-CN", Language::Chinese),
    ] {
        assert_eq!(
            options(&["--lang".into(), arg.into()]).unwrap().language,
            expected
        );
    }
    assert!(options(&["--lang".into(), "fr".into()]).is_err());
    let mut ui = Ui::new(query(), true);
    let data = Data::default();
    for (tab, detail, date_input) in [
        (0, false, None),
        (1, false, None),
        (2, false, None),
        (1, true, None),
        (0, false, Some("2026-11-02".into())),
    ] {
        ui.tab = tab;
        ui.detail = detail;
        ui.date_input = date_input.clone();
        ui.detail_scroll = 3;
        ui.status_scroll = 2;
        let before = ui.query.clone();
        assert!(!ui.key(KeyCode::Char('L'), &data));
        assert_eq!(ui.language, Language::Chinese);
        assert_eq!(ui.query, before);
        assert_eq!(ui.tab, tab);
        assert_eq!(ui.detail, detail);
        assert_eq!(ui.date_input, date_input);
        assert_eq!(ui.detail_scroll, 3);
        assert_eq!(ui.status_scroll, 2);
        assert!(!ui.key(KeyCode::Char('l'), &data));
        assert_eq!(ui.language, Language::English);
    }
}

#[test]
fn bilingual_layouts_have_one_line_help_padding_and_unchanged_financial_values() {
    let f = Fixture::new();
    f.header();
    for i in 0..25 {
        let id = i.to_string();
        f.attempt(&id);
        f.execute(&id, "filled");
        f.settle(&id, true, true);
    }
    let original = fs::read(&f.path).unwrap();
    let mut ledger = Ledger::open(&f.path).unwrap();
    ledger.refresh().unwrap();
    let mut ui = Ui::new(query(), false);
    for language in [Language::English, Language::Chinese] {
        ui.language = language;
        for (w, h) in [(80, 24), (80, 32), (110, 34), (160, 45)] {
            for tab in 0..3 {
                ui.tab = tab;
                ui.query.limit = render::trade_capacity(
                    h,
                    tab,
                    render::header_height(h, false, !ui.message.is_empty()),
                );
                let data = Data {
                    ledger: ledger
                        .view(ui.query.date, 1, None, 0, ui.query.limit)
                        .unwrap(),
                    ..Default::default()
                };
                let b = draw_buffer(&ui, &data, w, h);
                let text = buffer_text(&b);
                let lines: Vec<_> = text.lines().collect();
                assert!(
                    lines[h as usize - 1].contains(ui.text("Q Exit", "Q退出")),
                    "{text}"
                );
                assert!(lines[h as usize - 1].contains(ui.text("L 中文", "L English")));
                assert!(lines[..h as usize - 1]
                    .iter()
                    .all(|l| !l.contains(ui.text("Q Exit", "Q退出"))));
                assert!(ratatui::text::Text::from(render::footer(&ui, w)).width() <= w as usize);
                assert!(!text.contains("Q仅退出"));
                assert!(!text.contains("前后一天"));
                assert!(lines[0].contains(" UTC "), "{text}");
                if tab == 0 {
                    assert!(text.contains("+3.5932"), "{text}");
                    assert!(
                        text.contains(ui.text("Cumulative period", "累计时间")),
                        "{text}"
                    );
                    assert!(text.contains(" - "));
                }
                if tab == 1 {
                    assert!(text.contains(ui.text("Avg+fee", "含费均价")), "{text}");
                    assert!(text.contains("23:59:58"), "{text}");
                    assert!(text.contains("+0.1437"));
                    // Reader pagination must account for the new padding, so all
                    // fetched trades fit without relying on Table's hidden scroll.
                    assert_eq!(
                        text.matches("+0.1437").count(),
                        data.ledger.rows.len(),
                        "{text}"
                    );
                }
                let header = render::header_height(h, false, !ui.message.is_empty());
                let body = header + if h < 32 { 0 } else { 3 };
                let mut panels = vec![(0, header)];
                if h >= 32 {
                    panels.push((header, 3));
                }
                if tab == 0 {
                    panels.extend([(body, 6), (body + 6, 5), (body + 11, h - body - 12)]);
                } else {
                    panels.push((body, h - body - 1));
                }
                for (y, height) in panels {
                    assert!(
                        (2..w - 2).any(|x| b[(x, y + 1)].symbol() != " "),
                        "unwanted blank top row: {text}"
                    );
                    for row in y + 1..y + height - 1 {
                        assert_eq!(b[(1, row)].symbol(), " ", "left padding: {text}");
                        assert_eq!(b[(w - 2, row)].symbol(), " ", "right padding: {text}");
                    }
                }
            }
        }
        let data = Data {
            ledger: ledger.view(ui.query.date, 1, None, 0, 1).unwrap(),
            ..Default::default()
        };
        ui.detail = true;
        let text = buffer_text(&draw_buffer(&ui, &data, 110, 34));
        assert!(text.contains(ui.text("Actual cost", "实际成本")));
        ui.detail = false;
        ui.date_input = Some("2026-02-30".into());
        ui.key(KeyCode::Enter, &data);
        let text = buffer_text(&draw_buffer(&ui, &data, 110, 34));
        assert!(
            text.contains(ui.text("Enter a valid YYYY-MM-DD date", "请输入有效日期")),
            "{text}"
        );
        ui.date_input = None;
    }
    assert_eq!(
        original,
        fs::read(&f.path).unwrap(),
        "Rendering and switching must never write the journal"
    );
}

#[test]
fn status_grid_wraps_diagnostics_and_keeps_value_column_aligned_in_both_languages() {
    let now = chrono::Utc::now().timestamp_millis();
    let mut data = Data::default();
    data.ledger.ready = true;
    data.ledger.loading = false;
    data.live = Some(Snapshot {
        operator: None,
        version: 1,
        pid: 4321,
        run_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
        journal: "j".into(),
        started_ms: now - 5000,
        captured_ms: now,
        loop_ms: now,
        scan_ms: Some(now),
        settlement_ms: Some(now),
        target_window: now / 300_000 * 300,
        phase: "监控窗口".into(),
        scans: 12345,
        market_ws_connected: true,
        feed_max_age_ms: 5000,
        book_max_age_ms: 1000,
        rest_calls: 10,
        rest_successes: 9,
        rest_total_ms: 251,
        ws_reconnects: 3,
        quotes: vec![],
    });
    for language in [Language::English, Language::Chinese] {
        let mut ui = Ui::new(query(), false);
        ui.language = language;
        ui.tab = 2;
        let b = draw_buffer(&ui, &data, 110, 70);
        let text = buffer_text(&b);
        assert!(text.contains("01234567-89ab-cdef-0123-456789abcdef"));
        assert!(text.contains(ui.text("Monitoring window", "监控窗口")));
        assert!(text.contains(ui.text("Not collected", "未采集")));
        let header_y = text
            .lines()
            .position(|l| l.contains(ui.text("Field", "项目")))
            .unwrap() as u16;
        let value_x = (0..110)
            .find(|x| b[(*x, header_y)].symbol() == ui.text("V", "数"))
            .unwrap();
        for label in ["PID", "Run ID", ui.text("Scans", "扫描次数")] {
            let y = text.lines().position(|l| l.contains(label)).unwrap() as u16;
            assert_ne!(b[(value_x, y)].symbol(), " ", "{text}");
        }
        data.live_error = Some(format!("未连接策略快照服务: {} END", "x".repeat(160)));
        ui.status_scroll = 200;
        let text = buffer_text(&draw_buffer(&ui, &data, 80, 24));
        assert!(
            text.contains("END"),
            "Wrapped diagnostic tail must be reachable: {text}"
        );
        assert!(text.contains(ui.text("Date scope", "日期范围")));
        let maximum = ui.status_scroll_max.get();
        ui.key(KeyCode::Down, &data);
        assert_eq!(ui.status_scroll, maximum);
        ui.key(KeyCode::Up, &data);
        assert_eq!(ui.status_scroll, maximum.saturating_sub(1));
        data.live_error = None;
    }
}

#[test]
fn funding_key_requires_operator_confirmation_validates_amount_and_blocks_old_engine() {
    let mut data = operator_data();
    let mut ui = Ui::new(query(), false);
    ui.key(KeyCode::Char('f'), &data);
    assert!(ui.control_dialog.is_none());
    ui.key(KeyCode::Char('o'), &data);
    ui.key(KeyCode::Char('f'), &data);
    assert!(ui.outgoing.is_none());
    assert!(matches!(
        ui.control_dialog.as_ref().unwrap().action,
        control::Action::Deposit { .. }
    ));
    for c in "0.001".chars() {
        ui.key(KeyCode::Char(c), &data);
    }
    ui.key(KeyCode::Enter, &data);
    assert!(ui.outgoing.is_none());
    assert!(ui.control_dialog.is_some());
    for lang in [Language::English, Language::Chinese] {
        ui.language = lang;
        for (w, h) in [(80, 24), (110, 34), (146, 37)] {
            let text = buffer_text(&draw_buffer(&ui, &data, w, h));
            assert!(
                text.contains(ui.text("Add PAPER cash", "模拟入金")),
                "{text}"
            );
            assert!(
                text.contains(ui.text("Enter Confirm", "Enter确认")),
                "{text}"
            );
        }
    }
    ui.key(KeyCode::Esc, &data);
    ui.key(KeyCode::Char('f'), &data);
    for c in "500".chars() {
        ui.key(KeyCode::Char(c), &data);
    }
    ui.key(KeyCode::Enter, &data);
    assert!(
        matches!(ui.outgoing.take().unwrap().action,control::Action::Deposit{amount} if amount==D::from(500))
    );
    ui.command_busy = false;
    data.live
        .as_mut()
        .unwrap()
        .operator
        .as_mut()
        .unwrap()
        .funding_supported = false;
    ui.key(KeyCode::Char('f'), &data);
    assert!(ui.control_dialog.is_none());
}
