//! Terminal dashboard, and the same content as plain text for `papertrade status`.
//! Both only read the database (the dashboard also writes the operator's mode),
//! so they can be started and stopped at will next to a running bot.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::time::interval;

use crate::status::{LegSnapshot, RoundEstimate, Snapshot};
use crate::store::{EventRow, Store, Totals, now_ms};
use crate::strategy::decide::{Mode, Phase};

const REFRESH: Duration = Duration::from_millis(500);
/// The bot rewrites its status about once a second; older than this means it is not running.
const STALE_MS: i64 = 5_000;
/// How many timeline entries `status` prints.
const REPORT_EVENTS: usize = 10;
/// What stands in for a value that is not known.
const MISSING: &str = "—";

struct Screen {
    /// The moment the screen was loaded; every "ago" and "left" is measured from it.
    now_ms: i64,
    status: Option<Snapshot>,
    mode: Mode,
    events: Vec<EventRow>,
    totals: Totals,
}

impl Screen {
    async fn load(store: &Store) -> Result<Self> {
        Ok(Self {
            now_ms: now_ms(),
            status: store.read_status().await?,
            mode: store.mode().await?,
            events: store.recent_events(200).await?,
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

/// Everything the dashboard shows, once, as plain text.
pub async fn report(store: &Store) -> Result<String> {
    Ok(text(&Screen::load(store).await?))
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

const NO_STATUS: &str = "No status yet. Start the bot with `papertrade run`.";

fn draw(frame: &mut Frame, screen: &Screen) {
    let Some(status) = &screen.status else {
        let [header, hint, timeline] =
            Layout::vertical([Constraint::Length(4), Constraint::Length(3), Constraint::Min(4)])
                .areas(frame.area());
        draw_header(frame, header, screen);
        frame.render_widget(Paragraph::new(NO_STATUS).block(Block::bordered()), hint);
        panel(frame, timeline, TIMELINE, timeline_lines(&screen.events, timeline.height));
        return;
    };

    let funds = funds_lines(status, screen.now_ms);
    let [header, market, accounts, funds_area, ledger, timeline, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(bordered(funds.len())),
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, screen);
    // The estimate is a table and needs the wider half.
    let [price, estimate] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(market);
    panel(frame, price, " Market ", market_lines(status, screen.now_ms));
    panel(frame, estimate, " Estimate ", estimate_lines(status));

    let [long, short] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(accounts);
    for (area, leg) in [long, short].into_iter().zip(&status.legs) {
        let title = format!(" {} {} ", leg.side.as_str().to_uppercase(), short_hex(&leg.wallet));
        panel(frame, area, &title, account_lines(leg, screen.now_ms, short_hex));
    }
    panel(frame, funds_area, " Funds ", funds);
    panel(frame, ledger, " Ledger ", vec![ledger_line(&screen.totals)]);
    panel(frame, timeline, TIMELINE, timeline_lines(&screen.events, timeline.height));
    frame.render_widget(Line::from(" q quit · p pause/resume · flatten: `papertrade flatten`").dim(), footer);
}

const TIMELINE: &str = " Timeline (UTC, newest first) ";

/// The height of a bordered panel holding this many lines.
fn bordered(lines: usize) -> u16 {
    u16::try_from(lines).unwrap_or(u16::MAX).saturating_add(2)
}

fn panel(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(title.to_owned())), area);
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
        title.push(Span::from(format!(" {} ", status.coin)));
        if status.dry_run {
            title.push(Span::from(" DRY RUN ").black().on_cyan());
        }
        title.push(match liveness(status, screen.now_ms) {
            Some(stale) => Span::from(format!(" BOT {stale} ")).white().on_red(),
            None => Span::from(" live ").green(),
        });
        let color = match status.decision.as_str() {
            "wait" => Color::Yellow,
            _ => Color::Green,
        };
        decision = Line::from(vec![
            Span::from(format!(" {} ", status.phase.to_string().to_uppercase())).black().on_white(),
            Span::from(format!(" {} ▸ ", status.decision.to_uppercase())).bold().fg(color),
            Span::from(status.summary.clone()).fg(color),
        ]);
    }
    let block = Block::bordered().title(Line::from(title));
    frame.render_widget(Paragraph::new(decision).wrap(Wrap { trim: true }).block(block), area);
}

/// `None` while the bot is reporting; otherwise how long it has been silent.
fn liveness(status: &Snapshot, now_ms: i64) -> Option<String> {
    let silent_ms = now_ms - status.at_ms;
    (silent_ms > STALE_MS).then(|| format!("NOT RUNNING · last seen {} ago", age(silent_ms)))
}

fn dim(text: impl Into<String>) -> Span<'static> {
    Span::from(text.into()).dim()
}

fn plain(text: impl Into<String>) -> Span<'static> {
    Span::from(text.into())
}

