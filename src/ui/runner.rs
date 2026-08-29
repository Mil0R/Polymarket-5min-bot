use std::io::{self, stdout, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use crate::ui::control::{ControlResult, StrategyControls, StrategyKind};
use crate::ui::render;
use crate::ui::state::{DashboardAction, DashboardState};

pub fn spawn_dashboard_thread(
    state: Arc<Mutex<DashboardState>>,
    shutdown: Arc<AtomicBool>,
    controls: Arc<StrategyControls>,
) {
    std::thread::spawn(move || {
        if let Err(e) = run_dashboard(state, shutdown, controls) {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("bot.log")
                .and_then(|mut f| {
                    use std::io::Write;
                    writeln!(f, "TUI exited: {e}")
                });
        }
    });
}

fn run_dashboard(
    state: Arc<Mutex<DashboardState>>,
    shutdown: Arc<AtomicBool>,
    controls: Arc<StrategyControls>,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout: Stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(200);
    let mut last_tick = Instant::now();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        {
            let app = state.lock().expect("dashboard lock");
            terminal.draw(|frame| render::draw(frame, &app))?;
        }

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let action = {
                        let mut app = state.lock().expect("dashboard lock");
                        app.on_key(key.code)
                    };
                    if matches!(action, DashboardAction::Quit) {
                        shutdown.store(true, Ordering::Relaxed);
                        break;
                    }
                    let result = match action {
                        DashboardAction::ToggleStrategy(0) => {
                            Some(controls.toggle(StrategyKind::BinaryComplementArbitrage))
                        }
                        DashboardAction::ToggleStrategy(1) => {
                            Some(controls.toggle(StrategyKind::LateResolutionWinnerTrading))
                        }
                        DashboardAction::ToggleStrategy(2) => {
                            Some(controls.toggle(StrategyKind::WalletCopyTrading))
                        }
                        DashboardAction::PauseAll => {
                            controls.pause_all();
                            let mut app = state.lock().expect("dashboard lock");
                            app.push_event("All strategy entries manually paused");
                            None
                        }
                        DashboardAction::ConfirmEnable => Some(controls.confirm()),
                        _ => None,
                    };
                    if let Some(result) = result {
                        let mut app = state.lock().expect("dashboard lock");
                        match result {
                            ControlResult::Changed { strategy, enabled } => {
                                tracing::info!(
                                    ?strategy,
                                    enabled,
                                    "Strategy runtime state changed from TUI"
                                );
                                app.push_event(format!(
                                    "{} {}",
                                    strategy_name(strategy),
                                    if enabled { "started" } else { "paused" }
                                ))
                            }
                            ControlResult::ConfirmationRequired { strategy } => {
                                app.push_event(format!(
                                    "Enable LIVE {}? Press Y within 5 seconds",
                                    strategy_name(strategy)
                                ))
                            }
                            ControlResult::Rejected(reason) => {
                                tracing::warn!(%reason, "Strategy control rejected");
                                app.push_event(format!("Strategy control rejected: {reason}"));
                            }
                        }
                    }
                    let mut app = state.lock().expect("dashboard lock");
                    app.set_strategy_states(
                        controls.state(StrategyKind::BinaryComplementArbitrage),
                        controls.state(StrategyKind::LateResolutionWinnerTrading),
                        controls.state(StrategyKind::WalletCopyTrading),
                        controls.blocked_reason(),
                    );
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            {
                let mut app = state.lock().expect("dashboard lock");
                app.on_render_tick();
            }
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn strategy_name(strategy: StrategyKind) -> &'static str {
    match strategy {
        StrategyKind::BinaryComplementArbitrage => "Binary Complement Arbitrage",
        StrategyKind::LateResolutionWinnerTrading => "Late-Resolution Winner Trading",
        StrategyKind::WalletCopyTrading => "Wallet-Copy Trading",
    }
}
