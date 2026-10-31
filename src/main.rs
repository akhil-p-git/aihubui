use std::env;
use std::io::stdout;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aihubui::api::{self, Chat, ChatDetail, Client, Event, Models, SendRequest};
use aihubui::app::{App, Cmd};
use aihubui::ui;
use anyhow::{Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent,
};
use ratatui::crossterm::execute;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

const USAGE: &str = "aihubui [URL]

A terminal client for the aihub web chat. URL defaults to http://127.0.0.1:8787, where the
`aihub-web` systemd user service listens. It talks to the same HTTP API as the web page, so chats
are shared with the browser and aihub's keys, model rules and 30-day retention apply.

list:     j/k move  Enter open  n new chat  d delete  / search  m model  e intensity  Tab write  q quit
message:  Enter send  Alt+Enter or Ctrl+J newline  Esc back (or stop a reply)  Ctrl+O model
          Ctrl+E intensity  PgUp/PgDn scroll  Ctrl+C stop a reply, or quit when idle";

/// Everything the event loop reacts to. Replies to a message carry the generation of the send they
/// belong to, so a late event from a stopped reply can never reach the next one.
enum Msg {
    Key(ratatui::crossterm::event::KeyEvent),
    Paste(String),
    Tick,
    Models(Result<Models, String>),
    Chats(Result<Vec<Chat>, String>),
    Chat(String, Result<ChatDetail, String>),
    Event(u64, Event),
    StreamEnded(u64, Result<(), String>),
    Deleted(String, Result<(), String>),
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn text<T>(r: anyhow::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("{e:#}"))
}

/// Reads terminal input on its own thread and ticks every 100 ms so spinners move.
fn spawn_input(tx: UnboundedSender<Msg>) {
    thread::spawn(move || {
        loop {
            let msg = match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(TermEvent::Key(k)) => Msg::Key(k),
                    Ok(TermEvent::Paste(s)) => Msg::Paste(s),
                    Ok(_) => Msg::Tick,
                    Err(_) => return,
                },
                Ok(false) => Msg::Tick,
                Err(_) => return,
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}

struct Runner {
    client: Client,
    tx: UnboundedSender<Msg>,
    stream: Option<JoinHandle<()>>,
    generation: u64,
}

impl Runner {
    /// Runs the commands; returns true when the app should quit.
    fn run(&mut self, cmds: Vec<Cmd>) -> bool {
        for cmd in cmds {
            match cmd {
                Cmd::Quit => {
                    self.abort_stream();
                    return true;
                }
                Cmd::LoadModels => {
                    let (c, tx) = (self.client.clone(), self.tx.clone());
                    tokio::spawn(async move {
                        let _ = tx.send(Msg::Models(text(c.models().await)));
                    });
                }
                Cmd::LoadChats(q) => {
                    let (c, tx) = (self.client.clone(), self.tx.clone());
                    tokio::spawn(async move {
                        let _ = tx.send(Msg::Chats(text(c.chats(&q).await)));
                    });
                }
                Cmd::LoadChat(id) => self.load_chat(id, Duration::ZERO),
                Cmd::Delete(id) => {
                    let (c, tx) = (self.client.clone(), self.tx.clone());
                    tokio::spawn(async move {
                        let r = text(c.delete_chat(&id).await);
                        let _ = tx.send(Msg::Deleted(id, r));
                    });
                }
                Cmd::Send(req) => self.send(req),
                Cmd::Stop(id) => {
                    self.abort_stream();
                    // Give the server a moment to store the partial reply before asking for it.
                    if let Some(id) = id {
                        self.load_chat(id, Duration::from_millis(600));
                    }
                }
            }
        }
        false
    }

    fn load_chat(&self, id: String, delay: Duration) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let r = text(c.chat(&id).await);
            let _ = tx.send(Msg::Chat(id, r));
        });
    }

    fn abort_stream(&mut self) {
        if let Some(handle) = self.stream.take() {
            handle.abort();
        }
    }

    fn send(&mut self, req: SendRequest) {
        self.abort_stream();
        self.generation += 1;
        let (generation, c, tx) = (self.generation, self.client.clone(), self.tx.clone());
        self.stream = Some(tokio::spawn(async move {
            let events = tx.clone();
            let result = c
                .send(&req, move |e| {
                    let _ = events.send(Msg::Event(generation, e));
                })
                .await;
            let _ = tx.send(Msg::StreamEnded(generation, text(result)));
        }));
    }
}

async fn event_loop(terminal: &mut DefaultTerminal, base: String) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    spawn_input(tx.clone());
    let mut runner = Runner {
        client: Client::new(&base)?,
        tx,
        stream: None,
        generation: 0,
    };
    let mut app = App::new(base);
    let start = app.start_cmds();
    runner.run(start);

    loop {
        app.now_ms = now_ms();
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        let Some(msg) = rx.recv().await else {
            return Ok(());
        };
        let cmds = match msg {
            Msg::Key(k) => app.on_key(k),
            Msg::Paste(s) => {
                app.on_paste(&s);
                vec![]
            }
            Msg::Tick => {
                app.tick = app.tick.wrapping_add(1);
                vec![]
            }
            Msg::Models(r) => {
                app.apply_models(r);
                vec![]
            }
            Msg::Chats(r) => {
                app.apply_chats(r);
                vec![]
            }
            Msg::Chat(id, r) => {
                app.apply_chat(&id, r);
                vec![]
            }
            Msg::Event(g, e) if g == runner.generation => app.apply_event(e),
            Msg::StreamEnded(g, r) if g == runner.generation => app.stream_ended(r),
            Msg::Event(..) | Msg::StreamEnded(..) => vec![],
            Msg::Deleted(id, r) => app.deleted(&id, r),
        };
        if runner.run(cmds) {
            return Ok(());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if args.len() > 1 {
        bail!("expected at most one URL\n\n{USAGE}");
    }
    let base = args
        .first()
        .cloned()
        .unwrap_or_else(|| api::DEFAULT_URL.to_string());
    // reqwest is built without TLS: aihub-web listens on plain loopback HTTP.
    if !base.starts_with("http://") {
        bail!("the URL must start with http:// (got {base})");
    }

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, base).await;
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}