fn strong(text: impl Into<String>) -> Span<'static> {
    Span::from(text.into()).bold()
}

/// Mid, entry, move, where the round closes and where each leg is liquidated,
/// then what the protocol currently allows.
fn market_lines(status: &Snapshot, now_ms: i64) -> Vec<Line<'static>> {
    let hedged = status.phase == Phase::Hedged;
    let mut top = vec![dim("mid "), strong(status.mid.clone().unwrap_or_else(|| "no fresh quote".into()))];
    if hedged {
        let entry = status.round.as_ref().map_or(MISSING, |round| round.entry.as_str());
        top.extend([dim(" · entry "), plain(entry)]);
    }
    let positions =
        || status.legs.iter().filter_map(|leg| Some((leg.side, leg.account.as_ref()?.positions.first()?)));
    if positions().next().is_some() {
        let held = status.held_since_ms().and_then(|since| i64::try_from(since).ok());
        top.extend([dim(" · held "), plain(held.map_or(MISSING.to_owned(), |since| age(now_ms - since)))]);
    }
    let mut lines = vec![Line::from(top)];

    match status.move_bps {
        Some(bps) => {
            let reached = (bps.abs() / f64::from(status.close_bps)).clamp(0.0, 1.0);
            lines.push(Line::from(vec![
                dim("move "),
                strong(format!("{bps:+.1} bps")),
                dim(format!(" of ±{} target", status.close_bps)),
            ]));
            lines.push(Line::from(vec![
                Span::from(bar(reached, 30)).green(),
                dim(format!(" {:.0}%", reached * 100.0)),
            ]));
        }
        None if hedged => lines.push(Line::from("move unknown: no fresh price").yellow()),
        None => lines.push(Line::from("no hedged round open").dim()),
    }
    if let Some(round) = &status.round {
        lines.push(Line::from(vec![
            dim("closes at ≤ "),
            plain(round.close_below.clone()),
            dim(" or ≥ "),
            plain(round.close_above.clone()),
        ]));
    }
    let busts: Vec<String> =
        positions().map(|(side, position)| format!("{side} {}", position.bust)).collect();
    if !busts.is_empty() {
        lines.push(Line::from(vec![dim("bust "), plain(busts.join(" · "))]));
    }

    let queue = status.queue_total_usd.as_ref().map_or("unknown".to_owned(), |usd| format!("{usd} USD"));
    let mut protocol = vec![dim("payout queue "), plain(queue)];
    match &status.trading {
        Some(trading) => {
            protocol.push(dim(" · trading "));
            protocol.push(if trading.trading_paused { plain("PAUSED").red() } else { plain("open") });
            lines.push(Line::from(protocol));
            let refused = &trading.paused_actions;
            lines.push(Line::from(vec![
                dim("relayer refuses "),
                if refused.is_empty() { plain("nothing") } else { plain(refused.join(", ")).yellow() },
            ]));
        }
        None => {
            lines.push(Line::from(protocol));
            lines.push(Line::from("trading state unavailable").red());
        }
    }
    lines
}

