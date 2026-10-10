//! Terminal dashboard. It only reads the database (and writes the operator's
//! mode), so it can be started and stopped at will next to a running bot.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, LineGauge, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::time::interval;

use crate::status::{LegSnapshot, Snapshot};
use crate::store::{EventRow, IntentRow, Store, Totals, now_ms};
use crate::strategy::decide::Mode;

const REFRESH: Duration = Duration::from_millis(500);
/// The bot rewrites its status about once a second; older than this means it is not running.
const STALE_MS: i64 = 5_000;

struct Screen {
    status: Option<Snapshot>,
    mode: Mode,
    events: Vec<EventRow>,
    intents: Vec<IntentRow>,
    totals: Totals,
}

impl Screen {
    async fn load(store: &Store) -> Result<Self> {
        Ok(Self {
            status: store.read_status().await?,
            mode: store.mode().await?,
            events: store.recent_events(200).await?,
            intents: store.recent_intents(6).await?,
            totals: store.totals().await?,
        })
    }
}

pub async fn run(store: &Store) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, store).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, store: &Store) -> Result<()> {
    let mut keys = EventStream::new();
    let mut refresh = interval(REFRESH);
    loop {
        let screen = Screen::load(store).await?;
        terminal.draw(|frame| draw(frame, &screen))?;
        tokio::select! {
            _ = refresh.tick() => {}
            event = keys.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    // Flatten is deliberately not a single keypress: use `papertrade flatten`.
                    KeyCode::Char('p') => {
                        let next = if screen.mode == Mode::Run { Mode::Pause } else { Mode::Run };
                        store.set_mode(next).await?;
                    }
                    _ => {}
                },
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            },
        }
    }
}

fn draw(frame: &mut Frame, screen: &Screen) {
    let [header, market, accounts, ledger, timeline, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, screen);
    let Some(status) = &screen.status else {
        let hint =
            Paragraph::new("No status yet. Start the bot with `papertrade run`.").block(Block::bordered());
        frame.render_widget(hint, market);
        draw_timeline(frame, timeline, &screen.events);
        return;
    };

    let [price, protocol] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(market);
    draw_price(frame, price, status);
    draw_protocol(frame, protocol, status);

    let [long, short] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(accounts);
    for (area, leg) in [long, short].into_iter().zip(&status.legs) {
        draw_leg(frame, area, leg);
    }
    draw_ledger(frame, ledger, screen);
    draw_timeline(frame, timeline, &screen.events);
    frame.render_widget(Line::from(" q quit · p pause/resume · flatten: `papertrade flatten`").dim(), footer);
}

fn draw_header(frame: &mut Frame, area: Rect, screen: &Screen) {
    let mode = match screen.mode {
        Mode::Run => Span::from(" RUN ").black().on_green(),
        Mode::Pause => Span::from(" PAUSE ").black().on_yellow(),
        Mode::Flatten => Span::from(" FLATTEN ").white().on_red(),
    };
    let mut title = vec![Span::from(" papertrade ").bold(), mode];
    let mut decision = Line::from("the bot has not reported yet").dim();
    if let Some(status) = &screen.status {
        let age_ms = now_ms() - status.at_ms;
        title.push(Span::from(format!(" {} ", status.coin)));
        if status.dry_run {
            title.push(Span::from(" DRY RUN ").black().on_cyan());
        }
        title.push(if age_ms > STALE_MS {
            Span::from(format!(" BOT NOT RUNNING · last seen {} ago ", age(age_ms))).white().on_red()
        } else {
            Span::from(" live ").green()
        });
        let color = match status.decision.as_str() {
            "wait" => Color::Yellow,
            _ => Color::Green,
        };
        decision = Line::from(vec![
            Span::from(format!("{} ▸ ", status.decision.to_uppercase())).bold().fg(color),
            Span::from(status.summary.clone()).fg(color),
        ]);
    }
    let block = Block::bordered().title(Line::from(title));
    frame.render_widget(Paragraph::new(decision).wrap(Wrap { trim: true }).block(block), area);
}

fn draw_price(frame: &mut Frame, area: Rect, status: &Snapshot) {
    let block = Block::bordered().title(" Market ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [text, gauge] = Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).areas(inner);

    let mid = status.mid.as_deref().unwrap_or("no fresh quote");
    let lines = vec![
        Line::from(vec![Span::from("mid ").dim(), Span::from(mid.to_owned()).bold()]),
        match status.move_bps {
            Some(bps) => Line::from(vec![
                Span::from("move ").dim(),
                Span::from(format!("{bps:+.1} bps")).bold(),
                Span::from(format!(" of ±{} target", status.close_bps)).dim(),
            ]),
            None => Line::from("no hedged round open").dim(),
        },
    ];
    frame.render_widget(Paragraph::new(lines), text);
    if let Some(bps) = status.move_bps {
        let ratio = (bps.abs() / f64::from(status.close_bps)).clamp(0.0, 1.0);
        frame.render_widget(
            LineGauge::default().ratio(ratio).filled_style(Style::new().fg(Color::Green)),
            gauge,
        );
    }
}

fn draw_protocol(frame: &mut Frame, area: Rect, status: &Snapshot) {
    let lines = match &status.trading {
        Some(trading) => vec![
            field("trading paused", if trading.trading_paused { "yes" } else { "no" }),
            field(
                "relayer refuses",
                &if trading.paused_actions.is_empty() {
                    "nothing".to_owned()
                } else {
                    trading.paused_actions.join(", ")
                },
            ),
            field(
                "payout queue",
                &status.queue_total_usd.as_ref().map_or("unknown".to_owned(), |usd| format!("{usd} USD")),
            ),
        ],
        None => vec![Line::from("trading state unavailable").red()],
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::bordered().title(" Protocol ")),
        area,
    );
}

