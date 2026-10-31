//! Rendering. Takes `&mut App` only to clamp the transcript scroll once the wrapped height is known.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::api::{Message, ModelInfo};
use crate::app::{App, Confirm, Conn, Focus, Overlay};
use crate::wrap::wrap;

const DIM: Style = Style::new().fg(Color::DarkGray);
const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// "now", "5m", "3h", "12d", "4mo", "2y" from milliseconds.
pub fn format_age(now_ms: i64, then_ms: i64) -> String {
    let secs = ((now_ms - then_ms) / 1000).max(0);
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=2_591_999 => format!("{}d", secs / 86_400),
        2_592_000..=31_535_999 => format!("{}mo", secs / 2_592_000),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// Cuts `text` to at most `width` columns, ending in an ellipsis when it was cut.
pub fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn short_model(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn tier_style(model: &ModelInfo) -> Style {
    if model.costs_or_leaves() {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if model.tier == "free" {
        Style::new().fg(Color::Green)
    } else {
        DIM
    }
}

fn tier_label(model: &ModelInfo) -> &'static str {
    if model.remote {
        "cloud"
    } else if model.tier == "paid" {
        "PAID"
    } else if model.tier == "free" {
        "free"
    } else {
        "local"
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    match &app.conn {
        Conn::Connecting => {
            let text = format!(
                "{} connecting to {}",
                SPINNER[(app.tick / 2) as usize % 4],
                app.base
            );
            frame.render_widget(
                Paragraph::new(Line::styled(text, DIM)),
                centered(area, area.width, 1),
            );
            return;
        }
        Conn::Failed(error) => {
            let lines = vec![
                Line::styled(
                    "aihubui can't reach aihub-web",
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Line::raw(error.clone()),
                Line::raw(""),
                Line::raw("start it:  systemctl --user start aihub-web"),
                Line::raw(format!("check it:  curl -sf {}/api/health", app.base)),
                Line::raw(""),
                Line::styled("r retry   q quit", DIM),
            ];
            let height = lines.len() as u16;
            frame.render_widget(
                Paragraph::new(lines),
                centered(area, area.width.min(80), height),
            );
            return;
        }
        Conn::Ready => {}
    }

    let list_width = (area.width / 3).clamp(20, 36);
    let [left, right] =
        Layout::horizontal([Constraint::Length(list_width), Constraint::Min(10)]).areas(area);
    render_list(frame, app, left);

    let compose_h = compose_height(app, right.width.saturating_sub(2) as usize);
    let [transcript, compose, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(compose_h),
        Constraint::Length(1),
    ])
    .areas(right);
    render_transcript(frame, app, transcript);
    render_compose(frame, app, compose);
    render_status(frame, app, status);
    render_overlay(frame, app, area);
}

fn render_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .chats
        .iter()
        .map(|c| {
            ListItem::new(vec![
                Line::raw(truncate(&c.title, inner)),
                Line::styled(
                    truncate(
                        &format!(
                            "{} · {}",
                            short_model(&c.model),
                            format_age(app.now_ms, c.updated_at)
                        ),
                        inner,
                    ),
                    DIM,
                ),
            ])
        })
        .collect();
    let title = if app.search.is_empty() {
        format!(" Chats ({}) ", app.chats.len())
    } else {
        format!(
            " Chats matching \"{}\" ({}) ",
            truncate(&app.search, 12),
            app.chats.len()
        )
    };
    let focused = app.focus == Focus::List && app.overlay == Overlay::None;
    let list = List::new(items)
        .block(Block::bordered().title(title).border_style(if focused {
            Style::new().fg(Color::Cyan)
        } else {
            DIM
        }))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut app.list);
}

fn image_note(image: &serde_json::Value) -> String {
    match (
        image.get("w").and_then(|v| v.as_u64()),
        image.get("h").and_then(|v| v.as_u64()),
    ) {
        (Some(w), Some(h)) => format!("[image {w}x{h}, not shown here]"),
        _ => "[image, not shown here]".to_string(),
    }
}

fn header(message: &Message) -> Line<'static> {
    let mut spans = if message.role == "user" {
        vec![Span::styled(
            "you",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]
    } else {
        let name = message
            .model
            .as_deref()
            .map(short_model)
            .unwrap_or("assistant")
            .to_string();
        vec![Span::styled(
            name,
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        )]
    };
    match message.status.as_str() {
        "stopped" => spans.push(Span::styled("  (stopped)", DIM)),
        "error" => spans.push(Span::styled("  (failed)", Style::new().fg(Color::Red))),
        _ => {}
    }
    Line::from(spans)
}

fn body(text: &str, width: usize, lines: &mut Vec<Line<'static>>) {
    for line in wrap(text, width.saturating_sub(2).max(1)) {
        lines.push(Line::raw(format!("  {line}")));
    }
}

fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for m in &app.messages {
        lines.push(header(m));
        body(&m.content, width, &mut lines);
        for image in m.images.iter().flatten() {
            lines.push(Line::styled(format!("  {}", image_note(image)), DIM));
        }
        if let Some(e) = &m.error {
            lines.push(Line::styled(
                format!("  error: {}", e.message),
                Style::new().fg(Color::Red),
            ));
        }
        lines.push(Line::raw(""));
    }
    if let (Some(text), Some(_)) = (&app.pending_user, &app.stream) {
        lines.push(Line::from(Span::styled(
            "you",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        body(text, width, &mut lines);
        lines.push(Line::raw(""));
    }
    if let Some(stream) = &app.stream {
        let name = app
            .current_model()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "assistant".to_string());
        lines.push(Line::from(Span::styled(
            name,
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        )));
        if stream.text.is_empty() {
            let spin = SPINNER[(app.tick / 2) as usize % 4];
            let note = if stream.think_chars > 0 {
                format!("  {spin} thinking... ({} characters)", stream.think_chars)
            } else {
                format!("  {spin} waiting for the model...")
            };
            lines.push(Line::styled(note, DIM));
        } else {
            body(&format!("{}▍", stream.text), width, &mut lines);
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "New chat. Type a message and press Enter.",
            DIM,
        ));
        if let Some(m) = app.current_model() {
            lines.push(Line::styled(
                format!(
                    "Using {} ({}). Ctrl+O changes the model.",
                    m.name,
                    tier_label(m)
                ),
                DIM,
            ));
        }
    }
    lines
}

fn render_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = match &app.current {
        Some(c) => format!(
            " {} ",
            truncate(&c.title, area.width.saturating_sub(6) as usize)
        ),
        None => " New chat ".to_string(),
    };
    let block = Block::bordered().title(title).border_style(DIM);
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines = transcript_lines(app, inner_w);
    let max_back = lines.len().saturating_sub(inner_h);
    app.scroll_back = app.scroll_back.min(max_back);
    let end = lines.len() - app.scroll_back;
    let start = end.saturating_sub(inner_h);
    let mut visible: Vec<Line> = lines[start..end].to_vec();
    if app.scroll_back > 0 && !visible.is_empty() {
        let last = visible.len() - 1;
        visible[last] = Line::styled(format!("-- {} lines below (PgDn) --", app.scroll_back), DIM);
    }
    frame.render_widget(Paragraph::new(visible).block(block), area);
}