/// A text gauge: `filled` of `width` cells.
fn bar(filled: f64, width: usize) -> String {
    let cells = ((filled * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(cells), "░".repeat(width - cells))
}

const ESTIMATE_NOTE: &str = "an estimate: the contract has the last word at settlement";

fn estimate_lines(status: &Snapshot) -> Vec<Line<'static>> {
    let Some(estimate) = &status.estimate else {
        return vec![
            Line::from("no estimate: an input is missing").yellow(),
            Line::from("it needs the market's impact parameters, the fee rates,").dim(),
            Line::from("the mint curve, the payout queue and a price").dim(),
        ];
    };
    let fees = estimate.fees.as_ref().map_or(MISSING.to_owned(), |fees| {
        format!(
            "win {}% · loss {}% · liquidation {}%",
            fees.win_percent, fees.loss_percent, fees.liquidation_percent
        )
    });
    vec![
        Line::from(vec![
            dim("mint rate "),
            strong(format!("{} PAPER", estimate.mint_rate)),
            dim(" per USD of mint basis"),
        ]),
        Line::from(vec![dim("fees "), plain(fees)]),
        Line::from(estimate_row("", ["loser", "winner", "net", "PAPER", "USD/PAPER"].map(str::to_owned)))
            .dim(),
        Line::from(estimate_row("at target", estimate_cells(Some(&estimate.at_target)))),
        Line::from(estimate_row("now", estimate_cells(estimate.now.as_ref()))),
        Line::from(ESTIMATE_NOTE).dim(),
    ]
}

fn estimate_row(label: &str, cells: [String; 5]) -> String {
    let [loser, winner, net, paper, cost] = cells;
    format!("{label:<9} {loser:>9} {winner:>9} {net:>9} {paper:>11} {cost:>10}")
}

/// The figures of one round as signed USD amounts; dashes when there is none.
fn estimate_cells(round: Option<&RoundEstimate>) -> [String; 5] {
    let Some(round) = round else { return [MISSING; 5].map(str::to_owned) };
    // "0.00" needs no sign.
    let signed = |sign: char, amount: &str| {
        if amount.chars().all(|c| matches!(c, '0' | '.')) {
            amount.to_owned()
        } else {
            format!("{sign}{amount}")
        }
    };
    [
        signed('-', &round.loser_usd),
        signed('+', &round.winner_usd),
        round.net_usd.clone(),
        round.paper.clone(),
        round.usd_per_paper.clone().unwrap_or_else(|| MISSING.to_owned()),
    ]
}

/// One account: balances, keys, intents on their way, positions.
fn account_lines(leg: &LegSnapshot, now_ms: i64, hex: fn(&str) -> String) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match &leg.account {
        None => lines.push(Line::from("account state unavailable").red()),
        Some(account) => {
            lines.push(Line::from(vec![
                dim("available "),
                strong(format!("{} USD", account.available_usd)),
                dim(" · queued payout "),
                plain(format!("{} USD", account.queued_usd)),
            ]));
            let key = hex(&leg.session_key);
            lines.push(match account.session_key_expiry {
                None => Line::from(format!("session key {key} NOT REGISTERED")).red(),
                Some(expiry) => {
                    let left_ms = i64::try_from(expiry).unwrap_or(i64::MAX).saturating_mul(1000) - now_ms;
                    if left_ms <= 0 {
                        Line::from(format!("session key {key} EXPIRED")).red()
                    } else {
                        Line::from(vec![dim("session key "), plain(format!("{key} · {} left", age(left_ms)))])
                    }
                }
            });
        }
    }
    lines.push(Line::from(vec![
        dim("wallet key "),
        if leg.has_wallet_key { plain("present") } else { plain("absent").dim() },
    ]));
    lines.push(match leg.pending_intents {
        0 => Line::from(vec![dim("in flight "), plain("nothing")]),
        pending => Line::from(format!("{pending} intent(s) in flight")).yellow(),
    });
    if let Some(account) = &leg.account {
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
    lines
}