fn draw_leg(frame: &mut Frame, area: Rect, leg: &LegSnapshot) {
    let title = format!(" {} {} ", leg.side.as_str().to_uppercase(), short_hex(&leg.wallet));
    let mut lines = Vec::new();
    match &leg.account {
        None => lines.push(Line::from("account state unavailable").red()),
        Some(account) => {
            lines.push(field("available", &format!("{} USD", account.available_usd)));
            lines.push(field("queued payout", &format!("{} USD", account.queued_usd)));
            lines.push(match account.session_key_expiry {
                Some(expiry) => {
                    let left_ms = i64::try_from(expiry).unwrap_or(i64::MAX).saturating_mul(1000) - now_ms();
                    field("session key", &format!("{} · {} left", short_hex(&leg.session_key), age(left_ms)))
                }
                None => {
                    Line::from(format!("session key {} NOT REGISTERED", short_hex(&leg.session_key))).red()
                }
            });
            if account.positions.is_empty() {
                lines.push(Line::from("no position").dim());
            }
            for position in &account.positions {
                lines.push(Line::from(format!(
                    "{} {} USD × {} @ {} · bust {}",
                    if position.is_long { "long" } else { "short" },
                    position.margin_usd,
                    position.leverage,
                    position.entry,
                    position.bust,
                )));
            }
        }
    }
    if leg.pending_intents > 0 {
        lines.push(Line::from(format!("{} intent(s) in flight", leg.pending_intents)).yellow());
    }
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), area);
}

