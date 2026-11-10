use super::ledger::Trade;
use super::{age, health, Data, Ui};
use chrono::{DateTime, Utc};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};
use rust_decimal::Decimal as D;

const BORDER: Color = Color::DarkGray;
fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_style(Style::default().fg(BORDER))
        .title(title)
}
fn fmt(v: D, n: usize) -> String {
    format!("{:.n$}", v.round_dp(n as u32))
}
fn money(v: D) -> String {
    format!("{:+.4}", v.round_dp(4))
}
fn date(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|x| x.format("%m/%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "--".into())
}
fn pnl(v: D, pulse: bool) -> Style {
    let s = Style::default().fg(if v > D::ZERO {
        Color::Green
    } else if v < D::ZERO {
        Color::LightRed
    } else {
        Color::Gray
    });
    if pulse {
        s.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        s
    }
}
fn right(s: impl Into<String>) -> Cell<'static> {
    Cell::from(Text::from(s.into()).alignment(Alignment::Right))
}
fn pc(v: D, yes: bool) -> Cell<'static> {
    right(money(v)).style(pnl(v, yes))
}
fn status(t: &Trade, ui: &Ui) -> &'static str {
    match t.outcome.as_str() {
        "unknown" => ui.text("Unknown", "执行未知"),
        "error" => ui.text("Error", "执行错误"),
        "no_fill" => ui.text("No fill", "未成交"),
        _ => match t.pnl {
            None => ui.text("Pending", "待结算"),
            Some(p) if p < D::ZERO => ui.text("Loss", "亏损"),
            Some(_) => ui.text("Win", "盈利"),
        },
    }
}

// Terminal rows are taller than character cells are wide: keep a one-column
// horizontal inset, without reserving additional blank rows above/below content.
const PROFITS_HEIGHT: u16 = 6; // borders + heading + BTC/ETH/total
const MARKETS_HEIGHT: u16 = 5; // borders + heading + BTC/ETH
const HEALTH_HEIGHT: u16 = 3;
pub(super) fn header_height(height: u16, operator: bool, message: bool) -> u16 {
    if height < 32 {
        5
    } else {
        3 + u16::from(operator) + u16::from(message)
    }
}
pub(super) fn trade_capacity(height: u16, tab: usize, header: u16) -> usize {
    let chrome = header + 1 + if height < 32 { 0 } else { HEALTH_HEIGHT };
    let other_panels = if tab == 0 {
        PROFITS_HEIGHT + MARKETS_HEIGHT
    } else {
        0
    };
    height
        .saturating_sub(chrome + other_panels + 3)
        .clamp(1, 40) as usize
}

pub(super) fn footer(ui: &Ui, width: u16) -> String {
    let action = if ui.detail || ui.tab == 2 {
        ui.text("Scroll", "滚动")
    } else {
        ui.text("Select", "选择")
    };
    let language = ui.text("L 中文", "L English");
    let exit = ui.text("Q Exit", "Q退出");
    let candidates = [
        format!(
            "{} | {} | {} | ↑↓ {action} PgUp/PgDn | {} | {language} | {exit}",
            ui.text("Tab/1-3 Pages", "Tab/1-3切页"),
            ui.text("[ ] Day D Date T Today", "[ ]日期 D输入 T今天"),
            ui.text("A Asset R Arm", "A资产 R轨道"),
            ui.text("Enter Details Esc Back", "Enter详情 Esc返回")
        ),
        format!(
            "{} {} {} ↑↓ {action} {} {} {language} {exit}",
            ui.text("Tab/1-3 Pages", "Tab/1-3切页"),
            ui.text("[] Day D Date T Today", "[]日期 D输入 T今天"),
            ui.text("A Asset R Arm", "A资产 R轨道"),
            ui.text("Pg Page", "Pg翻页"),
            ui.text("Enter Details Esc Back", "Enter详情 Esc返回")
        ),
        format!(
            "{} | {} | {} | ↑↓ Pg | Enter Esc | {language} | {exit}",
            ui.text("Tab/1-3 Pages", "Tab/1-3切页"),
            ui.text("[] D T Date", "[]日期 D输入 T今天"),
            ui.text("A Asset R Arm", "A资产 R轨道")
        ),
        format!("Tab/1-3 [] D T A R O/P/E/F ↑↓ Pg Enter Esc | {language} | {exit}"),
    ];
    candidates
        .into_iter()
        .find(|s| Text::from(s.as_str()).width() <= width as usize)
        .unwrap()
}

