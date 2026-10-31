//! Application state. Keys and server replies go in; the side effects the event loop must run come
//! out as `Cmd`s, so every mode and the guards around sending can be tested without a terminal or a server.

use std::collections::HashSet;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::api::{Chat, ChatDetail, Event, Message, ModelInfo, Models, SendRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    Connecting,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Compose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    DeleteChat { id: String, title: String },
    PaidSend { model: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    Models { cursor: usize },
    Confirm(Confirm),
    Search(String),
}

/// A side effect for the event loop to run.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    LoadModels,
    LoadChats(String),
    LoadChat(String),
    Send(SendRequest),
    /// Cut the reply connection. The id, when there is one, is the chat to reload afterwards so the
    /// partial reply the server stored is shown.
    Stop(Option<String>),
    Delete(String),
    Quit,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Streaming {
    pub text: String,
    pub think_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    pub is_error: bool,
}

/// A chat load in flight. `reset_model` is true when the user opened the chat (adopt its model and
/// effort) and false when reloading after a stop or a dropped stream (keep what is selected).
struct Opening {
    id: String,
    reset_model: bool,
}

pub struct App {
    pub base: String,
    pub conn: Conn,
    pub chats: Vec<Chat>,
    pub list: ListState,
    pub current: Option<Chat>,
    pub messages: Vec<Message>,
    /// Text models only, available ones first, then by tier (local, free, paid) and name.
    pub models: Vec<ModelInfo>,
    pub default_model: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub input: String,
    pub focus: Focus,
    pub overlay: Overlay,
    pub stream: Option<Streaming>,
    /// The text just sent, shown until the server confirms it, and restored to the input if the send fails.
    pub pending_user: Option<String>,
    /// Lines scrolled up from the newest message; 0 follows the end.
    pub scroll_back: usize,
    pub status: Option<Status>,
    pub search: String,
    pub tick: u64,
    /// Wall-clock milliseconds since the epoch, set by the event loop before each draw.
    pub now_ms: i64,
    paid_ok: HashSet<String>,
    opening: Option<Opening>,
    select_after_load: Option<String>,
}

fn tier_rank(tier: &str) -> u8 {
    match tier {
        "local" => 0,
        "free" => 1,
        _ => 2,
    }
}

impl App {
    pub fn new(base: String) -> Self {
        App {
            base,
            conn: Conn::Connecting,
            chats: Vec::new(),
            list: ListState::default(),
            current: None,
            messages: Vec::new(),
            models: Vec::new(),
            default_model: None,
            model: None,
            effort: None,
            input: String::new(),
            focus: Focus::List,
            overlay: Overlay::None,
            stream: None,
            pending_user: None,
            scroll_back: 0,
            status: None,
            search: String::new(),
            tick: 0,
            now_ms: 0,
            paid_ok: HashSet::new(),
            opening: None,
            select_after_load: None,
        }
    }

    pub fn start_cmds(&mut self) -> Vec<Cmd> {
        self.conn = Conn::Connecting;
        vec![Cmd::LoadModels, Cmd::LoadChats(self.search.clone())]
    }

    fn info(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn current_model(&self) -> Option<&ModelInfo> {
        self.model.as_deref().and_then(|id| self.info(id))
    }

    pub fn selected_chat(&self) -> Option<&Chat> {
        self.list.selected().and_then(|i| self.chats.get(i))
    }

    /// The effort that will be used: the one picked, else the model's default.
    pub fn effective_effort(&self) -> Option<String> {
        let efforts = self.current_model()?.efforts.as_ref()?;
        self.effort
            .clone()
            .filter(|e| efforts.levels.contains(e))
            .or_else(|| efforts.default.clone())
    }

    fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            is_error: false,
        });
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            is_error: true,
        });
    }

    // ---- server replies ----

    pub fn apply_models(&mut self, result: Result<Models, String>) {
        match result {
            Ok(payload) => {
                let mut models: Vec<ModelInfo> =
                    payload.models.into_iter().filter(|m| m.is_text()).collect();
                models.sort_by_key(|m| (!m.available, tier_rank(&m.tier), m.name.to_lowercase()));
                self.models = models;
                self.default_model = payload.default_model;
                if self.model.as_ref().is_none_or(|m| self.info(m).is_none()) {
                    self.model = self
                        .default_model
                        .clone()
                        .filter(|m| self.info(m).is_some());
                }
                self.conn = Conn::Ready;
            }
            Err(e) if self.models.is_empty() => self.conn = Conn::Failed(e),
            Err(e) => self.set_error(e),
        }
    }

    pub fn apply_chats(&mut self, result: Result<Vec<Chat>, String>) {
        match result {
            Ok(chats) => {
                let keep = self
                    .select_after_load
                    .take()
                    .or_else(|| self.selected_chat().map(|c| c.id.clone()));
                self.chats = chats;
                let index = keep
                    .and_then(|id| self.chats.iter().position(|c| c.id == id))
                    .or(if self.chats.is_empty() { None } else { Some(0) });
                self.list.select(index);
            }
            Err(e) => self.set_error(e),
        }
    }

    pub fn apply_chat(&mut self, id: &str, result: Result<ChatDetail, String>) {
        let Some(opening) = self.opening.as_ref().filter(|o| o.id == id) else {
            return; // the user has moved on; this reply is stale
        };
        let reset_model = opening.reset_model;
        self.opening = None;
        match result {
            Ok(detail) => {
                if reset_model {
                    let usable = self.info(&detail.chat.model).is_some_and(|m| m.available);
                    self.model = if usable {
                        Some(detail.chat.model.clone())
                    } else {
                        self.default_model.clone()
                    };
                    self.effort = detail.chat.effort.clone();
                }
                self.messages = detail.messages;
                self.current = Some(detail.chat);
                self.scroll_back = 0;
            }
            Err(e) => self.set_error(e),
        }
    }

    pub fn apply_event(&mut self, event: Event) -> Vec<Cmd> {
        if self.stream.is_none() {
            return vec![]; // a late event from a reply that was stopped
        }
        match event {
            Event::Chat { chat, user } => {
                let is_new = self.current.as_ref().is_none_or(|c| c.id != chat.id);
                if is_new {
                    self.select_after_load = Some(chat.id.clone());
                }
                self.current = Some(chat);
                if let Some(user) = user {
                    self.messages.push(user);
                }
                self.pending_user = None;
                self.scroll_back = 0;
                if is_new {
                    vec![Cmd::LoadChats(self.search.clone())]
                } else {
                    vec![]
                }
            }
            Event::Note { dropped } => {
                self.set_status(format!(
                    "{dropped} earlier message(s) were left out to fit the model's context"
                ));
                vec![]
            }
            Event::Think(d) => {
                if let Some(s) = self.stream.as_mut() {
                    s.think_chars += d.chars().count();
                }
                vec![]
            }
            Event::Text(d) => {
                if let Some(s) = self.stream.as_mut() {
                    s.text.push_str(&d);
                }
                vec![]
            }
            Event::Done(message) => {
                self.messages.push(message);
                self.stream = None;
                vec![Cmd::LoadChats(self.search.clone())]
            }
            Event::Error(message) => {
                if let Some(e) = &message.error {
                    self.set_error(e.message.clone());
                }
                self.messages.push(message);
                self.stream = None;
                vec![Cmd::LoadChats(self.search.clone())]
            }
            Event::Progress | Event::Unknown(_) => vec![],
        }
    }

    pub fn stream_ended(&mut self, result: Result<(), String>) -> Vec<Cmd> {
        let Err(e) = result else {
            self.stream = None;
            return vec![];
        };
        if self.stream.is_none() {
            return vec![]; // already stopped by the user
        }
        self.stream = None;
        self.set_error(e);
        if let Some(text) = self.pending_user.take() {
            self.input = text; // the server never saw it: give it back
        }
        match &self.current {
            Some(chat) => {
                let id = chat.id.clone();
                self.opening = Some(Opening {
                    id: id.clone(),
                    reset_model: false,
                });
                vec![Cmd::LoadChat(id)]
            }
            None => vec![],
        }
    }

    pub fn deleted(&mut self, id: &str, result: Result<(), String>) -> Vec<Cmd> {
        match result {
            Ok(()) => {
                if self.current.as_ref().is_some_and(|c| c.id == id) {
                    self.reset_conversation();
                }
                self.set_status("chat deleted");
                vec![Cmd::LoadChats(self.search.clone())]
            }
            Err(e) => {
                self.set_error(e);
                vec![]
            }
        }
    }

    fn reset_conversation(&mut self) {
        self.current = None;
        self.messages.clear();
        self.effort = None;
        self.model = self
            .default_model
            .clone()
            .filter(|m| self.info(m).is_some());
        self.scroll_back = 0;
        self.opening = None;
    }

    // ---- keys ----

    pub fn on_paste(&mut self, text: &str) {
        if self.focus == Focus::Compose && self.overlay == Overlay::None {
            self.input.push_str(&text.replace('\r', "\n"));
        } else if let Overlay::Search(s) = &mut self.overlay {
            s.push_str(&text.replace(['\r', '\n'], " "));
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if key.kind != KeyEventKind::Press {
            return vec![];
        }
        self.status = None;
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return if self.stream.is_some() {
                self.stop()
            } else {
                vec![Cmd::Quit]
            };
        }
        match self.overlay.clone() {
            Overlay::Models { cursor } => self.models_key(key, cursor),
            Overlay::Confirm(confirm) => self.confirm_key(key, confirm),
            Overlay::Search(input) => self.search_key(key, input),
            Overlay::None => match self.focus {
                Focus::List => self.list_key(key),
                Focus::Compose => self.compose_key(key),
            },
        }
    }

    fn stop(&mut self) -> Vec<Cmd> {
        self.stream = None;
        if let Some(text) = self.pending_user.take() {
            self.input = text;
        }
        match &self.current {
            Some(chat) => {
                let id = chat.id.clone();
                self.opening = Some(Opening {
                    id: id.clone(),
                    reset_model: false,
                });
                vec![Cmd::Stop(Some(id))]
            }
            None => vec![Cmd::Stop(None)],
        }
    }

    fn scroll_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            _ => return false,
        }
        true
    }

    fn list_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.scroll_key(key.code) {
            return vec![];
        }
        match key.code {
            KeyCode::Char('q') => vec![Cmd::Quit],
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                vec![]
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                vec![]
            }
            KeyCode::Enter | KeyCode::Right => self.open_selected(),
            KeyCode::Char('n') => {
                if self.stream.is_some() {
                    self.set_error(
                        "a reply is still coming in: press Esc in the message box to stop it first",
                    );
                    return vec![];
                }
                self.reset_conversation();
                self.focus = Focus::Compose;
                vec![]
            }
            KeyCode::Char('i') | KeyCode::Tab => {
                self.focus = Focus::Compose;
                vec![]
            }
            KeyCode::Char('d') => {
                if let Some(chat) = self.selected_chat() {
                    self.overlay = Overlay::Confirm(Confirm::DeleteChat {
                        id: chat.id.clone(),
                        title: chat.title.clone(),
                    });
                }
                vec![]
            }
            KeyCode::Char('/') => {
                self.overlay = Overlay::Search(self.search.clone());
                vec![]
            }
            KeyCode::Esc if !self.search.is_empty() => {
                self.search.clear();
                vec![Cmd::LoadChats(String::new())]
            }
            KeyCode::Char('m') => {
                self.open_models();
                vec![]
            }
            KeyCode::Char('e') => {
                self.cycle_effort();
                vec![]
            }
            KeyCode::Char('r') => self.start_cmds(),
            _ => vec![],
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.chats.is_empty() {
            return;
        }
        let last = self.chats.len() as isize - 1;
        let current = self.list.selected().unwrap_or(0) as isize;
        self.list
            .select(Some((current + delta).clamp(0, last) as usize));
    }

    fn open_selected(&mut self) -> Vec<Cmd> {
        let Some(chat) = self.selected_chat() else {
            return vec![];
        };
        if self.stream.is_some() {
            self.set_error(
                "a reply is still coming in: press Esc in the message box to stop it first",
            );
            return vec![];
        }
        let id = chat.id.clone();
        self.opening = Some(Opening {
            id: id.clone(),
            reset_model: true,
        });
        self.focus = Focus::Compose;
        vec![Cmd::LoadChat(id)]
    }

    fn compose_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.scroll_key(key.code) {
            return vec![];
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let newline = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc => {
                if self.stream.is_some() {
                    self.stop()
                } else {
                    self.focus = Focus::List;
                    vec![]
                }
            }
            KeyCode::Tab => {
                self.focus = Focus::List;
                vec![]
            }
            KeyCode::Enter if newline => {
                self.input.push('\n');
                vec![]
            }
            KeyCode::Enter => self.try_send(),
            KeyCode::Char('j') if ctrl => {
                self.input.push('\n');
                vec![]
            }
            KeyCode::Char('o') if ctrl => {
                self.open_models();
                vec![]
            }
            KeyCode::Char('e') if ctrl => {
                self.cycle_effort();
                vec![]
            }
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                vec![]
            }
            KeyCode::Char('w') if ctrl => {
                let trimmed = self.input.trim_end().len();
                let cut = self.input[..trimmed]
                    .rfind(char::is_whitespace)
                    .map_or(0, |i| i + 1);
                self.input.truncate(cut);
                vec![]
            }
            KeyCode::Backspace => {
                self.input.pop();
                vec![]
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.input.push(c);
                vec![]
            }
            _ => vec![],
        }
    }

    fn try_send(&mut self) -> Vec<Cmd> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return vec![];
        }
        if self.stream.is_some() {
            self.set_error("a reply is still coming in: Esc stops it");
            return vec![];
        }
        if self.opening.as_ref().is_some_and(|o| o.reset_model) {
            self.set_error("still loading this chat");
            return vec![];
        }
        let Some(model) = self.model.clone() else {
            self.set_error("no model is available: press m to see why");
            return vec![];
        };
        let Some(info) = self.info(&model) else {
            self.set_error("that model is no longer listed: press m to pick another");
            return vec![];
        };
        if !info.available {
            let reason = info
                .reason
                .clone()
                .unwrap_or_else(|| "it is not available".to_string());
            self.set_error(format!("{} can't be used: {reason}", info.name));
            return vec![];
        }
        if info.costs_or_leaves() && !self.paid_ok.contains(&model) {
            self.overlay = Overlay::Confirm(Confirm::PaidSend { model });
            return vec![];
        }
        let request = SendRequest {
            chat_id: self.current.as_ref().map(|c| c.id.clone()),
            text: text.clone(),
            model,
            effort: self.effective_effort(),
        };
        self.input.clear();
        self.pending_user = Some(text);
        self.stream = Some(Streaming::default());
        self.scroll_back = 0;
        vec![Cmd::Send(request)]
    }

    fn open_models(&mut self) {
        let cursor = self
            .model
            .as_deref()
            .and_then(|id| self.models.iter().position(|m| m.id == id))
            .unwrap_or(0);
        self.overlay = Overlay::Models { cursor };
    }

    fn cycle_effort(&mut self) {
        let Some(efforts) = self.current_model().and_then(|m| m.efforts.clone()) else {
            self.set_status("this model has no intensity setting");
            return;
        };
        if efforts.levels.is_empty() {
            return;
        }
        let current = self.effective_effort();
        let at = current.and_then(|c| efforts.levels.iter().position(|l| *l == c));
        let next = efforts.levels[at.map_or(0, |i| (i + 1) % efforts.levels.len())].clone();
        self.set_status(format!("intensity: {next}"));
        self.effort = Some(next);
    }

    fn models_key(&mut self, key: KeyEvent, cursor: usize) -> Vec<Cmd> {
        let last = self.models.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Char('j') | KeyCode::Down => {
                self.overlay = Overlay::Models {
                    cursor: (cursor + 1).min(last),
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay = Overlay::Models {
                    cursor: cursor.saturating_sub(1),
                }
            }
            KeyCode::Enter => {
                if let Some(m) = self.models.get(cursor) {
                    if m.available {
                        if self.model.as_deref() != Some(m.id.as_str()) {
                            self.effort = None;
                        }
                        self.model = Some(m.id.clone());
                        self.overlay = Overlay::None;
                    } else {
                        let reason = m
                            .reason
                            .clone()
                            .unwrap_or_else(|| "not available".to_string());
                        self.set_error(reason);
                    }
                }
            }
            _ => {}
        }
        vec![]
    }

    fn confirm_key(&mut self, key: KeyEvent, confirm: Confirm) -> Vec<Cmd> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.overlay = Overlay::None;
                match confirm {
                    Confirm::DeleteChat { id, .. } => vec![Cmd::Delete(id)],
                    Confirm::PaidSend { model } => {
                        self.paid_ok.insert(model);
                        self.try_send()
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.overlay = Overlay::None;
                vec![]
            }
            _ => vec![],
        }
    }

    fn search_key(&mut self, key: KeyEvent, mut input: String) -> Vec<Cmd> {
        match key.code {
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                vec![]
            }
            KeyCode::Enter => {
                self.search = input.trim().to_string();
                self.overlay = Overlay::None;
                vec![Cmd::LoadChats(self.search.clone())]
            }
            KeyCode::Backspace => {
                input.pop();
                self.overlay = Overlay::Search(input);
                vec![]
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                self.overlay = Overlay::Search(input);
                vec![]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Efforts, MessageError};

    fn model(id: &str, tier: &str, available: bool) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            name: id.rsplit('/').next().unwrap().to_string(),
            tier: tier.to_string(),
            efforts: None,
            available,
            reason: if available {
                None
            } else {
                Some("Set AIHUB_OPENROUTER_KEY".to_string())
            },
            output: None,
            remote: false,
        }
    }

    fn chat(id: &str, model: &str) -> Chat {
        Chat {
            id: id.to_string(),
            title: format!("title {id}"),
            model: model.to_string(),
            effort: None,
            created_at: 1,
            updated_at: 2,
            snippet: None,
        }
    }

    fn msg(role: &str, content: &str) -> Message {
        Message {
            id: format!("m{content}"),
            chat_id: "c1".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            model: None,
            status: "done".to_string(),
            error: None,
            created_at: 3,
            images: None,
        }
    }

    fn thinking_model() -> ModelInfo {
        ModelInfo {
            efforts: Some(Efforts {
                levels: vec!["low".into(), "medium".into(), "high".into()],
                default: Some("medium".into()),
            }),
            ..model("ollama/think", "local", true)
        }
    }

    fn app() -> App {
        let mut a = App::new("http://x".into());
        a.apply_models(Ok(Models {
            models: vec![
                model("anthropic/opus", "paid", true),
                model("openrouter/free-one", "free", false),
                ModelInfo {
                    output: Some("image".into()),
                    ..model("images/zimage", "local", true)
                },
                model("ollama/qwen3:8b", "local", true),
                thinking_model(),
            ],
            default_model: Some("ollama/qwen3:8b".into()),
        }));
        a.apply_chats(Ok(vec![
            chat("c1", "ollama/qwen3:8b"),
            chat("c2", "images/zimage"),
        ]));
        a
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(a: &mut App, code: KeyCode) -> Vec<Cmd> {
        a.on_key(key(code))
    }

    fn with(a: &mut App, code: KeyCode, mods: KeyModifiers) -> Vec<Cmd> {
        a.on_key(KeyEvent::new(code, mods))
    }

    fn type_text(a: &mut App, s: &str) {
        for c in s.chars() {
            press(a, KeyCode::Char(c));
        }
    }

    fn open_new_chat(a: &mut App) {
        press(a, KeyCode::Char('n'));
        assert_eq!(a.focus, Focus::Compose);
    }

    fn send(a: &mut App, text: &str) -> Vec<Cmd> {
        type_text(a, text);
        press(a, KeyCode::Enter)
    }

    #[test]
    fn model_list_hides_image_models_and_sorts_usable_first() {
        let a = app();
        let ids: Vec<_> = a.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "ollama/qwen3:8b",
                "ollama/think",
                "anthropic/opus",
                "openrouter/free-one"
            ]
        );
        assert_eq!(a.model.as_deref(), Some("ollama/qwen3:8b"));
        assert_eq!(a.conn, Conn::Ready);
    }

    #[test]
    fn first_models_failure_is_a_connection_failure_but_later_ones_are_not() {
        let mut a = App::new("http://x".into());
        a.apply_models(Err("cannot reach aihub-web at http://x".into()));
        assert_eq!(
            a.conn,
            Conn::Failed("cannot reach aihub-web at http://x".into())
        );
        let mut b = app();
        b.apply_models(Err("boom".into()));
        assert_eq!(b.conn, Conn::Ready);
        assert!(b.status.as_ref().unwrap().is_error);
        assert_eq!(b.models.len(), 4);
    }

    #[test]
    fn opening_a_chat_loads_it_and_adopts_its_model_and_effort() {
        let mut a = app();
        a.models.push(thinking_model());
        let cmds = press(&mut a, KeyCode::Enter);
        assert_eq!(cmds, [Cmd::LoadChat("c1".into())]);
        assert_eq!(a.focus, Focus::Compose);
        let mut c = chat("c1", "ollama/think");
        c.effort = Some("low".into());
        a.apply_chat(
            "c1",
            Ok(ChatDetail {
                chat: c,
                messages: vec![msg("user", "hi")],
            }),
        );
        assert_eq!(a.model.as_deref(), Some("ollama/think"));
        assert_eq!(a.effort.as_deref(), Some("low"));
        assert_eq!(a.messages.len(), 1);
    }

    #[test]
    fn a_reply_for_a_chat_the_user_left_is_ignored() {
        let mut a = app();
        press(&mut a, KeyCode::Enter); // opens c1
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Char('j'));
        press(&mut a, KeyCode::Enter); // opens c2, c1's reply is now stale
        a.apply_chat(
            "c1",
            Ok(ChatDetail {
                chat: chat("c1", "ollama/qwen3:8b"),
                messages: vec![msg("user", "old")],
            }),
        );
        assert!(a.current.is_none());
        assert!(a.messages.is_empty());
    }

    #[test]
    fn a_chat_on_an_image_model_falls_back_to_the_default_text_model() {
        let mut a = app();
        press(&mut a, KeyCode::Char('j'));
        press(&mut a, KeyCode::Enter);
        a.apply_chat(
            "c2",
            Ok(ChatDetail {
                chat: chat("c2", "images/zimage"),
                messages: vec![],
            }),
        );
        assert_eq!(a.model.as_deref(), Some("ollama/qwen3:8b"));
    }

    #[test]
    fn sending_starts_a_stream_and_keeps_the_text_until_confirmed() {
        let mut a = app();
        open_new_chat(&mut a);
        let cmds = send(&mut a, "hello there");
        assert_eq!(
            cmds,
            [Cmd::Send(SendRequest {
                chat_id: None,
                text: "hello there".into(),
                model: "ollama/qwen3:8b".into(),
                effort: None
            })]
        );
        assert!(a.input.is_empty());
        assert!(a.stream.is_some());
        assert_eq!(a.pending_user.as_deref(), Some("hello there"));
        // a second send while the reply streams is refused
        type_text(&mut a, "more");
        assert!(press(&mut a, KeyCode::Enter).is_empty());
        assert!(a.status.as_ref().unwrap().is_error);
        assert_eq!(a.input, "more");
    }

    #[test]
    fn an_empty_message_is_never_sent() {
        let mut a = app();
        open_new_chat(&mut a);
        assert!(send(&mut a, "   ").is_empty());
        assert!(a.stream.is_none());
    }

    #[test]
    fn an_unavailable_model_is_refused_with_its_reason_and_the_text_is_kept() {
        let mut a = app();
        open_new_chat(&mut a);
        a.model = Some("openrouter/free-one".into());
        let cmds = send(&mut a, "hi");
        assert!(cmds.is_empty());
        assert!(
            a.status
                .as_ref()
                .unwrap()
                .text
                .contains("Set AIHUB_OPENROUTER_KEY")
        );
        assert_eq!(a.input, "hi");
        assert!(a.stream.is_none());
    }

    #[test]
    fn a_paid_model_asks_once_then_remembers() {
        let mut a = app();
        open_new_chat(&mut a);
        a.model = Some("anthropic/opus".into());
        assert!(send(&mut a, "hi").is_empty());
        assert_eq!(
            a.overlay,
            Overlay::Confirm(Confirm::PaidSend {
                model: "anthropic/opus".into()
            })
        );
        assert_eq!(a.input, "hi");

        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.overlay, Overlay::None);
        assert!(a.stream.is_none());

        press(&mut a, KeyCode::Enter);
        let cmds = press(&mut a, KeyCode::Char('y'));
        assert!(
            matches!(cmds.as_slice(), [Cmd::Send(r)] if r.model == "anthropic/opus" && r.text == "hi")
        );

        // after the reply, the same paid model no longer asks
        a.apply_event(Event::Chat {
            chat: chat("c9", "anthropic/opus"),
            user: None,
        });
        a.apply_event(Event::Done(msg("assistant", "ok")));
        let cmds = send(&mut a, "again");
        assert!(matches!(cmds.as_slice(), [Cmd::Send(_)]));
    }

    #[test]
    fn approving_one_paid_model_does_not_approve_another() {
        let mut a = app();
        a.models.push(ModelInfo {
            remote: true,
            ..model("ollama/big:cloud", "local", true)
        });
        open_new_chat(&mut a);
        a.model = Some("anthropic/opus".into());
        send(&mut a, "hi");
        let cmds = press(&mut a, KeyCode::Char('y'));
        assert!(matches!(cmds.as_slice(), [Cmd::Send(_)]));
        a.apply_event(Event::Chat {
            chat: chat("c9", "m"),
            user: None,
        });
        a.apply_event(Event::Done(msg("assistant", "ok")));

        a.model = Some("ollama/big:cloud".into());
        assert!(send(&mut a, "again").is_empty());
        assert_eq!(
            a.overlay,
            Overlay::Confirm(Confirm::PaidSend {
                model: "ollama/big:cloud".into()
            })
        );
    }

    #[test]
    fn a_remote_ollama_model_counts_as_leaving_the_machine() {
        let mut a = app();
        a.models.push(ModelInfo {
            remote: true,
            ..model("ollama/big:cloud", "local", true)
        });
        a.model = Some("ollama/big:cloud".into());
        open_new_chat(&mut a);
        a.model = Some("ollama/big:cloud".into());
        assert!(send(&mut a, "hi").is_empty());
        assert!(matches!(
            a.overlay,
            Overlay::Confirm(Confirm::PaidSend { .. })
        ));
    }

    #[test]
    fn stream_events_build_the_conversation() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "hi");
        let cmds = a.apply_event(Event::Chat {
            chat: chat("c9", "ollama/qwen3:8b"),
            user: Some(msg("user", "hi")),
        });
        assert_eq!(cmds, [Cmd::LoadChats(String::new())]);
        assert_eq!(a.current.as_ref().unwrap().id, "c9");
        assert!(a.pending_user.is_none());
        a.apply_event(Event::Think("hmm".into()));
        a.apply_event(Event::Text("Hel".into()));
        a.apply_event(Event::Text("lo".into()));
        assert_eq!(a.stream.as_ref().unwrap().text, "Hello");
        assert_eq!(a.stream.as_ref().unwrap().think_chars, 3);
        let cmds = a.apply_event(Event::Done(msg("assistant", "Hello")));
        assert_eq!(cmds, [Cmd::LoadChats(String::new())]);
        assert!(a.stream.is_none());
        assert_eq!(a.messages.len(), 2);
        // the sidebar selection follows the new chat once the list reloads
        a.apply_chats(Ok(vec![chat("c1", "m"), chat("c9", "m")]));
        assert_eq!(a.selected_chat().unwrap().id, "c9");
    }

    #[test]
    fn events_after_a_stop_are_ignored() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "hi");
        press(&mut a, KeyCode::Esc);
        assert!(a.apply_event(Event::Text("late".into())).is_empty());
        assert!(
            a.apply_event(Event::Done(msg("assistant", "late")))
                .is_empty()
        );
        assert!(a.messages.is_empty());
    }

    #[test]
    fn stopping_before_the_server_answers_gives_the_text_back() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "draft");
        let cmds = press(&mut a, KeyCode::Esc);
        assert_eq!(cmds, [Cmd::Stop(None)]);
        assert_eq!(a.input, "draft");
        assert!(a.stream.is_none());
    }

    #[test]
    fn stopping_an_existing_chat_reloads_it_to_show_the_stored_partial_reply() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "hi");
        a.apply_event(Event::Chat {
            chat: chat("c9", "ollama/qwen3:8b"),
            user: Some(msg("user", "hi")),
        });
        a.apply_event(Event::Text("partial".into()));
        let cmds = press(&mut a, KeyCode::Esc);
        assert_eq!(cmds, [Cmd::Stop(Some("c9".into()))]);
        assert!(a.input.is_empty());
        a.apply_chat(
            "c9",
            Ok(ChatDetail {
                chat: chat("c9", "ollama/qwen3:8b"),
                messages: vec![msg("user", "hi"), msg("assistant", "partial")],
            }),
        );
        assert_eq!(a.messages.len(), 2);
    }

    #[test]
    fn a_failed_send_restores_the_text_and_reports_the_error() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "hi");
        let cmds = a.stream_ended(Err("Set AIHUB_OPENROUTER_KEY (HTTP 400)".into()));
        assert!(cmds.is_empty());
        assert_eq!(a.input, "hi");
        assert!(a.status.as_ref().unwrap().is_error);
        assert!(a.stream.is_none());
    }

    #[test]
    fn a_dropped_stream_in_a_known_chat_reloads_it() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "hi");
        a.apply_event(Event::Chat {
            chat: chat("c9", "m"),
            user: Some(msg("user", "hi")),
        });
        let cmds = a.stream_ended(Err("the reply stream ended early".into()));
        assert_eq!(cmds, [Cmd::LoadChat("c9".into())]);
        assert!(a.input.is_empty());
    }

    #[test]
    fn an_error_event_keeps_the_partial_message_and_shows_the_error() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "hi");
        a.apply_event(Event::Chat {
            chat: chat("c9", "m"),
            user: Some(msg("user", "hi")),
        });
        let mut failed = msg("assistant", "half");
        failed.status = "error".into();
        failed.error = Some(MessageError {
            code: "provider".into(),
            message: "model crashed".into(),
        });
        a.apply_event(Event::Error(failed));
        assert_eq!(a.status.as_ref().unwrap().text, "model crashed");
        assert_eq!(a.messages.last().unwrap().status, "error");
        assert!(a.stream.is_none());
    }

    #[test]
    fn ctrl_c_quits_when_idle_and_stops_a_reply_otherwise() {
        let mut a = app();
        assert_eq!(
            with(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL),
            [Cmd::Quit]
        );
        open_new_chat(&mut a);
        send(&mut a, "hi");
        assert_eq!(
            with(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL),
            [Cmd::Stop(None)]
        );
    }

    #[test]
    fn q_only_quits_from_the_list_and_types_in_the_message_box() {
        let mut a = app();
        open_new_chat(&mut a);
        assert!(press(&mut a, KeyCode::Char('q')).is_empty());
        assert_eq!(a.input, "q");
        press(&mut a, KeyCode::Esc);
        assert_eq!(press(&mut a, KeyCode::Char('q')), [Cmd::Quit]);
    }

    #[test]
    fn newlines_come_from_alt_enter_ctrl_j_and_paste_never_send() {
        let mut a = app();
        open_new_chat(&mut a);
        type_text(&mut a, "a");
        assert!(with(&mut a, KeyCode::Enter, KeyModifiers::ALT).is_empty());
        type_text(&mut a, "b");
        with(&mut a, KeyCode::Char('j'), KeyModifiers::CONTROL);
        a.on_paste("c\r\nd");
        assert_eq!(a.input, "a\nb\nc\n\nd");
        assert!(a.stream.is_none());
    }

    #[test]
    fn ctrl_w_deletes_a_word_and_ctrl_u_clears() {
        let mut a = app();
        open_new_chat(&mut a);
        type_text(&mut a, "one two three");
        with(&mut a, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(a.input, "one two ");
        with(&mut a, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(a.input, "one ");
        with(&mut a, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(a.input, "");
    }

    #[test]
    fn effort_cycles_from_the_default_and_is_sent_only_when_the_model_has_one() {
        let mut a = app();
        open_new_chat(&mut a);
        a.model = Some("ollama/think".into());
        assert_eq!(a.effective_effort().as_deref(), Some("medium"));
        with(&mut a, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(a.effective_effort().as_deref(), Some("high"));
        with(&mut a, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(a.effective_effort().as_deref(), Some("low"));
        let cmds = send(&mut a, "x");
        assert!(matches!(cmds.as_slice(), [Cmd::Send(r)] if r.effort.as_deref() == Some("low")));

        let mut b = app();
        open_new_chat(&mut b);
        with(&mut b, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(b.status.as_ref().unwrap().text.contains("no intensity"));
        let cmds = send(&mut b, "x");
        assert!(matches!(cmds.as_slice(), [Cmd::Send(r)] if r.effort.is_none()));
    }

    #[test]
    fn picking_a_model_resets_effort_and_refuses_unavailable_ones() {
        let mut a = app();
        open_new_chat(&mut a);
        a.model = Some("ollama/think".into());
        a.effort = Some("high".into());
        with(&mut a, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(matches!(a.overlay, Overlay::Models { cursor: 1 }));
        press(&mut a, KeyCode::Up);
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.overlay, Overlay::None);
        assert_eq!(a.model.as_deref(), Some("ollama/qwen3:8b"));
        assert_eq!(a.effort, None);

        with(&mut a, KeyCode::Char('o'), KeyModifiers::CONTROL);
        for _ in 0..10 {
            press(&mut a, KeyCode::Down);
        }
        press(&mut a, KeyCode::Enter);
        assert!(
            matches!(a.overlay, Overlay::Models { .. }),
            "an unavailable model must not be picked"
        );
        assert_eq!(a.model.as_deref(), Some("ollama/qwen3:8b"));
        assert!(a.status.as_ref().unwrap().is_error);
    }

    #[test]
    fn deleting_asks_first_and_clears_the_open_chat() {
        let mut a = app();
        assert!(press(&mut a, KeyCode::Char('d')).is_empty());
        assert!(
            matches!(a.overlay, Overlay::Confirm(Confirm::DeleteChat { ref id, .. }) if id == "c1")
        );
        press(&mut a, KeyCode::Char('n'));
        assert_eq!(a.overlay, Overlay::None);

        press(&mut a, KeyCode::Char('d'));
        assert_eq!(
            press(&mut a, KeyCode::Char('y')),
            [Cmd::Delete("c1".into())]
        );

        a.current = Some(chat("c1", "m"));
        a.messages = vec![msg("user", "x")];
        let cmds = a.deleted("c1", Ok(()));
        assert_eq!(cmds, [Cmd::LoadChats(String::new())]);
        assert!(a.current.is_none() && a.messages.is_empty());

        let cmds = a.deleted("c1", Err("HTTP 500".into()));
        assert!(cmds.is_empty());
        assert!(a.status.as_ref().unwrap().is_error);
    }

    #[test]
    fn search_filters_the_list_and_escape_clears_it() {
        let mut a = app();
        press(&mut a, KeyCode::Char('/'));
        type_text(&mut a, "eagle");
        assert_eq!(
            press(&mut a, KeyCode::Enter),
            [Cmd::LoadChats("eagle".into())]
        );
        assert_eq!(a.search, "eagle");
        assert_eq!(press(&mut a, KeyCode::Esc), [Cmd::LoadChats(String::new())]);
        assert!(a.search.is_empty());
    }

    #[test]
    fn list_selection_survives_a_reload_and_clamps_at_the_ends() {
        let mut a = app();
        press(&mut a, KeyCode::Char('k'));
        assert_eq!(a.list.selected(), Some(0));
        press(&mut a, KeyCode::Char('j'));
        press(&mut a, KeyCode::Char('j'));
        assert_eq!(a.list.selected(), Some(1));
        a.apply_chats(Ok(vec![chat("c0", "m"), chat("c1", "m"), chat("c2", "m")]));
        assert_eq!(a.selected_chat().unwrap().id, "c2");
        a.apply_chats(Ok(vec![]));
        assert_eq!(a.list.selected(), None);
    }

    #[test]
    fn starting_a_new_chat_or_opening_another_is_refused_while_a_reply_streams() {
        let mut a = app();
        open_new_chat(&mut a);
        send(&mut a, "hi");
        press(&mut a, KeyCode::Tab);
        assert!(press(&mut a, KeyCode::Char('n')).is_empty());
        assert!(a.status.as_ref().unwrap().is_error);
        assert!(press(&mut a, KeyCode::Enter).is_empty());
        assert!(a.stream.is_some());
    }

    #[test]
    fn page_keys_scroll_the_transcript_from_both_panes() {
        let mut a = app();
        press(&mut a, KeyCode::PageUp);
        assert_eq!(a.scroll_back, 10);
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::PageDown);
        assert_eq!(a.scroll_back, 0);
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut a = app();
        let mut k = key(KeyCode::Char('q'));
        k.kind = KeyEventKind::Release;
        assert!(a.on_key(k).is_empty());
    }
}
