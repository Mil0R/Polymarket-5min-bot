use crate::ui::control::StrategyDisplayState;
use crate::ui::state::{DashboardState, HealthStatus, PriceDir};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Wrap},
    Frame,
};

fn border_style(selected: bool) -> Style {
    Style::default().fg(if selected {
        Color::Cyan
    } else {
        Color::Rgb(55, 60, 75)
    })
}

fn title_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn pnl_style(value: f64) -> Style {
    Style::default().fg(if value > 0.0 {
        Color::Green
    } else if value < 0.0 {
        Color::Red
    } else {
        Color::Gray
    })
}

fn state_label(state: StrategyDisplayState) -> (&'static str, Color) {
    match state {
        StrategyDisplayState::Running => ("RUNNING", Color::Green),
        StrategyDisplayState::Paused => ("PAUSED", Color::Yellow),
        StrategyDisplayState::Blocked => ("BLOCKED", Color::Red),
        StrategyDisplayState::Unavailable => ("UNAVAILABLE", Color::DarkGray),
    }
}

fn health_color(app: &DashboardState) -> Color {
    if app
        .services
        .iter()
        .any(|service| service.status == HealthStatus::Err)
    {
        Color::Red
    } else if app
        .services
        .iter()
        .any(|service| service.status == HealthStatus::Warn)
    {
        Color::Yellow
    } else {
        Color::Green
    }
}

pub fn draw(frame: &mut Frame, app: &DashboardState) {
    let area = frame.area();
    if area.width < 40 || area.height < 22 {
        draw_too_small(frame, area);
        return;
    }
    if area.width < 70 || area.height < 32 {
        draw_mobile(frame, area, app);
        return;
    }

    let desktop = area.width >= 105 && area.height >= 34;
    let market_height = if desktop {
        (app.markets.len() as u16 + 3).clamp(5, 9)
    } else {
        5
    };
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(market_height),
            Constraint::Min(if desktop { 14 } else { 15 }),
            Constraint::Length(if desktop { 7 } else { 5 }),
            Constraint::Length(3),
        ])
        .split(area);
    draw_header(frame, root[0], app);
    draw_markets(frame, root[1], app);
    if desktop {
        draw_desktop_strategies(frame, root[2], app);
    } else {
        draw_medium_strategies(frame, root[2], app);
    }
    draw_events(frame, root[3], app);
    draw_footer(frame, root[4], app, false);
}

fn draw_mobile(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(area);
    draw_mobile_header(frame, root[0], app);
    draw_mobile_markets(frame, root[1], app);
    draw_selected_strategy(frame, root[2], app);
    draw_events(frame, root[3], app);
    draw_footer(frame, root[4], app, true);
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("POLYPULSE · terminal too small"),
            Line::from(format!(
                "Current {}x{} · need 40x22",
                area.width, area.height
            )),
            Line::from("Rotate phone or reduce font · Q quit"),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_header(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let mode_color = match app.execution_mode.as_str() {
        "DRY RUN" => Color::Cyan,
        "CANARY" => Color::Yellow,
        _ => Color::Red,
    };
    let health_color = health_color(app);
    let mut health = vec![Span::styled(
        " HEALTH",
        Style::default().fg(health_color).bold(),
    )];
    health.extend(app.services.iter().map(|service| {
        let (dot, color) = match service.status {
            HealthStatus::Ok => ("●", Color::Green),
            HealthStatus::Warn => ("◐", Color::Yellow),
            HealthStatus::Err => ("○", Color::Red),
        };
        Span::styled(
            format!(" {dot} {}", service.name),
            Style::default().fg(color),
        )
    }));
    let widget = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" POLYPULSE ", title_style()),
            Span::styled(
                format!(" {} ", app.execution_mode),
                Style::default().fg(mode_color).bold(),
            ),
            Span::raw(format!(
                "  Uptime {}  Window {}  Left {}  UTC {}",
                app.uptime(),
                app.window_label,
                app.window_countdown(),
                app.utc_now().format("%H:%M:%S")
            )),
        ]),
        Line::from(health),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(false)),
    );
    frame.render_widget(widget, area);
}