pub(super) fn draw(frame: &mut Frame, ui: &Ui, data: &Data) {
    let area = frame.area();
    let utc_now = Utc::now();
    let clock = utc_now.format("%Y-%m-%d %H:%M:%S UTC");
    if area.width < 80 || area.height < 24 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(ui.text(
                    "Late Resolution Winner · Read-only viewer",
                    "Late Resolution Winner · 只读查看器",
                )),
                Line::from(format!(
                    "{} {}×{} | {} 80×24",
                    ui.text("Size", "当前"),
                    area.width,
                    area.height,
                    ui.text("Minimum", "至少")
                )),
                Line::from(clock.to_string()),
                Line::from(ui.text("L 中文 | Q Exit", "L English | Q退出")),
            ])
            .block(block(ui.text(" Terminal too small ", " 终端尺寸不足 "))),
            area,
        );
        return;
    }
    let compact = area.height < 32;
    let header_height = header_height(
        area.height,
        data.live.as_ref().is_some_and(|s| s.operator.is_some()),
        !ui.message.is_empty(),
    );
    let sections = Layout::vertical(if compact {
        vec![
            Constraint::Length(header_height),
            Constraint::Min(15),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(header_height),
            Constraint::Length(HEALTH_HEIGHT),
            Constraint::Min(15),
            Constraint::Length(1),
        ]
    })
    .split(area);
    let body_area = sections[if compact { 1 } else { 2 }];
    let arm = if ui.query.arm == 2 {
        ui.text("Operator ETH≤0.97", "Operator ETH≤0.97")
    } else if ui.query.arm == 1 {
        ui.text("Candidate ETH≤0.97", "候选 ETH≤0.97")
    } else {
        ui.text("Control ETH≤0.98", "对照 ETH≤0.98")
    };
    let tabs = [
        ui.text("1 Overview", "1 总览"),
        ui.text("2 Trades", "2 交易"),
        ui.text("3 Status", "3 状态"),
    ]
    .into_iter()
    .enumerate()
    .flat_map(|(i, t)| {
        [
            Span::styled(
                format!("{t} "),
                if ui.tab == i {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
            Span::raw(" "),
        ]
    })
    .collect::<Vec<_>>();
    let mut header = tabs;
    header.push(Span::styled(
        format!(
            "PAPER | {arm} | {}",
            if ui.query.arm == 2 {
                ui.text("O/P/E/F Control", "O/P/E/F控制")
            } else {
                ui.text("Read-only", "只读")
            }
        ),
        Style::default().fg(Color::Cyan),
    ));
    let now = utc_now.timestamp_millis();
    let (message, bad) = health(data, now, ui.language);
    let health_style = Style::default().fg(if bad { Color::LightRed } else { Color::Green });
    let scan = data.live.as_ref().and_then(|s| s.scan_ms);
    let settle = data.live.as_ref().and_then(|s| s.settlement_ms);
    let ages = format!(
        "{} {} | {} {}",
        ui.text("Scan", "扫描"),
        age(now, scan, ui.language),
        ui.text("Settlement check", "结算检查"),
        age(now, settle, ui.language)
    );
    let controls = data
        .live
        .as_ref()
        .and_then(|s| s.operator.as_ref())
        .map(|o| {
            let state = if data.live_error.is_some()
                || now.saturating_sub(data.live.as_ref().unwrap().captured_ms) > 3000
            {
                ui.text("UNKNOWN", "状态未知")
            } else if o.failed {
                ui.text("FAILED", "故障")
            } else if o.settings.paused {
                ui.text("PAUSED", "已暂停")
            } else {
                ui.text("RUNNING", "运行中")
            };
            let pending = o
                .settings
                .pending_budget
                .map(|v| {
                    format!(
                        " ->{v}@{}",
                        date(o.settings.effective_window_ms.unwrap_or(0))
                    )
                })
                .unwrap_or_default();
            let options = [
                format!(
                    "Operator {state} | ${}{} | {} {} | {} {} | v{} | O/P/E/F",
                    o.settings.budget,
                    pending,
                    ui.text("Cash", "现金"),
                    fmt(o.cash, 4),
                    ui.text("Capital", "本金"),
                    fmt(o.capital, 2),
                    o.settings.revision
                ),
                format!(
                    "OP {state} ${}{} | {} {} | {} {} | v{}",
                    o.settings.budget,
                    pending,
                    ui.text("Cash", "现金"),
                    fmt(o.cash, 2),
                    ui.text("Capital", "本金"),
                    fmt(o.capital, 2),
                    o.settings.revision
                ),
                format!(
                    "OP {state} ${} | {} {} | {} {}",
                    o.settings.budget,
                    ui.text("Cash", "现金"),
                    fmt(o.cash, 2),
                    ui.text("Capital", "本金"),
                    fmt(o.capital, 2)
                ),
            ];
            options
                .into_iter()
                .find(|s| Text::from(s.as_str()).width() <= area.width.saturating_sub(4) as usize)
                .unwrap_or_else(|| {
                    format!(
                        "Operator {state} | ${} | Cash {}",
                        o.settings.budget,
                        fmt(o.cash, 2)
                    )
                })
        });
    let mut header_lines = vec![Line::from(header)];
    if compact {
        header_lines.push(Line::styled(message.clone(), health_style));
        header_lines.push(Line::from(if !ui.message.is_empty() {
            ui.message.clone()
        } else {
            controls.clone().unwrap_or_else(|| ages.clone())
        }));
    } else {
        if let Some(controls) = controls {
            header_lines.push(Line::from(controls));
        }
        if !ui.message.is_empty() {
            header_lines.push(Line::from(ui.message.clone()));
        }
    }
    frame.render_widget(
        Paragraph::new(header_lines)
            .block(block(format!(" LATE RESOLUTION WINNER TRADING | {clock} "))),
        sections[0],
    );
    if !compact {
        frame.render_widget(
            Paragraph::new(message)
                .style(health_style)
                .block(block(format!(
                    " {} | {ages} ",
                    ui.text("Health", "运行健康")
                ))),
            sections[1],
        );
    }
    match ui.tab {
        0 => {
            let body = Layout::vertical([
                Constraint::Length(PROFITS_HEIGHT),
                Constraint::Length(MARKETS_HEIGHT),
                Constraint::Min(4),
            ])
            .split(body_area);
            profits(frame, body[0], ui, data);
            markets(frame, body[1], ui, data, now);
            trades(frame, body[2], ui, data);
        }
        1 => trades(frame, body_area, ui, data),
        _ => runtime(frame, body_area, ui, data, now),
    }
    frame.render_widget(
        Paragraph::new(footer(ui, area.width)).style(Style::default().fg(Color::DarkGray)),
        *sections.last().unwrap(),
    );
    if ui.detail {
        if let Some(t) = data.ledger.rows.get(ui.selected) {
            details(frame, ui, t, body_area);
        }
    }
    if let Some(dialog) = &ui.control_dialog {
        let popup = Rect {
            x: area.x + (area.width - 74) / 2,
            y: area.y + (area.height - 10) / 2,
            width: 74,
            height: 10,
        };
        let funding = matches!(dialog.action, super::control::Action::Deposit { .. });
        let action = match dialog.action {
            super::control::Action::Pause => ui
                .text(
                    "Pause NEW Operator PAPER entries",
                    "暂停Operator模拟轨道的新入场",
                )
                .to_string(),
            super::control::Action::Resume => ui
                .text(
                    "Resume Operator PAPER entries",
                    "恢复Operator模拟轨道的新入场",
                )
                .to_string(),
            super::control::Action::Deposit { .. } => format!(
                "{}: {}_",
                ui.text(
                    "Add PAPER cash (0.01–1,000,000.00 USDC)",
                    "模拟入金（0.01–1,000,000.00 USDC）"
                ),
                dialog.input
            ),
            super::control::Action::SetBudget { .. } => format!(
                "{}: {}_",
                ui.text("New budget USDC (1.00–100.00)", "新额度USDC（1.00–100.00）"),
                dialog.input
            ),
        };
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(action, Style::default().fg(Color::Yellow)),
                Line::from(ui.text(
                    "Only Operator PAPER. Frozen research arms keep running.",
                    "仅控制Operator模拟轨道，冻结研究轨道继续运行。",
                )),
                Line::from(ui.text(
                    "Collection and existing settlements continue.",
                    "行情采集、评估及已有成交的结算继续。",
                )),
                Line::from(if funding {
                    ui.text(
                        "Immediate after ACK; trade PnL, budget and pause unchanged.",
                        "回执后立即入账，交易盈亏、额度和暂停状态保持原值。",
                    )
                } else {
                    ui.text(
                        "Budget changes apply at the next 5-minute boundary.",
                        "额度修改于下一个五分钟窗口起点生效。",
                    )
                }),
                Line::from(format!(
                    "{} v{}",
                    ui.text("Expected revision", "预期版本"),
                    dialog.revision
                )),
                Line::from(ui.text(
                    "Enter Confirm | Esc Cancel | Backspace Edit",
                    "Enter确认 | Esc取消 | Backspace修改",
                )),
                Line::from(ui.message.clone()),
            ])
            .wrap(Wrap { trim: false })
            .block(block(
                ui.text(" Operator PAPER control ", " Operator模拟控制 "),
            )),
            popup,
        );
    }
    if let Some(input) = &ui.date_input {
        let popup = Rect {
            x: area.x + (area.width - 68) / 2,
            y: area.y + (area.height - 7) / 2,
            width: 68,
            height: 7,
        };
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("{}: {input}_", ui.text("Date", "日期"))),
                Line::from(ui.text(
                    "YYYY-MM-DD | Backspace Edit | Enter Apply | Esc Cancel",
                    "YYYY-MM-DD | Backspace修改 | Enter确认 | Esc取消",
                )),
                Line::from(ui.language.diagnostic(&ui.message)),
            ])
            .block(block(ui.text(
                " Ledger date (UTC) · market / health stay live ",
                " 选择账本日期（UTC） · 行情与运行状态仍为最新 ",
            )))
            .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