/// How the two accounts are funded, and the transfer between them if one is under way.
fn funds_lines(status: &Snapshot, now_ms: i64) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(match &status.funds {
        None => Line::from(vec![dim("funds "), plain(MISSING)]),
        Some(funds) => {
            let verdict = match funds.verdict.as_str() {
                "enough" => plain("enough").green(),
                "rebalance" => plain("rebalance").yellow(),
                other => plain(other.to_owned()).red(),
            };
            Line::from(vec![
                dim("total "),
                strong(format!("{} USD", funds.total_usd)),
                dim(" · difference "),
                plain(format!("{} USD", funds.difference_usd)),
                dim(" · rounds affordable "),
                plain(funds.rounds_affordable.to_string()),
                dim(" · verdict "),
                verdict,
                dim(" · transfers "),
                plain(if funds.can_rebalance { "automatic" } else { "manual" }),
            ])
        }
    });
    match &status.transfer {
        None => lines.push(Line::from("no transfer in progress").dim()),
        Some(transfer) => {
            let stuck = transfer.stage == "stuck";
            let stage = if stuck { plain("stuck").red() } else { plain(transfer.stage.clone()) };
            lines.push(Line::from(vec![
                dim("transfer "),
                strong(format!("{} → {} {} USD", transfer.from, transfer.to, transfer.amount_usd)),
                dim(" · stage "),
                stage,
                dim(" · "),
                plain(format!("{} elapsed", age(now_ms - transfer.started_at_ms))),
            ]));
            if let Some(note) = &transfer.note {
                let note = plain(note.clone());
                lines.push(Line::from(vec![dim("  "), if stuck { note.red() } else { note }]));
            }
        }
    }
    lines
}

fn ledger_line(totals: &Totals) -> Line<'static> {
    let cost = totals.cost_per_paper().map_or("n/a".to_owned(), |cost| format!("{cost:.5} USD/PAPER"));
    Line::from(vec![
        dim("settled positions "),
        strong(totals.trades.to_string()),
        dim("  net "),
        strong(format!("{:+.2} USD", totals.net_pnl_usd)),
        dim("  PAPER "),
        strong(format!("{:.0}", totals.paper_minted)),
        dim("  actual cost "),
        strong(cost),
    ])
}

/// As many of the newest events as fit a bordered panel of this height.
fn timeline_lines(events: &[EventRow], height: u16) -> Vec<Line<'static>> {
    let visible = usize::from(height.saturating_sub(2));
    events.iter().take(visible).map(event_line).collect()
}

fn event_line(event: &EventRow) -> Line<'static> {
    Line::from(vec![
        Span::from(clock(event.at_ms)).dim(),
        Span::from(format!(" {:8} ", event.kind)).cyan(),
        Span::from(event.summary.clone()),
    ])
}