fn draw_mobile_header(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let (health_ok, health_total) = app.services.iter().fold((0, 0), |(ok, total), service| {
        (
            ok + u16::from(service.status == HealthStatus::Ok),
            total + 1,
        )
    });
    let mode_color = match app.execution_mode.as_str() {
        "DRY RUN" => Color::Cyan,
        "CANARY" => Color::Yellow,
        _ => Color::Red,
    };
    let widget = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" POLYPULSE ", title_style()),
            Span::styled(&app.execution_mode, Style::default().fg(mode_color).bold()),
            Span::raw(format!("  Left {}", app.window_countdown())),
        ]),
        Line::from(vec![
            Span::raw(format!(" Up {} · ", app.uptime())),
            Span::styled(
                format!("HEALTH {health_ok}/{health_total}"),
                Style::default().fg(health_color(app)).bold(),
            ),
            Span::raw(format!(" · UTC {}", app.utc_now().format("%H:%M"))),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style(false)),
    );
    frame.render_widget(widget, area);
}

fn draw_markets(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let header = Row::new(["ASSET", "YES ASK", "NO ASK", "SUM", "ARB EDGE", "BOOK AGE"])
        .style(Style::default().fg(Color::Gray).bold());
    let rows: Vec<Row> = if app.markets.is_empty() {
        vec![Row::new(["—", "waiting", "—", "—", "—", "—"])]
    } else {
        app.markets
            .iter()
            .map(|market| {
                let price = |value: Option<f64>, direction: PriceDir| {
                    value
                        .map(|value| format!("{value:.3}{}", direction.arrow()))
                        .unwrap_or_else(|| "—".into())
                };
                let sum = market
                    .both_prices()
                    .map(|(yes, no)| format!("{:.3}", yes + no))
                    .unwrap_or_else(|| "—".into());
                let edge = app.profit_pct(market);
                let edge = if edge > 0.0 {
                    format!("+{edge:.2}%")
                } else {
                    "—".into()
                };
                let age = market
                    .last_update
                    .map(|updated| format!("{}s", updated.elapsed().as_secs()))
                    .unwrap_or_else(|| "—".into());
                let row = Row::new([
                    market.symbol.clone(),
                    price(market.yes_display(), market.yes_dir),
                    price(market.no_display(), market.no_dir),
                    sum,
                    edge,
                    age,
                ]);
                row.style(market_row_style(market))
            })
            .collect()
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(11),
            Constraint::Min(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" MARKET SNAPSHOT ")
            .title_style(title_style())
            .borders(Borders::ALL)
            .border_style(border_style(false)),
    );
    frame.render_widget(table, area);
}

fn draw_mobile_markets(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let header = Row::new(["ASSET", "YES", "NO", "SUM", "EDGE"])
        .style(Style::default().fg(Color::Gray).bold());
    let rows = app.markets.iter().take(2).map(|market| {
        let display = |value: Option<f64>| {
            value
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "—".into())
        };
        let sum = market
            .both_prices()
            .map(|(yes, no)| format!("{:.2}", yes + no))
            .unwrap_or_else(|| "—".into());
        let edge = app.profit_pct(market);
        let row = Row::new([
            market.symbol.clone(),
            display(market.yes_display()),
            display(market.no_display()),
            sum,
            if edge > 0.0 {
                format!("+{edge:.1}%")
            } else {
                "—".into()
            },
        ]);
        row.style(market_row_style(market))
    });
    let rows: Vec<_> = if app.markets.is_empty() {
        vec![Row::new(["—", "wait", "—", "—", "—"])]
    } else {
        rows.collect()
    };
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Min(7),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .title(" MARKETS ")
                .title_style(title_style())
                .borders(Borders::ALL)
                .border_style(border_style(false)),
        ),
        area,
    );
}

fn draw_desktop_strategies(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_arbitrage(frame, panels[0], app);
    draw_winner_buy(frame, panels[1], app);
}

fn draw_medium_strategies(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let panels = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_arbitrage_compact(frame, panels[0], app);
    draw_winner_buy_compact(frame, panels[1], app);
}

fn draw_selected_strategy(frame: &mut Frame, area: Rect, app: &DashboardState) {
    if app.selected_strategy == 0 {
        draw_arbitrage_compact(frame, area, app);
    } else {
        draw_winner_buy_compact(frame, area, app);
    }
}