fn profits(frame: &mut Frame, area: Rect, ui: &Ui, data: &Data) {
    let head = Row::new([
        Cell::from(ui.text("Asset", "资产")),
        right(ui.text("Day PnL", "当日已结算")),
        right(ui.text("Total PnL", "策略累计")),
        right(ui.text("Day W/L", "当日胜/负")),
        right(ui.text("No fill", "未成交")),
        right(ui.text("Exposure", "在途成本")),
    ])
    .style(Style::default().fg(Color::Cyan));
    let rows = (0..3).map(|i| {
        let d = &data.ledger.daily[i];
        let t = &data.ledger.total[i];
        let known = data.ledger.ready && !data.ledger.loading;
        Row::new(vec![
            Cell::from(["BTC", "ETH", ui.text("Total", "合计")][i]),
            if known {
                pc(d.pnl, ui.pulse(&format!("day{i}")))
            } else {
                right(ui.text("Loading", "加载中"))
            },
            if known {
                pc(t.pnl, ui.pulse(&format!("total{i}")))
            } else {
                right("--")
            },
            right(format!("{}/{}", d.wins, d.losses)),
            right(d.no_fills.to_string()),
            right(fmt(t.exposure + t.reserved, 4)),
        ])
    });
    let title = format!(" {} {} UTC · USDC ", ui.text("PnL", "盈亏"), ui.query.date);
    let foot = format!(
        " {} {} - {}{} ",
        ui.text("Cumulative period", "累计时间"),
        date(data.ledger.start_ms),
        date(data.ledger.as_of_ms),
        if data.ledger.partial {
            ui.text(" | Partial tail", " | 尾行写入中")
        } else {
            ""
        }
    );
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Fill(1),
                Constraint::Fill(1),
                Constraint::Length(9),
                Constraint::Length(7),
                Constraint::Length(11),
            ],
        )
        .header(head)
        .column_spacing(1)
        .block(block(title).title_bottom(foot)),
        area,
    );
}
fn markets(frame: &mut Frame, area: Rect, ui: &Ui, data: &Data, now: i64) {
    let current = now / 300_000 * 300;
    let live = data
        .live
        .as_ref()
        .filter(|s| data.live_error.is_none() && now - s.captured_ms <= 3500);
    let rows = (0..2).map(|i| {
        let asset = ["btc", "eth"][i];
        let q = live.and_then(|s| {
            s.quotes
                .iter()
                .find(|q| q.asset == asset && q.window_start == current)
        });
        let fresh = q.zip(live).is_some_and(|(q, s)| {
            q.reference_ms
                .is_some_and(|t| t <= now + 1000 && now - t <= s.feed_max_age_ms)
        });
        let twap = q.filter(|_| fresh).and_then(|q| q.twap);
        let delta = q
            .filter(|_| fresh)
            .and_then(|q| q.open.zip(q.twap))
            .filter(|(a, _)| *a > D::ZERO)
            .map(|(o, t)| (t - o) / o * D::from(10000));
        let book = |up: bool| {
            q.zip(live)
                .and_then(|(q, s)| {
                    let ts = if up { q.up_ms } else { q.down_ms };
                    ts.filter(|at| *at <= now + 1000 && now - *at <= s.book_max_age_ms)
                        .and(if up { q.up_ask } else { q.down_ask })
                })
                .map(|v| fmt(v, 3))
                .unwrap_or_else(|| "--".into())
        };
        let observation = data.ledger.observations[i].as_ref().filter(|o| {
            o.slug
                .rsplit('-')
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                == Some(current)
        });
        let decision = observation.and_then(|o| {
            o.decisions
                .iter()
                .find(|d| d.arm == super::ledger::ARMS[ui.query.arm.min(1)])
        });
        let reason = if live.is_none() {
            ui.text("Offline", "未连接").into()
        } else if !fresh {
            ui.text("Stale/missing ref", "参考陈旧/缺失").into()
        } else if let Some(d) = decision {
            // A historical confirmation is never presented as current after stale/reset conditions.
            if now - observation.unwrap().at.timestamp_millis() > 10_000 {
                ui.text("Awaiting evaluation", "等待新评估").into()
            } else {
                match d.reason.as_str() {
                    "awaiting_confirmation" => {
                        format!("{}{}/2", ui.text("Confirm ", "确认"), d.confirmations)
                    }
                    "eth_ask_above_cap" => ui.text("Ask above cap", "报价超限").into(),
                    "already_attempted" => ui.text("Attempted", "本窗已尝试").into(),
                    "attempt" => ui.text("Triggered", "已触发尝试").into(),
                    "invalid_reference_or_market" => {
                        ui.text("Ref/book rejected", "参考/盘口拒绝").into()
                    }
                    "insufficient_paper_cash" => ui.text("Low PAPER cash", "模拟资金不足").into(),
                    "outside_entry_window" => ui.text("Outside entry window", "入场时段外").into(),
                    "outside_control_quote_range" => {
                        ui.text("Outside ask range", "报价范围外").into()
                    }
                    "invalid_book_quality" => ui.text("Book rejected", "盘口拒绝").into(),
                    "unsupported_asset" => ui.text("Unsupported asset", "不支持的资产").into(),
                    _ => d.reason.clone(),
                }
            }
        } else if now / 1000 - current < 240 {
            ui.text("Wait for last 60s", "等待最后60秒").into()
        } else {
            ui.text("Awaiting signal", "等待合格信号").into()
        };
        let mut price = right(twap.map(|v| fmt(v, 2)).unwrap_or_else(|| "--".into()));
        if ui.pulse(&format!("twap{asset}")) {
            price = price.style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        }
        Row::new(vec![
            Cell::from(asset.to_ascii_uppercase()),
            price,
            right(
                delta
                    .map(|v| format!("{v:+.2}"))
                    .unwrap_or_else(|| "--".into()),
            ),
            right(book(true)),
            right(book(false)),
            right(age(now, q.and_then(|q| q.reference_ms), ui.language)),
            Cell::from(reason),
        ])
    });
    let start = DateTime::from_timestamp(current, 0).unwrap();
    let end = DateTime::from_timestamp(current + 300, 0).unwrap();
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(5),
                Constraint::Length(11),
                Constraint::Length(8),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Min(16),
            ],
        )
        .column_spacing(1)
        .header(
            Row::new(vec![
                Cell::from(ui.text("Asset", "资产")),
                right("TWAP / USD"),
                right(ui.text("Δ bps", "变化bps")),
                right("Up ask"),
                right("Dn ask"),
                right(ui.text("Ref age", "参考龄")),
                Cell::from(ui.text("Latest evaluation", "最近本窗评估")),
            ])
            .style(Style::default().fg(Color::Cyan)),
        )
        .block(block(format!(
            " {} {}–{} UTC · {} {}s | {} ",
            ui.text("Live window", "最新窗口"),
            start.format("%H:%M"),
            end.format("%H:%M"),
            ui.text("Left", "剩余"),
            300 - now / 1000 % 300,
            ui.text("USDC/share", "合约价USDC/份")
        ))),
        area,
    );
}
fn trades(frame: &mut Frame, area: Rect, ui: &Ui, data: &Data) {
    let compact = area.width < 100;
    let rows = data.ledger.rows.iter().map(|t| {
        let direction = if t.direction { "↑" } else { "↓" };
        let mut cells = vec![
            Cell::from(if t.outcome == "unknown" {
                "--:--:--".into()
            } else {
                t.at.format("%H:%M:%S").to_string()
            }),
            Cell::from(format!("{} {direction}", t.asset)),
        ];
        if !compact {
            cells.push(right(fmt(t.ask, 3)));
        }
        cells.extend([
            right(fmt(t.limit, 3)),
            right(t.average.map(|v| fmt(v, 6)).unwrap_or_else(|| "--".into())),
            right(if t.outcome == "unknown" {
                "--".into()
            } else {
                fmt(t.cost, 4)
            }),
            Cell::from(status(t, ui)),
        ]);
        cells.push(match t.pnl {
            Some(v) => pc(v, false),
            None => right("--"),
        });
        Row::new(cells)
    });
    let mut widths = vec![Constraint::Length(8), Constraint::Length(7)];
    if !compact {
        widths.push(Constraint::Length(7));
    }
    widths.extend([
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Min(10),
    ]);
    let mut labels = vec![ui.text("Time", "执行时间"), ui.text("Asset", "资产/向")];
    if !compact {
        labels.push(ui.text("Ask", "信号ask"));
    }
    labels.extend([
        ui.text("Limit", "限价"),
        ui.text("Avg+fee", "含费均价"),
        ui.text("Cost USDC", "投入USDC"),
        ui.text("Status", "状态"),
        ui.text("PnL USDC", "净盈亏USDC"),
    ]);
    let asset = match ui.query.asset {
        Some(0) => "BTC",
        Some(1) => "ETH",
        _ => "BTC+ETH",
    };
    let title = format!(
        " {} UTC {} · {} · {} {}/{} · {} {} ",
        ui.query.date,
        ui.text("Trades", "交易"),
        asset,
        ui.text("Page", "页"),
        data.ledger.page + 1,
        data.ledger.row_count.div_ceil(ui.query.limit).max(1),
        data.ledger.row_count,
        ui.text("attempts", "次尝试")
    );
    let table = Table::new(rows, widths)
        .header(
            Row::new(labels.into_iter().enumerate().map(|(i, label)| {
                if i >= 2 && i != (if compact { 5 } else { 6 }) {
                    right(label)
                } else {
                    Cell::from(label)
                }
            }))
            .style(Style::default().fg(Color::Cyan)),
        )
        .column_spacing(1)
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("› ")
        .block(block(title));
    let mut state =
        TableState::default().with_selected((!data.ledger.rows.is_empty()).then_some(ui.selected));
    frame.render_stateful_widget(table, area, &mut state);
    if data.ledger.rows.is_empty() {
        let inner = block("").inner(area);
        let inner = Rect {
            y: inner.y.saturating_add(1),
            height: inner.height.saturating_sub(1),
            ..inner
        };
        frame.render_widget(
            Paragraph::new(if data.ledger.loading {
                ui.text("Loading ledger…", "账本加载中…")
            } else {
                ui.text(
                    "No attempts for the selected date / asset",
                    "所选日期 / 资产没有交易尝试",
                )
            })
            .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
}
fn details(frame: &mut Frame, ui: &Ui, t: &Trade, area: Rect) {
    frame.render_widget(Clear, area);
    let e = &t.evaluation;
    let unknown = ui.text("Unknown", "未知");
    let lines = vec![
        Line::from(format!("{} {}", ui.text("Market", "市场"), t.slug)),
        Line::from(format!("{} {}", ui.text("Attempt", "尝试"), t.id)),
        Line::from(format!(
            "{} {} | {} {} USDC",
            ui.text("Parameter revision", "参数版本"),
            t.parameter_revision
                .map(|v| v.to_string())
                .unwrap_or_else(|| "frozen v1".into()),
            ui.text("Budget cap", "预算上限"),
            t.budget_limit.unwrap_or(D::from(5))
        )),
        Line::from(format!(
            "{} {} | {} {} | {} {} | {} {}",
            ui.text("Direction", "方向"),
            if t.direction {
                ui.text("Up", "Up 上涨")
            } else {
                ui.text("Down", "Down 下跌")
            },
            ui.text("Status", "状态"),
            status(t, ui),
            ui.text("Signal ask", "信号ask"),
            t.ask,
            ui.text("Limit", "限价"),
            t.limit
        )),
        Line::from(format!(
            "{} {} | {} {} | {} {} USDC",
            ui.text("Filled shares", "成交份数"),
            t.filled,
            ui.text("Avg incl. fee", "含费均价"),
            t.average
                .map(|v| v.to_string())
                .unwrap_or_else(|| "--".into()),
            ui.text("Actual cost", "实际成本"),
            if t.outcome == "unknown" {
                unknown.into()
            } else {
                t.cost.to_string()
            }
        )),
        Line::from(format!(
            "{} {} USDC | {} {} ({})",
            ui.text("Reserved cost", "预留成本"),
            t.reserved,
            ui.text("Fee", "费用"),
            if t.outcome == "unknown" {
                unknown.into()
            } else {
                t.fee.to_string()
            },
            ui.text("already included in known cost", "已含在已知成本中")
        )),
        Line::from(format!(
            "{} {} | {} {}",
            ui.text("Signal", "信号"),
            e.evaluated_at.format("%Y-%m-%d %H:%M:%S%.3f"),
            ui.text("Execution", "执行"),
            if t.outcome == "unknown" {
                unknown.into()
            } else {
                t.at.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
            }
        )),
        Line::from(format!(
            "{} {} | {} {}",
            ui.text("Settled", "结算"),
            t.settled_at
                .map(|t| t.format("%m/%d %H:%M:%S").to_string())
                .unwrap_or_else(|| ui.text("Pending", "尚未结算").into()),
            ui.text("Net PnL", "净盈亏"),
            t.pnl.map(money).unwrap_or_else(|| "--".into())
        )),
        Line::from(format!(
            "{} {} ({}) | signed r {:.6}",
            ui.text("Model p", "模型 p"),
            e.model_probability,
            ui.text(
                "uncalibrated; not actual win probability",
                "未校准，不是真实胜率"
            ),
            e.reversal_margin_ratio
        )),
        Line::from(format!(
            "{} {}s | {} {}ms | {} {} USDC",
            ui.text("Remaining", "剩余"),
            e.seconds_remaining,
            ui.text("Book age", "盘口年龄"),
            e.book_age_ms,
            ui.text("Depth", "深度"),
            e.depth_usdc
        )),
        Line::from(format!(
            "{} {}ms | {} {}ms ({})",
            ui.text("Request", "请求"),
            t.request_ms
                .map(|v| v.to_string())
                .unwrap_or_else(|| "--".into()),
            ui.text("Total execution", "总执行"),
            t.total_ms
                .map(|v| v.to_string())
                .unwrap_or_else(|| "--".into()),
            ui.text("includes simulation delay", "含模拟延迟")
        )),
        Line::from(format!(
            "{} {:?} | run {}",
            ui.text("Checks", "检查"),
            e.failed_checks,
            e.run_id
        )),
        Line::from(format!(
            "{} {}",
            ui.text("Execution diagnostic", "执行诊断"),
            ui.language
                .diagnostic(t.execution_error.as_deref().unwrap_or("--"))
        )),
        Line::from(ui.text(
            "Confirmation: 2 distinct TWAPs required; full sequence is in observed records",
            "确认：研究尝试要求2个不同TWAP；完整确认过程见原始observed记录",
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((ui.detail_scroll, 0))
            .block(block(ui.text(
                " Trade details · UTC · original precision ",
                " 交易详情 · UTC · 金额保持原始精度 ",
            ))),
        area,
    );
}

/// Wrap by terminal cells so Chinese labels and long diagnostics occupy the
/// same grid as English. Table cells do not wrap automatically in ratatui.
fn wrap_value(value: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in value.split('\n') {
        let mut part = String::new();
        let mut used = 0;
        for c in line.chars() {
            let size = Text::from(c.to_string()).width();
            if used + size > width && !part.is_empty() {
                result.push(std::mem::take(&mut part));
                used = 0;
            }
            part.push(c);
            used += size;
        }
        result.push(part);
    }
    result
}

fn runtime(frame: &mut Frame, area: Rect, ui: &Ui, data: &Data, now: i64) {
    let normal = Style::default().fg(Color::Gray);
    let section = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let error = Style::default().fg(Color::LightRed);
    let mut fields: Vec<(String, String, Style)> = Vec::new();
    let mut add =
        |label: &str, value: String, style: Style| fields.push((label.into(), value, style));
    add(
        ui.text("Ledger & funds", "账本与资金"),
        String::new(),
        section,
    );
    add(
        ui.text("Ledger", "账本状态"),
        if data.ledger.loading {
            ui.text("Loading", "加载中")
        } else if data.ledger.error.is_some() {
            ui.text("Validation failed", "验证失败")
        } else {
            ui.text("Loaded", "已载入")
        }
        .into(),
        normal,
    );
    add(
        ui.text("Runs", "运行记录"),
        data.ledger.run_count.to_string(),
        normal,
    );
    add(
        ui.text("Partial tail", "尾行写入中"),
        ui.text(
            if data.ledger.partial { "Yes" } else { "No" },
            if data.ledger.partial { "是" } else { "否" },
        )
        .into(),
        normal,
    );
    add(
        ui.text("Results through", "结果截至"),
        date(data.ledger.as_of_ms),
        normal,
    );
    let total = &data.ledger.total[2];
    add(
        ui.text("Pending fills", "待结算成交"),
        total.pending.to_string(),
        normal,
    );
    add(
        ui.text("Unknown executions", "未知执行"),
        total.unknown.to_string(),
        normal,
    );
    add(
        ui.text("Reserved funds", "预留资金"),
        format!("{} USDC", total.reserved),
        normal,
    );
    add(
        ui.text("Pending cost", "在途成本"),
        format!("{} USDC", total.exposure),
        normal,
    );
    add(
        ui.text("Oldest pending age", "最老在途年龄"),
        age(now, data.ledger.oldest_pending_ms, ui.language),
        normal,
    );
    add(
        ui.text("PAPER cash", "可用模拟现金"),
        format!(
            "{} USDC",
            data.ledger.capital + total.pnl - total.exposure - total.reserved
        ),
        normal,
    );
    add(
        ui.text("Contributed capital", "累计模拟本金"),
        format!("{} USDC", data.ledger.capital),
        normal,
    );
    add(
        ui.text("Added PAPER cash", "追加模拟入金"),
        format!("{} USDC", data.ledger.deposits),
        normal,
    );
    add(
        ui.text("Trading PnL (funding excluded)", "交易盈亏（不含入金）"),
        format!("{} USDC", total.pnl),
        normal,
    );
    add(
        ui.text("Settled cost", "已结算成本"),
        format!("{} USDC", total.cost),
        normal,
    );
    add(
        ui.text("Order budget", "单笔预算上限"),
        format!("{} USDC", data.ledger.budget),
        normal,
    );
    if let Some(s) = &data.live {
        add(ui.text("Engine", "策略进程"), String::new(), section);
        add("PID", s.pid.to_string(), normal);
        add("Run ID", s.run_id.clone(), normal);
        add(
            ui.text("Run started", "本次运行开始"),
            date(s.started_ms),
            normal,
        );
        add(
            ui.text("Snapshot age", "快照年龄"),
            age(now, Some(s.captured_ms), ui.language),
            normal,
        );
        add(
            ui.text("Phase", "运行阶段"),
            ui.language.diagnostic(&s.phase),
            normal,
        );
        add(
            ui.text("Loop age", "循环年龄"),
            age(now, Some(s.loop_ms), ui.language),
            normal,
        );
        add(ui.text("Scans", "扫描次数"), s.scans.to_string(), normal);
        add(
            ui.text("Scan age", "扫描年龄"),
            age(now, s.scan_ms, ui.language),
            normal,
        );
        add(
            ui.text("Target window", "查询目标窗口"),
            s.target_window.to_string(),
            normal,
        );
        add(
            ui.text("Current window", "当前实际窗口"),
            (now / 300_000 * 300).to_string(),
            normal,
        );
        add(
            ui.text("Settlement check age", "结算检查年龄"),
            age(now, s.settlement_ms, ui.language),
            normal,
        );
        add(
            ui.text("Feed & requests", "行情与请求"),
            String::new(),
            section,
        );
        add(
            ui.text("Market WS", "市场WS"),
            if s.market_ws_connected {
                ui.text("Connected", "连接")
            } else {
                ui.text("Disconnected", "未连接")
            }
            .into(),
            normal,
        );
        add(
            ui.text("WS reconnects", "本次WS重连"),
            s.ws_reconnects.to_string(),
            normal,
        );
        add(
            ui.text("REST success / calls", "REST成功/请求"),
            format!("{} / {}", s.rest_successes, s.rest_calls),
            normal,
        );
        add(
            ui.text("REST mean", "REST平均耗时"),
            if s.rest_calls > 0 {
                format!("{:.2} ms", s.rest_total_ms as f64 / s.rest_calls as f64)
            } else {
                "--".into()
            },
            normal,
        );
        add(
            ui.text("REST p50 / p99", "REST p50/p99"),
            ui.text("Not collected", "未采集").into(),
            normal,
        );
        for q in &s.quotes {
            add(
                &format!("{} TWAP", q.asset.to_ascii_uppercase()),
                q.twap.map(|v| v.to_string()).unwrap_or_else(|| "--".into()),
                normal,
            );
            add(
                &format!(
                    "{} {}",
                    q.asset.to_ascii_uppercase(),
                    ui.text("reference age", "参考年龄")
                ),
                age(now, q.reference_ms, ui.language),
                normal,
            );
        }
        add(
            ui.text("Reference source", "参考源"),
            "crypto_prices_twap_sixty".into(),
            normal,
        );
    }
    if let Some(e) = &data.live_error {
        add(
            ui.text("Live snapshot", "实时快照"),
            ui.language.diagnostic(e),
            error,
        );
    }
    if let Some(e) = &data.ledger.error {
        add(
            ui.text("Ledger error", "账本错误"),
            ui.language.diagnostic(e),
            error,
        );
    }
    add(
        ui.text("Date scope", "日期范围"),
        ui.text(
            "UTC date filters the ledger; market and runtime data stay live",
            "UTC 日期只筛选账本；运行与行情始终为最新状态",
        )
        .into(),
        normal,
    );
    add(
        ui.text("Health scope", "健康口径"),
        ui.text(
            "A live process does not imply an advancing strategy",
            "进程存活不等于策略推进",
        )
        .into(),
        normal,
    );
    let panel = block(ui.text(" Status ", " 状态 "));
    let inner = panel.inner(area);
    let label_width = 24.min(inner.width / 2);
    let value_width = inner.width.saturating_sub(label_width + 2).max(1) as usize;
    let mut rows = Vec::new();
    for (label, value, style) in fields {
        for (i, part) in wrap_value(&value, value_width).into_iter().enumerate() {
            rows.push(
                Row::new([if i == 0 { label.clone() } else { String::new() }, part]).style(style),
            );
        }
    }
    let capacity = inner.height.saturating_sub(1) as usize;
    let max_offset = rows.len().saturating_sub(capacity).min(u16::MAX as usize);
    ui.status_scroll_max.set(max_offset as u16);
    let offset = (ui.status_scroll as usize).min(max_offset);
    frame.render_widget(
        Table::new(
            rows.into_iter().skip(offset),
            [Constraint::Length(label_width), Constraint::Min(1)],
        )
        .column_spacing(2)
        .header(Row::new([ui.text("Field", "项目"), ui.text("Value", "数值")]).style(section))
        .block(panel),
        area,
    );
}

#[cfg(test)]
mod display_tests {
    use super::*;
    #[test]
    fn decimal_display_rounds_instead_of_truncating_money() {
        assert_eq!(fmt(D::from_str_exact("4.99627018").unwrap(), 4), "4.9963");
        assert_eq!(money(D::from_str_exact("-0.046775448").unwrap()), "-0.0468");
        assert_eq!(money(D::from_str_exact("1.43429652").unwrap()), "+1.4343");
    }
}