fn draw_ledger(frame: &mut Frame, area: Rect, screen: &Screen) {
    let totals = &screen.totals;
    let cost = totals.cost_per_paper().map_or("n/a".to_owned(), |cost| format!("{cost:.5} USD/PAPER"));
    let mut lines = vec![Line::from(vec![
        Span::from("settled positions ").dim(),
        Span::from(totals.trades.to_string()).bold(),
        Span::from("  net ").dim(),
        Span::from(format!("{:+.2} USD", totals.net_pnl_usd)).bold(),
        Span::from("  PAPER ").dim(),
        Span::from(format!("{:.0}", totals.paper_minted)).bold(),
        Span::from("  cost ").dim(),
        Span::from(cost).bold(),
    ])];
    lines.push(Line::from("recent intents").dim());
    for intent in &screen.intents {
        let result = intent.outcome.as_deref().or(intent.detail.as_deref()).unwrap_or("");
        lines.push(Line::from(format!(
            "{} {:5} {:14} {:8} {result}",
            clock(intent.created_at_ms),
            intent.side,
            intent.action,
            intent.status,
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Ledger ")), area);
}

fn draw_timeline(frame: &mut Frame, area: Rect, events: &[EventRow]) {
    let visible = usize::from(area.height.saturating_sub(2));
    let lines: Vec<Line> = events
        .iter()
        .take(visible)
        .map(|event| {
            Line::from(vec![
                Span::from(clock(event.at_ms)).dim(),
                Span::from(format!(" {:8} ", event.kind)).cyan(),
                Span::from(event.summary.clone()),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Timeline (UTC, newest first) ")),
        area,
    );
}

fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![Span::from(format!("{label} ")).dim(), Span::from(value.to_owned())])
}

/// `0x02b4…1a78`
fn short_hex(hex: &str) -> String {
    if hex.len() <= 12 { hex.to_owned() } else { format!("{}…{}", &hex[..6], &hex[hex.len() - 4..]) }
}

/// A duration in its largest sensible unit.
pub fn age(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// `MM-DD HH:MM:SS` in UTC.
pub fn clock(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms)
        .map_or_else(|| "??-?? ??:??:??".to_owned(), |at| at.format("%m-%d %H:%M:%S").to_string())
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::status::{AccountSnapshot, PositionSnapshot, TradingSnapshot};
    use crate::strategy::decide::Side;

    fn leg(side: Side, wallet: &str, position: Option<PositionSnapshot>) -> LegSnapshot {
        LegSnapshot {
            side,
            wallet: wallet.into(),
            session_key: "0xd24a429da902726a53db059fb5bd4e411f788782".into(),
            account: Some(AccountSnapshot {
                available_usd: "1000.00".into(),
                queued_usd: "0.00".into(),
                session_key_expiry: Some(u64::try_from(now_ms() / 1000).unwrap() + 20 * 86_400),
                positions: position.into_iter().collect(),
            }),
            pending_intents: 0,
        }
    }

    fn hedged_screen() -> Screen {
        let position = |is_long| PositionSnapshot {
            id: if is_long { "7" } else { "8" }.into(),
            is_long,
            entry: "82700.0".into(),
            bust: if is_long { "78604.4" } else { "86795.6" }.into(),
            margin_usd: "500.00".into(),
            leverage: 20,
        };
        Screen {
            status: Some(Snapshot {
                at_ms: now_ms(),
                dry_run: false,
                mode: Mode::Run,
                coin: "BTC".into(),
                close_bps: 100,
                decision: "wait".into(),
                summary: "hedged; price is inside the target".into(),
                mid: Some("83100.5".into()),
                move_bps: Some(48.4),
                trading: Some(TradingSnapshot { trading_paused: false, paused_actions: Vec::new() }),
                queue_total_usd: Some("0.00".into()),
                legs: vec![
                    leg(Side::Long, "0x02b439b469daedb910fdbb6d7cdc875c38161a78", Some(position(true))),
                    leg(Side::Short, "0x70997970c51812dc3a010c7d01b50e0d17dc79c8", Some(position(false))),
                ],
            }),
            mode: Mode::Run,
            events: vec![EventRow {
                at_ms: 1_791_621_936_000,
                kind: "decision".into(),
                summary: "hedged; price is inside the target".into(),
            }],
            intents: vec![IntentRow {
                created_at_ms: 1_791_621_900_000,
                side: "long".into(),
                action: "openPosition".into(),
                status: "settled".into(),
                outcome: Some("succeeded".into()),
                detail: None,
            }],
            totals: Totals { trades: 12, net_pnl_usd: -81.6, paper_minted: 58_800.0 },
        }
    }

    fn render(screen: &Screen) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();
        terminal.draw(|frame| draw(frame, screen)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .map(|line| line.trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_hedged_round_shows_decision_move_legs_ledger_and_timeline() {
        let text = render(&hedged_screen());
        for expected in [
            "RUN",
            "live",
            "WAIT ▸ hedged; price is inside the target",
            "mid 83100.5",
            "move +48.4 bps of ±100 target",
            "relayer refuses nothing",
            "payout queue 0.00 USD",
            "LONG 0x02b4…1a78",
            "SHORT 0x7099…79c8",
            "long 500.00 USD × 20 @ 82700.0 · bust 78604.4",
            "short 500.00 USD × 20 @ 82700.0 · bust 86795.6",
            "settled positions 12  net -81.60 USD  PAPER 58800  cost 0.00139 USD/PAPER",
            "openPosition",
            "10-10 08:45:36 decision",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn a_stopped_bot_is_called_out() {
        let mut screen = hedged_screen();
        screen.status.as_mut().unwrap().at_ms = now_ms() - 90_000;
        screen.mode = Mode::Pause;
        let text = render(&screen);
        assert!(text.contains("BOT NOT RUNNING · last seen 1m ago"), "{text}");
        assert!(text.contains("PAUSE"), "{text}");
    }

    #[test]
    fn missing_data_is_shown_as_missing() {
        let mut screen = hedged_screen();
        let status = screen.status.as_mut().unwrap();
        status.trading = None;
        status.mid = None;
        status.move_bps = None;
        status.legs[0].account = None;
        status.legs[1].account.as_mut().unwrap().session_key_expiry = None;
        status.legs[1].pending_intents = 1;
        let text = render(&screen);
        for expected in [
            "trading state unavailable",
            "mid no fresh quote",
            "account state unavailable",
            "NOT REGISTERED",
            "1 intent(s) in flight",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn before_the_first_status_it_says_how_to_start() {
        let screen = Screen { status: None, ..hedged_screen() };
        assert!(render(&screen).contains("Start the bot with `papertrade run`"));
    }

    #[test]
    fn clock_shows_utc_month_day_and_time() {
        assert_eq!(clock(0), "01-01 00:00:00");
        assert_eq!(clock(1_791_621_936_000), "10-10 08:45:36");
        assert_eq!(clock(1_709_251_199_000), "02-29 23:59:59");
    }

    #[test]
    fn age_uses_the_largest_unit() {
        assert_eq!(age(-5), "0s");
        assert_eq!(age(59_999), "59s");
        assert_eq!(age(60_000), "1m");
        assert_eq!(age(2 * 3_600_000), "2h");
        assert_eq!(age(29 * 86_400_000), "29d");
    }

    #[test]
    fn long_hex_is_abbreviated() {
        assert_eq!(short_hex("0x02b439b469daedb910fdbb6d7cdc875c38161a78"), "0x02b4…1a78");
        assert_eq!(short_hex("0xabc"), "0xabc");
    }
}