/// The dashboard's content without the dashboard: one section per panel.
fn text(screen: &Screen) -> String {
    let Some(status) = &screen.status else { return format!("{NO_STATUS}\n") };
    let mut out = String::new();
    let mut field = |label: &str, value: String| {
        let _ = writeln!(out, "{label:<10}{value}");
    };
    field("mode", format!("{}{}", screen.mode.as_str(), if status.dry_run { " (dry run)" } else { "" }));
    field("bot", liveness(status, screen.now_ms).unwrap_or_else(|| "live".to_owned()));
    field("market", status.coin.clone());
    field("legs", status.phase.to_string());
    field("decision", format!("{}: {}", status.decision, status.summary));

    let mut section = |title: String, lines: Vec<Line<'static>>| {
        let _ = writeln!(out, "\n{title}");
        for line in lines {
            let _ = writeln!(out, "  {line}");
        }
    };
    section("Market".to_owned(), market_lines(status, screen.now_ms));
    section("Estimate".to_owned(), estimate_lines(status));
    for leg in &status.legs {
        section(
            format!("{} account {}", leg.side, leg.wallet),
            account_lines(leg, screen.now_ms, str::to_owned),
        );
    }
    section("Funds".to_owned(), funds_lines(status, screen.now_ms));
    section("Ledger".to_owned(), vec![ledger_line(&screen.totals)]);
    section(
        "Timeline (UTC, newest first)".to_owned(),
        screen.events.iter().take(REPORT_EVENTS).map(event_line).collect(),
    );
    out
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
    use crate::status::{
        AccountSnapshot, EstimateSnapshot, FeesSnapshot, FundsSnapshot, PositionSnapshot, RoundSnapshot,
        TradingSnapshot, TransferSnapshot,
    };
    use crate::strategy::decide::Side;

    /// 2026-10-10 08:46:40 UTC.
    const NOW: i64 = 1_791_622_000_000;
    const LONG: &str = "0x02b439b469daedb910fdbb6d7cdc875c38161a78";
    const SHORT: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
    const SESSION_KEY: &str = "0xd24a429da902726a53db059fb5bd4e411f788782";

    fn leg(side: Side, wallet: &str, position: Option<PositionSnapshot>) -> LegSnapshot {
        LegSnapshot {
            side,
            wallet: wallet.into(),
            session_key: SESSION_KEY.into(),
            account: Some(AccountSnapshot {
                available_usd: "1000.00".into(),
                queued_usd: "0.00".into(),
                session_key_expiry: Some(u64::try_from(NOW / 1000).unwrap() + 20 * 86_400),
                positions: position.into_iter().collect(),
            }),
            pending_intents: 0,
            has_wallet_key: false,
        }
    }

    fn round_estimate(figures: [&str; 5]) -> RoundEstimate {
        let [loser, winner, net, paper, cost] = figures.map(str::to_owned);
        RoundEstimate { loser_usd: loser, winner_usd: winner, net_usd: net, paper, usd_per_paper: Some(cost) }
    }

    /// A hedged round half way to its target, twelve minutes old, with every section reported.
    fn hedged_screen() -> Screen {
        let position = |is_long| PositionSnapshot {
            id: if is_long { "7" } else { "8" }.into(),
            is_long,
            entry: "82700.0".into(),
            bust: if is_long { "78604.4" } else { "86795.6" }.into(),
            margin_usd: "500.00".into(),
            leverage: 20,
            opened_at_ms: Some(u64::try_from(NOW).unwrap() - if is_long { 12 * 60_000 } else { 11 * 60_000 }),
        };
        Screen {
            now_ms: NOW,
            status: Some(Snapshot {
                at_ms: NOW - 400,
                dry_run: false,
                mode: Mode::Run,
                coin: "BTC".into(),
                close_bps: 100,
                phase: Phase::Hedged,
                decision: "wait".into(),
                summary: "hedged; price is inside the target".into(),
                mid: Some("83100.5".into()),
                move_bps: Some(48.4),
                round: Some(RoundSnapshot {
                    entry: "82700.0".into(),
                    close_below: "81873.0".into(),
                    close_above: "83527.0".into(),
                }),
                trading: Some(TradingSnapshot { trading_paused: false, paused_actions: Vec::new() }),
                queue_total_usd: Some("0.00".into()),
                legs: vec![
                    leg(Side::Long, LONG, Some(position(true))),
                    leg(Side::Short, SHORT, Some(position(false))),
                ],
                estimate: Some(EstimateSnapshot {
                    mint_rate: "100.00".into(),
                    at_target: round_estimate(["100.00", "86.38", "-13.61", "9800.00", "0.001389"]),
                    now: Some(round_estimate(["48.42", "40.84", "-7.58", "4745.16", "0.001597"])),
                    fees: Some(FeesSnapshot {
                        win_percent: "2.00".into(),
                        loss_percent: "2.00".into(),
                        liquidation_percent: "2.00".into(),
                    }),
                }),
                funds: Some(FundsSnapshot {
                    total_usd: "2000.00".into(),
                    difference_usd: "0.00".into(),
                    rounds_affordable: 2,
                    verdict: "enough".into(),
                    can_rebalance: false,
                }),
                transfer: None,
            }),
            mode: Mode::Run,
            events: vec![EventRow {
                at_ms: 1_791_621_936_000,
                kind: "decision".into(),
                summary: "hedged; price is inside the target".into(),
            }],
            totals: Totals { trades: 12, net_pnl_usd: -81.6, paper_minted: 58_800.0 },
        }
    }

    fn status(screen: &mut Screen) -> &mut Snapshot {
        screen.status.as_mut().unwrap()
    }

    /// The dashboard on the smallest terminal it promises to be readable on.
    fn render(screen: &Screen) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        terminal.draw(|frame| draw(frame, screen)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .map(|line| line.trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Asserts that the dashboard and the `status` text both show each of these.
    fn both_show(screen: &Screen, expected: &[&str]) {
        for (what, shown) in [("dashboard", render(screen)), ("status", text(screen))] {
            for expected in expected {
                assert!(shown.contains(expected), "the {what} is missing {expected:?} in:\n{shown}");
            }
        }
    }

    #[test]
    fn a_hedged_round_shows_market_estimate_accounts_funds_ledger_and_timeline() {
        let screen = hedged_screen();
        both_show(
            &screen,
            &[
                // Market
                "mid 83100.5 · entry 82700.0 · held 12m",
                "move +48.4 bps of ±100 target",
                "███████████████░░░░░░░░░░░░░░░ 48%",
                "closes at ≤ 81873.0 or ≥ 83527.0",
                "bust long 78604.4 · short 86795.6",
                "payout queue 0.00 USD · trading open",
                "relayer refuses nothing",
                // Estimate
                "mint rate 100.00 PAPER per USD of mint basis",
                "fees win 2.00% · loss 2.00% · liquidation 2.00%",
                "              loser    winner       net       PAPER  USD/PAPER",
                "at target   -100.00    +86.38    -13.61     9800.00   0.001389",
                "now          -48.42    +40.84     -7.58     4745.16   0.001597",
                "an estimate: the contract has the last word at settlement",
                // Accounts
                "available 1000.00 USD · queued payout 0.00 USD",
                "· 20d left",
                "wallet key absent",
                "in flight nothing",
                "long 500.00 USD × 20 @ 82700.0 · bust 78604.4",
                "short 500.00 USD × 20 @ 82700.0 · bust 86795.6",
                // Funds
                "total 2000.00 USD · difference 0.00 USD · rounds affordable 2 · verdict enough · transfers manual",
                "no transfer in progress",
                // Ledger and timeline
                "settled positions 12  net -81.60 USD  PAPER 58800  actual cost 0.00139 USD/PAPER",
                "10-10 08:45:36 decision hedged; price is inside the target",
            ],
        );
        let dashboard = render(&screen);
        for expected in [
            "RUN",
            "live",
            "HEDGED  WAIT ▸ hedged; price is inside the target",
            "LONG 0x02b4…1a78",
            "SHORT 0x7099…79c8",
            "session key 0xd24a…8782 · 20d left",
        ] {
            assert!(dashboard.contains(expected), "missing {expected:?} in:\n{dashboard}");
        }
        let report = text(&screen);
        for expected in [
            "mode      run\n",
            "bot       live\n",
            "market    BTC\n",
            "legs      hedged\n",
            "decision  wait: hedged; price is inside the target\n",
            &format!("long account {LONG}\n"),
            &format!("short account {SHORT}\n"),
            &format!("session key {SESSION_KEY} · 20d left"),
        ] {
            assert!(report.contains(expected), "missing {expected:?} in:\n{report}");
        }
    }

    #[test]
    fn a_stopped_bot_is_called_out() {
        let mut screen = hedged_screen();
        status(&mut screen).at_ms = NOW - 90_000;
        status(&mut screen).dry_run = true;
        screen.mode = Mode::Pause;
        let dashboard = render(&screen);
        assert!(dashboard.contains("BOT NOT RUNNING · last seen 1m ago"), "{dashboard}");
        assert!(dashboard.contains("PAUSE") && dashboard.contains("DRY RUN"), "{dashboard}");
        let report = text(&screen);
        assert!(report.contains("mode      pause (dry run)\n"), "{report}");
        assert!(report.contains("bot       NOT RUNNING · last seen 1m ago\n"), "{report}");
    }

    #[test]
    fn missing_data_is_shown_as_missing() {
        let mut screen = hedged_screen();
        let status = status(&mut screen);
        status.trading = None;
        status.queue_total_usd = None;
        status.mid = None;
        status.move_bps = None;
        status.round = None;
        status.estimate = None;
        status.funds = None;
        status.legs[0].account = None;
        let short = status.legs[1].account.as_mut().unwrap();
        short.session_key_expiry = None;
        short.positions[0].opened_at_ms = None;
        status.legs[1].pending_intents = 1;
        screen.totals = Totals::default();
        both_show(
            &screen,
            &[
                "mid no fresh quote · entry — · held —",
                "move unknown: no fresh price",
                "bust short 86795.6",
                "payout queue unknown",
                "trading state unavailable",
                "no estimate: an input is missing",
                "account state unavailable",
                "NOT REGISTERED",
                "1 intent(s) in flight",
                "funds —",
                "settled positions 0  net +0.00 USD  PAPER 0  actual cost n/a",
            ],
        );
        // Nothing that is unknown is passed off as a figure.
        let dashboard = render(&screen);
        for absent in ["closes at", "mint rate", "at target", "total "] {
            assert!(!dashboard.contains(absent), "{absent:?} should not be shown in:\n{dashboard}");
        }
    }

    #[test]
    fn an_estimate_says_which_of_its_figures_it_does_not_have() {
        let mut screen = hedged_screen();
        let estimate = status(&mut screen).estimate.as_mut().unwrap();
        estimate.now = None;
        estimate.fees = None;
        estimate.at_target.paper = "0.00".into();
        estimate.at_target.usd_per_paper = None;
        estimate.at_target.loser_usd = "0.00".into();
        both_show(
            &screen,
            &[
                "fees —",
                "at target      0.00    +86.38    -13.61        0.00          —",
                "now               —         —         —           —          —",
            ],
        );
    }

    #[test]
    fn a_flat_bot_shows_the_next_rounds_estimate_and_no_round() {
        let mut screen = hedged_screen();
        let status = status(&mut screen);
        status.phase = Phase::Flat;
        status.summary = "flat; settling before the next open".into();
        status.move_bps = None;
        status.round = None;
        status.estimate.as_mut().unwrap().now = None;
        for leg in &mut status.legs {
            leg.account.as_mut().unwrap().positions.clear();
            leg.has_wallet_key = true;
        }
        both_show(
            &screen,
            &[
                "no hedged round open",
                "at target   -100.00    +86.38    -13.61     9800.00   0.001389",
                "no position",
                "wallet key present",
            ],
        );
        let dashboard = render(&screen);
        for absent in ["entry", "held", "bust", "closes at"] {
            assert!(!dashboard.contains(absent), "{absent:?} should not be shown in:\n{dashboard}");
        }
    }

    #[test]
    fn a_transfer_under_way_shows_direction_amount_stage_time_and_note() {
        let mut screen = hedged_screen();
        let status = status(&mut screen);
        status.funds = Some(FundsSnapshot {
            total_usd: "1840.00".into(),
            difference_usd: "560.00".into(),
            rounds_affordable: 1,
            verdict: "rebalance".into(),
            can_rebalance: true,
        });
        status.transfer = Some(TransferSnapshot {
            from: Side::Long,
            to: Side::Short,
            amount_usd: "280.00".into(),
            stage: "credit".into(),
            started_at_ms: NOW - 200_000,
            note: Some("waiting for the deposit to be credited".into()),
        });
        both_show(
            &screen,
            &[
                "total 1840.00 USD · difference 560.00 USD · rounds affordable 1 · verdict rebalance · transfers automatic",
                "transfer long → short 280.00 USD · stage credit · 3m elapsed",
                "waiting for the deposit to be credited",
            ],
        );

        // A stuck transfer keeps its reason on screen, and a transfer may have nothing to add.
        let transfer = screen.status.as_mut().unwrap().transfer.as_mut().unwrap();
        transfer.stage = "stuck".into();
        transfer.note = Some("the withdrawal was refused three times: admission_rejected (funding)".into());
        both_show(
            &screen,
            &[
                "transfer long → short 280.00 USD · stage stuck · 3m elapsed",
                "the withdrawal was refused three times: admission_rejected (funding)",
            ],
        );
        screen.status.as_mut().unwrap().transfer.as_mut().unwrap().note = None;
        both_show(&screen, &["stage stuck · 3m elapsed", "settled positions 12"]);
    }

    #[test]
    fn an_expired_session_key_and_a_paused_protocol_stand_out() {
        let mut screen = hedged_screen();
        let status = status(&mut screen);
        status.legs[0].account.as_mut().unwrap().session_key_expiry =
            Some(u64::try_from(NOW / 1000).unwrap());
        status.trading = Some(TradingSnapshot {
            trading_paused: true,
            paused_actions: vec!["closePositions".into(), "openPosition".into()],
        });
        status.queue_total_usd = Some("1234.56".into());
        both_show(
            &screen,
            &[
                "EXPIRED",
                "payout queue 1234.56 USD · trading PAUSED",
                "relayer refuses closePositions, openPosition",
            ],
        );
    }

    #[test]
    fn everything_fits_110_columns_by_40_rows_with_room_for_the_timeline() {
        let mut screen = hedged_screen();
        // The widest things each panel can hold: six-figure prices, a refused relayer,
        // a transfer with a note, and a busy timeline.
        let status = status(&mut screen);
        status.mid = Some("123456.5".into());
        status.round = Some(RoundSnapshot {
            entry: "123400.0".into(),
            close_below: "122166.0".into(),
            close_above: "124634.0".into(),
        });
        for leg in &mut status.legs {
            let position = &mut leg.account.as_mut().unwrap().positions[0];
            position.entry = "123400.0".into();
            position.bust = if position.is_long { "117291.7" } else { "129508.3" }.into();
            position.margin_usd = "50000.00".into();
        }
        status.trading.as_mut().unwrap().paused_actions =
            vec!["closePositions".into(), "openPosition".into()];
        status.estimate.as_mut().unwrap().at_target =
            round_estimate(["10000.00", "8638.39", "-1361.60", "980000.00", "0.001389"]);
        status.transfer = Some(TransferSnapshot {
            from: Side::Short,
            to: Side::Long,
            amount_usd: "25000.00".into(),
            stage: "withdraw".into(),
            started_at_ms: NOW - 45_000,
            note: Some("waiting for the withdrawal to reach the wallet's Hyperliquid account".into()),
        });
        screen.events = (0..30)
            .map(|n| EventRow {
                at_ms: NOW - n * 1000,
                kind: "decision".into(),
                summary: format!("event {n}"),
            })
            .collect();
        let dashboard = render(&screen);
        for expected in [
            "mid 123456.5 · entry 123400.0 · held 12m",
            "closes at ≤ 122166.0 or ≥ 124634.0",
            "bust long 117291.7 · short 129508.3",
            "relayer refuses closePositions, openPosition",
            "at target -10000.00  +8638.39  -1361.60   980000.00   0.001389",
            "an estimate: the contract has the last word at settlement",
            "long 50000.00 USD × 20 @ 123400.0 · bust 117291.7",
            "short 50000.00 USD × 20 @ 123400.0 · bust 129508.3",
            "transfer short → long 25000.00 USD · stage withdraw · 45s elapsed",
            "waiting for the withdrawal to reach the wallet's Hyperliquid account",
            "settled positions 12",
            "q quit · p pause/resume",
        ] {
            assert!(dashboard.contains(expected), "missing {expected:?} in:\n{dashboard}");
        }
        let events = dashboard.lines().filter(|line| line.contains(" decision event ")).count();
        assert!(events >= 8, "only {events} timeline entries fit in:\n{dashboard}");
    }

    #[test]
    fn status_prints_only_the_latest_events() {
        let mut screen = hedged_screen();
        screen.events = (0..30)
            .map(|n| EventRow { at_ms: NOW - n * 1000, kind: "intent".into(), summary: format!("event {n}") })
            .collect();
        let report = text(&screen);
        assert!(report.contains("event 9\n") && !report.contains("event 10"), "{report}");
    }

    #[tokio::test]
    async fn the_report_is_read_from_the_database() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(report(&store).await.unwrap(), format!("{NO_STATUS}\n"));

        let mut snapshot = hedged_screen().status.unwrap();
        snapshot.at_ms = now_ms();
        store.write_status(&snapshot).await.unwrap();
        store.set_mode(Mode::Pause).await.unwrap();
        let report = report(&store).await.unwrap();
        for expected in [
            "mode      pause\n",
            "bot       live\n",
            "at target   -100.00    +86.38    -13.61     9800.00   0.001389",
            "settled positions 0  net +0.00 USD  PAPER 0  actual cost n/a",
            "mode     operator set mode to pause",
        ] {
            assert!(report.contains(expected), "missing {expected:?} in:\n{report}");
        }
    }

    #[test]
    fn before_the_first_status_it_says_how_to_start() {
        let screen = Screen { status: None, ..hedged_screen() };
        assert!(render(&screen).contains("Start the bot with `papertrade run`"));
        assert_eq!(text(&screen), "No status yet. Start the bot with `papertrade run`.\n");
    }

    #[test]
    fn the_bar_fills_with_the_share_of_the_target_reached() {
        assert_eq!(bar(0.0, 10), "░░░░░░░░░░");
        assert_eq!(bar(0.5, 10), "█████░░░░░");
        assert_eq!(bar(1.0, 10), "██████████");
        assert_eq!(bar(7.0, 4), "████");
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