fn input_lines(app: &App, width: usize) -> Vec<String> {
    wrap(&app.input, width.max(1))
}

fn compose_height(app: &App, inner_width: usize) -> u16 {
    let lines = if app.input.is_empty() {
        1
    } else {
        input_lines(app, inner_width.saturating_sub(0)).len()
    };
    lines.clamp(1, 6) as u16 + 2
}

fn render_compose(frame: &mut Frame, app: &App, area: Rect) {
    let mut title = vec![Span::raw(" ")];
    match app.current_model() {
        Some(m) => {
            title.push(Span::raw(m.name.clone()));
            title.push(Span::raw(" "));
            title.push(Span::styled(format!("[{}]", tier_label(m)), tier_style(m)));
            if let Some(e) = app.effective_effort() {
                title.push(Span::styled(format!(" intensity {e}"), DIM));
            }
        }
        None => title.push(Span::styled(
            "no model available",
            Style::new().fg(Color::Red),
        )),
    }
    title.push(Span::raw(" "));
    let focused = app.focus == Focus::Compose && app.overlay == Overlay::None;
    let block = Block::bordered()
        .title(Line::from(title))
        .border_style(if focused {
            Style::new().fg(Color::Cyan)
        } else {
            DIM
        });
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;

    if app.input.is_empty() {
        let hint = if app.stream.is_some() {
            "reply in progress: Esc stops it"
        } else {
            "Type a message"
        };
        frame.render_widget(Paragraph::new(Line::styled(hint, DIM)).block(block), area);
        if focused {
            frame.set_cursor_position(Position::new(area.x + 1, area.y + 1));
        }
        return;
    }
    let lines = input_lines(app, inner_w);
    let skip = lines.len().saturating_sub(inner_h);
    let shown: Vec<Line> = lines[skip..].iter().map(|l| Line::raw(l.clone())).collect();
    frame.render_widget(Paragraph::new(shown).block(block), area);
    if focused {
        let last = lines
            .last()
            .map(|l| l.width())
            .unwrap_or(0)
            .min(inner_w.saturating_sub(1));
        let row = (lines.len() - skip).saturating_sub(1);
        frame.set_cursor_position(Position::new(
            area.x + 1 + last as u16,
            area.y + 1 + row as u16,
        ));
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let line = if let Some(status) = &app.status {
        let style = if status.is_error {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Yellow)
        };
        Line::styled(truncate(&status.text, area.width as usize), style)
    } else {
        let hints = match (app.focus, app.stream.is_some()) {
            (_, true) if app.focus == Focus::Compose => {
                "reply in progress   Esc stop   PgUp/PgDn scroll"
            }
            (Focus::List, _) => {
                "j/k move  Enter open  n new  d delete  / search  m model  e intensity  Tab write  q quit"
            }
            (Focus::Compose, _) => {
                "Enter send  Alt+Enter newline  Esc back  Ctrl+O model  Ctrl+E intensity  PgUp/PgDn scroll"
            }
        };
        Line::styled(truncate(hints, area.width as usize), DIM)
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_overlay(frame: &mut Frame, app: &App, area: Rect) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::Models { cursor } => {
            let height = (app.models.len() as u16 + 4).min(area.height.saturating_sub(2));
            let popup = centered(area, 72.min(area.width.saturating_sub(4)), height);
            frame.render_widget(Clear, popup);
            let inner_w = popup.width.saturating_sub(4) as usize;
            let items: Vec<ListItem> = app
                .models
                .iter()
                .map(|m| {
                    let name_style = if m.available { Style::new() } else { DIM };
                    let marker = if app.model.as_deref() == Some(m.id.as_str()) {
                        "* "
                    } else {
                        "  "
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(marker),
                        Span::styled(truncate(&m.name, inner_w.saturating_sub(14)), name_style),
                        Span::raw("  "),
                        Span::styled(tier_label(m), tier_style(m)),
                        Span::styled(if m.available { "" } else { "  unavailable" }, DIM),
                    ]))
                })
                .collect();
            let mut state = ratatui::widgets::ListState::default().with_selected(Some(*cursor));
            let list = List::new(items)
                .block(Block::bordered().title(" Model (Enter pick, Esc close) "))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
            frame.render_stateful_widget(list, popup, &mut state);
            if let Some(m) = app.models.get(*cursor).filter(|m| !m.available) {
                let reason = m.reason.clone().unwrap_or_default();
                let line = Rect::new(
                    popup.x + 1,
                    popup.y + popup.height - 1,
                    popup.width.saturating_sub(2),
                    1,
                );
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        truncate(&reason, line.width as usize),
                        Style::new().fg(Color::Red),
                    )),
                    line,
                );
            }
        }
        Overlay::Confirm(confirm) => {
            let lines = match confirm {
                Confirm::DeleteChat { title, .. } => vec![
                    Line::from(vec![
                        Span::raw("Delete "),
                        Span::styled(
                            format!("\"{}\"", truncate(title, 40)),
                            Style::new().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("?"),
                    ]),
                    Line::styled("This cannot be undone.", DIM),
                ],
                Confirm::PaidSend { model } => {
                    let info = app.models.iter().find(|m| &m.id == model);
                    let name = info
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| model.clone());
                    let why = if info.is_some_and(|m| m.remote) {
                        "runs on Ollama's servers: your message leaves this machine and may be billed."
                    } else {
                        "is a paid model: this message will cost money."
                    };
                    vec![
                        Line::from(vec![
                            Span::styled(
                                name,
                                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(format!(" {why}")),
                        ]),
                        Line::styled(
                            "You won't be asked again for this model until aihubui restarts.",
                            DIM,
                        ),
                    ]
                }
            };
            let popup = centered(area, 78.min(area.width.saturating_sub(4)), 6);
            frame.render_widget(Clear, popup);
            let mut all = lines;
            all.push(Line::raw(""));
            all.push(Line::styled(
                "y yes    n no",
                Style::new().add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(
                Paragraph::new(all)
                    .wrap(ratatui::widgets::Wrap { trim: true })
                    .block(Block::bordered().title(" Confirm ")),
                popup,
            );
        }
        Overlay::Search(input) => {
            let popup = centered(area, 60.min(area.width.saturating_sub(4)), 3);
            frame.render_widget(Clear, popup);
            frame.render_widget(
                Paragraph::new(input.clone())
                    .block(Block::bordered().title(" Search chats (Enter apply, Esc cancel) ")),
                popup,
            );
            let col = (input.width() as u16).min(popup.width.saturating_sub(3));
            frame.set_cursor_position(Position::new(popup.x + 1 + col, popup.y + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Chat, Efforts, MessageError};
    use crate::app::{Cmd, Streaming};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn draw(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn model(id: &str, tier: &str, available: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.rsplit('/').next().unwrap().into(),
            tier: tier.into(),
            efforts: None,
            available,
            reason: if available {
                None
            } else {
                Some("Set AIHUB_OPENROUTER_KEY in ~/.config/aihub/aihub.env".into())
            },
            output: None,
            remote: false,
        }
    }

    fn chat(id: &str, title: &str, model: &str) -> Chat {
        Chat {
            id: id.into(),
            title: title.into(),
            model: model.into(),
            effort: None,
            created_at: 0,
            updated_at: 1_000_000,
            snippet: None,
        }
    }

    fn msg(role: &str, content: &str) -> Message {
        Message {
            id: "m".into(),
            chat_id: "c1".into(),
            role: role.into(),
            content: content.into(),
            model: if role == "assistant" {
                Some("ollama/qwen3:8b".into())
            } else {
                None
            },
            status: "done".into(),
            error: None,
            created_at: 0,
            images: None,
        }
    }

    fn app() -> App {
        let mut a = App::new("http://127.0.0.1:8787".into());
        a.conn = Conn::Ready;
        a.now_ms = 1_000_000 + 3 * 3_600_000;
        a.models = vec![
            ModelInfo {
                efforts: Some(Efforts {
                    levels: vec!["low".into(), "high".into()],
                    default: Some("high".into()),
                }),
                ..model("ollama/qwen3:8b", "local", true)
            },
            model("anthropic/opus", "paid", true),
            model("openrouter/free-one", "free", false),
        ];
        a.model = Some("ollama/qwen3:8b".into());
        a.chats = vec![
            chat("c1", "Eagle and squirrel", "ollama/qwen3:8b"),
            chat("c2", "日本語のタイトル", "images/zimage"),
        ];
        a.list.select(Some(0));
        a
    }

    fn press(a: &mut App, code: KeyCode) -> Vec<Cmd> {
        a.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn ages_use_the_largest_unit() {
        assert_eq!(format_age(10_000, 10_000), "now");
        assert_eq!(format_age(10_000_000, 9_700_000), "5m");
        assert_eq!(format_age(3 * 3_600_000, 0), "3h");
        assert_eq!(format_age(12 * 86_400_000, 0), "12d");
        assert_eq!(format_age(0, 5), "now");
    }

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a longer title", 8), "a longe…");
        assert!(truncate("日本語のタイトル", 7).width() <= 7);
        assert_eq!(truncate("x", 0), "…");
    }

    #[test]
    fn connecting_and_failed_screens_say_what_to_do() {
        let mut a = App::new("http://127.0.0.1:8787".into());
        assert!(draw(&mut a, 80, 10).contains("connecting to http://127.0.0.1:8787"));
        a.conn = Conn::Failed("cannot reach aihub-web at http://127.0.0.1:8787".into());
        let screen = draw(&mut a, 80, 12);
        assert!(screen.contains("can't reach aihub-web"), "{screen}");
        assert!(
            screen.contains("systemctl --user start aihub-web"),
            "{screen}"
        );
        assert!(
            screen.contains("curl -sf http://127.0.0.1:8787/api/health"),
            "{screen}"
        );
    }

    #[test]
    fn ready_screen_shows_chats_model_and_hints() {
        let mut a = app();
        let screen = draw(&mut a, 110, 24);
        assert!(screen.contains("Chats (2)"), "{screen}");
        assert!(screen.contains("Eagle and squirrel"), "{screen}");
        assert!(screen.contains("qwen3:8b · 3h"), "{screen}");
        assert!(screen.contains("New chat"), "{screen}");
        assert!(
            screen.contains("qwen3:8b [local] intensity high"),
            "{screen}"
        );
        assert!(screen.contains("Enter open"), "{screen}");
    }

    #[test]
    fn transcript_shows_messages_images_and_errors() {
        let mut a = app();
        let mut failed = msg("assistant", "half an ans");
        failed.status = "error".into();
        failed.error = Some(MessageError {
            code: "x".into(),
            message: "model crashed".into(),
        });
        let mut pic = msg("assistant", "");
        pic.images = Some(vec![serde_json::json!({"w": 1024, "h": 768})]);
        a.messages = vec![msg("user", "draw an eagle"), pic, failed];
        a.current = Some(chat("c1", "Eagle and squirrel", "ollama/qwen3:8b"));
        let screen = draw(&mut a, 110, 30);
        assert!(screen.contains("you"), "{screen}");
        assert!(screen.contains("draw an eagle"), "{screen}");
        assert!(
            screen.contains("[image 1024x768, not shown here]"),
            "{screen}"
        );
        assert!(screen.contains("(failed)"), "{screen}");
        assert!(screen.contains("error: model crashed"), "{screen}");
    }

    #[test]
    fn a_streaming_reply_shows_text_and_a_cursor_or_a_spinner() {
        let mut a = app();
        a.stream = Some(Streaming::default());
        a.pending_user = Some("hello".into());
        a.focus = Focus::Compose;
        let waiting = draw(&mut a, 110, 24);
        assert!(waiting.contains("waiting for the model"), "{waiting}");
        assert!(waiting.contains("hello"), "{waiting}");
        assert!(waiting.contains("reply in progress"), "{waiting}");

        a.stream = Some(Streaming {
            text: String::new(),
            think_chars: 42,
        });
        assert!(draw(&mut a, 110, 24).contains("thinking... (42 characters)"));

        a.stream = Some(Streaming {
            text: "Hi th".into(),
            think_chars: 0,
        });
        assert!(draw(&mut a, 110, 24).contains("Hi th▍"));
    }

    #[test]
    fn long_transcripts_show_the_newest_lines_and_a_scroll_marker() {
        let mut a = app();
        a.messages = (0..60)
            .map(|i| msg("user", &format!("message number {i}")))
            .collect();
        let bottom = draw(&mut a, 100, 24);
        assert!(bottom.contains("message number 59"), "{bottom}");
        assert!(!bottom.contains("message number 0\n"), "{bottom}");
        a.scroll_back = 30;
        let up = draw(&mut a, 100, 24);
        assert!(!up.contains("message number 59"), "{up}");
        assert!(up.contains("lines below"), "{up}");
        a.scroll_back = 10_000;
        draw(&mut a, 100, 24);
        assert!(
            a.scroll_back < 10_000,
            "scroll is clamped to the transcript"
        );
    }

    #[test]
    fn long_input_grows_the_box_up_to_six_lines_and_shows_the_tail() {
        let mut a = app();
        a.focus = Focus::Compose;
        a.input = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let screen = draw(&mut a, 100, 30);
        assert!(screen.contains("line 19"), "{screen}");
        assert!(
            !screen.contains("line 0\n") && !screen.contains("line 0 "),
            "{screen}"
        );
    }

    #[test]
    fn model_picker_marks_tiers_and_explains_unavailable_models() {
        let mut a = app();
        press(&mut a, KeyCode::Char('m'));
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("Model (Enter pick, Esc close)"), "{screen}");
        assert!(screen.contains("* qwen3:8b"), "{screen}");
        assert!(screen.contains("PAID"), "{screen}");
        assert!(screen.contains("unavailable"), "{screen}");
        for _ in 0..5 {
            press(&mut a, KeyCode::Down);
        }
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("Set AIHUB_OPENROUTER_KEY"), "{screen}");
    }

    #[test]
    fn confirmations_say_what_will_happen() {
        let mut a = app();
        press(&mut a, KeyCode::Char('d'));
        let screen = draw(&mut a, 100, 24);
        assert!(
            screen.contains("Delete \"Eagle and squirrel\"?"),
            "{screen}"
        );
        assert!(screen.contains("y yes"), "{screen}");
        press(&mut a, KeyCode::Char('n'));

        a.focus = Focus::Compose;
        a.model = Some("anthropic/opus".into());
        a.input = "hi".into();
        press(&mut a, KeyCode::Enter);
        let screen = draw(&mut a, 100, 24);
        assert!(
            screen.contains("opus is a paid model: this message will cost money."),
            "{screen}"
        );
    }

    #[test]
    fn search_box_shows_what_was_typed() {
        let mut a = app();
        press(&mut a, KeyCode::Char('/'));
        for c in "eagle".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("Search chats"), "{screen}");
        assert!(screen.contains("eagle"), "{screen}");
        press(&mut a, KeyCode::Enter);
        assert!(draw(&mut a, 100, 24).contains("Chats matching \"eagle\""));
    }

    #[test]
    fn status_line_shows_errors() {
        let mut a = app();
        a.focus = Focus::Compose;
        a.model = Some("openrouter/free-one".into());
        a.input = "hi".into();
        press(&mut a, KeyCode::Enter);
        assert!(draw(&mut a, 120, 24).contains("free-one can't be used: Set AIHUB_OPENROUTER_KEY"));
    }

    #[test]
    fn tiny_terminals_do_not_panic_in_any_state() {
        for (w, h) in [(1, 1), (10, 3), (30, 8), (50, 12)] {
            let mut a = app();
            a.messages = vec![msg("user", "x".repeat(300).as_str())];
            draw(&mut a, w, h);
            a.stream = Some(Streaming {
                text: "y".repeat(200),
                think_chars: 1,
            });
            draw(&mut a, w, h);
            press(&mut a, KeyCode::Char('m'));
            draw(&mut a, w, h);
            press(&mut a, KeyCode::Esc);
            press(&mut a, KeyCode::Char('d'));
            draw(&mut a, w, h);
            a.overlay = Overlay::Search("q".repeat(100));
            draw(&mut a, w, h);
            a.conn = Conn::Failed("e".repeat(300));
            draw(&mut a, w, h);
        }
    }
}