fn market_is_opportunity(market: &crate::ui::state::MarketRow) -> bool {
    market.both_prices().is_some_and(|(yes, no)| yes + no < 1.0)
}

fn market_row_style(market: &crate::ui::state::MarketRow) -> Style {
    if market_is_opportunity(market) {
        Style::default().fg(Color::Red).bold()
    } else {
        Style::default()
    }
}

fn strategy_block(
    shortcut: &str,
    title: &str,
    state: StrategyDisplayState,
    selected: bool,
) -> Block<'static> {
    let (label, color) = state_label(state);
    Block::default()
        .title(
            Line::from(vec![
                Span::styled(
                    format!(" [{shortcut}] "),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::styled(format!("{title} "), title_style()),
            ])
            .left_aligned(),
        )
        .title(
            Line::from(Span::styled(
                format!(" {label} "),
                Style::default().fg(color).bold(),
            ))
            .right_aligned(),
        )
        .borders(Borders::ALL)
        .border_style(border_style(selected))
}

fn draw_arbitrage(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let pnl = if app.execution_mode == "DRY RUN" {
        app.paper_pnl
    } else {
        app.session_pnl
    };
    let lines = vec![
        metric_line(
            "Scans",
            app.arb_scans,
            "Opportunities",
            app.opportunities_admitted,
        ),
        metric_line(
            "Attempts",
            app.trades_started,
            "Succeeded",
            app.trades_succeeded,
        ),
        metric_line(
            "Failed",
            app.trades_failed,
            "One-sided",
            app.paper_one_sided,
        ),
        metric_line(
            "Depth reject",
            app.depth_rejections,
            "Other reject",
            app.opportunities_rejected,
        ),
        metric_line(
            "Quote reject",
            app.quote_rejections,
            "Risk reject",
            app.risk_rejections,
        ),
        Line::from(vec![
            Span::styled(format!("{:<13}", "Exposure"), muted()),
            Span::raw(format!("${:.2} / ${:.2}", app.exposure, app.exposure_limit)),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<13}", "PnL"), muted()),
            Span::styled(format!("${pnl:.2}"), pnl_style(pnl)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(strategy_block(
                "1",
                "SPREAD ARBITRAGE",
                app.arbitrage_state,
                app.selected_strategy == 0,
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_arbitrage_compact(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let pnl = if app.execution_mode == "DRY RUN" {
        app.paper_pnl
    } else {
        app.session_pnl
    };
    let lines = vec![
        compact_metric("Scans", app.arb_scans, "Opp", app.opportunities_admitted),
        compact_metric("Attempts", app.trades_started, "OK", app.trades_succeeded),
        compact_metric("Failed", app.trades_failed, "One-side", app.paper_one_sided),
        Line::from(format!(
            "Reject  D{:>5} Q{:>5} R{:>5} O{:>5}",
            app.depth_rejections,
            app.quote_rejections,
            app.risk_rejections,
            app.opportunities_rejected
        )),
        Line::from(vec![
            Span::styled("Exposure", muted()),
            Span::raw(format!(" ${:>7.2}  ", app.exposure)),
            Span::styled("PnL", muted()),
            Span::styled(format!("${pnl:.2}"), pnl_style(pnl)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(strategy_block(
                "1",
                "SPREAD ARBITRAGE",
                app.arbitrage_state,
                app.selected_strategy == 0,
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_winner_buy(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let total = app.winner_buy_wins + app.winner_buy_losses;
    let win_rate = if total > 0 {
        app.winner_buy_wins as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    let lines = vec![
        metric_line(
            "Scans",
            app.winner_buy_scans,
            "Candidates",
            app.winner_buy_signals,
        ),
        metric_line(
            "Attempts",
            app.winner_buy_attempts,
            "Fills",
            app.winner_buy_fills,
        ),
        metric_line(
            "Data reject",
            app.winner_data_rejections,
            "Book reject",
            app.winner_market_rejections,
        ),
        metric_line(
            "Boundary",
            app.winner_boundary_rejections,
            "Primary EV",
            app.winner_ev_rejections,
        ),
        metric_line(
            "Shadow EV",
            app.winner_shadow_ev_failures,
            "Confirming",
            app.winner_buy_awaiting_confirmation,
        ),
        metric_line(
            "Pending",
            app.winner_buy_pending,
            "Resolved",
            app.winner_buy_resolved,
        ),
        Line::from(vec![
            Span::styled(format!("{:<13}", "Result"), muted()),
            Span::raw(format!(
                "{}-{} ({win_rate:.1}%)  ",
                app.winner_buy_wins, app.winner_buy_losses
            )),
            Span::styled(
                format!("${:.2}", app.winner_buy_realized_pnl),
                pnl_style(app.winner_buy_realized_pnl),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(strategy_block(
                "2",
                "WINNER BUY",
                app.winner_buy_state,
                app.selected_strategy == 1,
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_winner_buy_compact(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let total = app.winner_buy_wins + app.winner_buy_losses;
    let win_rate = if total > 0 {
        app.winner_buy_wins as f64 / total as f64 * 100.0
    } else {
        0.0
    };
    let lines = vec![
        compact_metric(
            "Scans",
            app.winner_buy_scans,
            "Cand",
            app.winner_buy_signals,
        ),
        compact_metric(
            "Attempts",
            app.winner_buy_attempts,
            "Fills",
            app.winner_buy_fills,
        ),
        compact_metric(
            "Data rej",
            app.winner_data_rejections,
            "Book rej",
            app.winner_market_rejections,
        ),
        compact_metric(
            "Boundary",
            app.winner_boundary_rejections,
            "EV reject",
            app.winner_ev_rejections,
        ),
        compact_metric(
            "Pending",
            app.winner_buy_pending,
            "Resolved",
            app.winner_buy_resolved,
        ),
        Line::from(vec![
            Span::raw(format!(
                "W-L {}-{} {win_rate:.0}%  ",
                app.winner_buy_wins, app.winner_buy_losses
            )),
            Span::styled(
                format!("${:.2}", app.winner_buy_realized_pnl),
                pnl_style(app.winner_buy_realized_pnl),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(strategy_block(
                "2",
                "WINNER BUY",
                app.winner_buy_state,
                app.selected_strategy == 1,
            ))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn compact_metric(left: &str, left_value: u64, right: &str, right_value: u64) -> Line<'static> {
    Line::from(format!(
        "{left:<9}{left_value:>7}  {right:<9}{right_value:>7}"
    ))
}

fn metric_line(left: &str, left_value: u64, right: &str, right_value: u64) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{left:<13}"), muted()),
        Span::raw(format!("{left_value:>8}")),
        Span::raw("   "),
        Span::styled(format!("{right:<13}"), muted()),
        Span::raw(format!("{right_value:>8}")),
    ])
}

fn draw_events(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let lines = app
        .recent_events(area.height.saturating_sub(2) as usize)
        .into_iter()
        .map(|event| {
            if let Some((time, message)) = event.split_once("] ") {
                Line::from(vec![
                    Span::styled(format!("{time}] "), muted()),
                    Span::raw(message.to_string()),
                ])
            } else {
                Line::from(event)
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" RECENT EVENTS ")
                .title_style(title_style())
                .borders(Borders::ALL)
                .border_style(border_style(false)),
        ),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &DashboardState, mobile: bool) {
    let warning = if app.arbitrage_state == StrategyDisplayState::Blocked
        || app.winner_buy_state == StrategyDisplayState::Blocked
    {
        format!("  Blocked: {}", app.strategy_message)
    } else {
        String::new()
    };
    if mobile {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("1/2 ", Style::default().fg(Color::Cyan)),
                Span::raw("View  "),
                Span::styled("Space ", Style::default().fg(Color::Cyan)),
                Span::raw("Toggle  "),
                Span::styled("P ", Style::default().fg(Color::Yellow)),
                Span::raw("All  "),
                Span::styled("Q ", Style::default().fg(Color::Red)),
                Span::raw("Quit"),
            ]))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let line = if area.width < 105 {
        Line::from(vec![
            Span::styled("↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::raw("Select  "),
            Span::styled("Space ", Style::default().fg(Color::Cyan)),
            Span::raw("Toggle  "),
            Span::styled("P ", Style::default().fg(Color::Yellow)),
            Span::raw("All  "),
            Span::styled("Y ", Style::default().fg(Color::Yellow)),
            Span::raw("Confirm  "),
            Span::styled("Q ", Style::default().fg(Color::Red)),
            Span::raw(format!("Quit{warning}")),
        ])
    } else {
        Line::from(vec![
            Span::styled("↑/↓ ", Style::default().fg(Color::Cyan)),
            Span::raw("Select  "),
            Span::styled("Space ", Style::default().fg(Color::Cyan)),
            Span::raw("Start/Pause  "),
            Span::styled("P ", Style::default().fg(Color::Yellow)),
            Span::raw("Pause all  "),
            Span::styled("Y ", Style::default().fg(Color::Yellow)),
            Span::raw("Confirm LIVE  "),
            Span::styled("Q ", Style::default().fg(Color::Red)),
            Span::raw(format!("Quit{warning}")),
        ])
    };
    frame.render_widget(
        Paragraph::new(line).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style(false)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn rendered_state(width: u16, height: u16, state: &DashboardState) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, state)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn desktop_and_medium_render_both_strategies_without_charts() {
        let state = DashboardState::new_live("FAK", 1_000.0);
        for (width, height) in [(120, 40), (80, 32)] {
            let text = rendered_state(width, height, &state);
            assert!(text.contains("MARKET SNAPSHOT"));
            assert!(text.contains("SPREAD ARBITRAGE"));
            assert!(text.contains("WINNER BUY"));
            assert!(text.contains("HEALTH"));
            assert!(!text.contains("PNL FLOW"));
            assert!(!text.contains("EDGE CURVES"));
        }
    }

    #[test]
    fn market_sum_below_one_is_an_opportunity() {
        let mut state = DashboardState::new_live("FAK", 1_000.0);
        state.update_market("BTC", Some(0.45), Some(0.50), false);
        assert!(market_is_opportunity(&state.markets[0]));
        let opportunity_style = market_row_style(&state.markets[0]);
        assert_eq!(opportunity_style.fg, Some(Color::Red));
        assert!(opportunity_style.add_modifier.contains(Modifier::BOLD));
        state.update_market("BTC", Some(0.51), Some(0.50), false);
        assert!(!market_is_opportunity(&state.markets[0]));
        assert_eq!(market_row_style(&state.markets[0]), Style::default());
    }

    #[test]
    fn phone_layout_renders_only_the_selected_strategy_at_40_by_22() {
        let mut state = DashboardState::new_live("FAK", 1_000.0);
        let arbitrage = rendered_state(40, 22, &state);
        assert!(arbitrage.contains("MARKETS"));
        assert!(arbitrage.contains("SPREAD ARBITRAGE"));
        assert!(!arbitrage.contains("WINNER BUY"));
        assert!(arbitrage.contains("1/2"));
        assert!(arbitrage.contains("Space"));

        state.selected_strategy = 1;
        let winner = rendered_state(40, 22, &state);
        assert!(winner.contains("WINNER BUY"));
        assert!(!winner.contains("SPREAD ARBITRAGE"));
        assert!(winner.contains("Pending"));
    }

    #[test]
    fn responsive_breakpoints_switch_without_hiding_controls() {
        let state = DashboardState::new_live("FAK", 1_000.0);
        let narrow = rendered_state(69, 32, &state);
        assert!(narrow.contains("SPREAD ARBITRAGE"));
        assert!(!narrow.contains("WINNER BUY"));
        assert!(narrow.contains("Space"));

        let medium = rendered_state(70, 32, &state);
        assert!(medium.contains("SPREAD ARBITRAGE"));
        assert!(medium.contains("WINNER BUY"));

        let short_wide = rendered_state(120, 31, &state);
        assert!(short_wide.contains("SPREAD ARBITRAGE"));
        assert!(!short_wide.contains("WINNER BUY"));
    }

    #[test]
    fn undersized_terminal_shows_actionable_message() {
        let state = DashboardState::new_live("FAK", 1_000.0);
        let text = rendered_state(39, 21, &state);
        assert!(text.contains("terminal too small"));
        assert!(text.contains("need 40x22"));
        assert!(text.contains("Q quit"));
    }
}
