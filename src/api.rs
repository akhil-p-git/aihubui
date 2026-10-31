//! The aihub-web HTTP API as this client uses it. Field names follow `app/aihub_web/db.py`,
//! `catalog.py` and the events in `chat.py`; unknown fields are ignored so the server can grow.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

pub const DEFAULT_URL: &str = "http://127.0.0.1:8787";

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    /// Milliseconds since the epoch.
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    pub id: String,
    pub chat_id: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub model: Option<String>,
    /// "done", "error" or "stopped".
    pub status: String,
    #[serde(default)]
    pub error: Option<MessageError>,
    pub created_at: i64,
    /// Metadata of generated images; this client never shows the pictures themselves.
    #[serde(default)]
    pub images: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Efforts {
    pub levels: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// "local", "free" or "paid".
    pub tier: String,
    #[serde(default)]
    pub efforts: Option<Efforts>,
    pub available: bool,
    #[serde(default)]
    pub reason: Option<String>,
    /// "image" for image models; absent for text models.
    #[serde(default)]
    pub output: Option<String>,
    /// An Ollama cloud model: listed with the local ones, but the prompt leaves this machine.
    #[serde(default)]
    pub remote: bool,
}

impl ModelInfo {
    pub fn is_text(&self) -> bool {
        self.output.as_deref() != Some("image")
    }

    /// True when using the model may cost money or send the prompt off this machine.
    pub fn costs_or_leaves(&self) -> bool {
        self.tier == "paid" || self.remote
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Models {
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub default_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatList {
    chats: Vec<Chat>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatDetail {
    pub chat: Chat,
    pub messages: Vec<Message>,
}

/// The only fields the server accepts on `POST /api/chat` that this client sets; it rejects extras.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SendRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub text: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The chat the reply belongs to (new or existing) and the stored user message.
    Chat {
        chat: Chat,
        user: Option<Message>,
    },
    /// Earlier messages were left out to fit the model's context.
    Note {
        dropped: u64,
    },
    Think(String),
    Text(String),
    /// The reply finished and was stored.
    Done(Message),
    /// The reply failed; the message holds the error and any partial text.
    Error(Message),
    /// Image progress; ignored by this client.
    Progress,
    Unknown(String),
}

#[derive(Deserialize)]
struct RawEvent {
    t: String,
    #[serde(default)]
    d: Option<String>,
    #[serde(default)]
    chat: Option<Chat>,
    #[serde(default)]
    user: Option<Message>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    dropped: Option<serde_json::Value>,
}

pub fn parse_event(line: &str) -> Result<Event> {
    let raw: RawEvent = serde_json::from_str(line).context("unreadable stream event")?;
    Ok(match raw.t.as_str() {
        "chat" => Event::Chat {
            chat: raw
                .chat
                .ok_or_else(|| anyhow!("chat event without a chat"))?,
            user: raw.user,
        },
        "note" => Event::Note {
            dropped: raw.dropped.and_then(|v| v.as_u64()).unwrap_or(0),
        },
        "think" => Event::Think(raw.d.unwrap_or_default()),
        "text" => Event::Text(raw.d.unwrap_or_default()),
        "done" => Event::Done(
            raw.message
                .ok_or_else(|| anyhow!("done event without a message"))?,
        ),
        "error" => Event::Error(
            raw.message
                .ok_or_else(|| anyhow!("error event without a message"))?,
        ),
        "progress" => Event::Progress,
        other => Event::Unknown(other.to_string()),
    })
}

/// Splits a byte stream into newline-terminated events. A line is decoded only once it is complete, so
/// a multi-byte character split across two network chunks is never cut.
#[derive(Default)]
pub struct NdjsonDecoder {
    buf: Vec<u8>,
}

impl NdjsonDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<Event>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            let text = text.trim();
            if !text.is_empty() {
                out.push(parse_event(text));
            }
        }
        out
    }

    /// Anything left after the last newline: the stream ended in the middle of an event.
    pub fn has_partial_line(&self) -> bool {
        self.buf.iter().any(|b| !b.is_ascii_whitespace())
    }
}

/// Turns an error response into a sentence, using the server's own message when it sent one.
pub fn error_text(status: StatusCode, body: &str) -> String {
    #[derive(Deserialize)]
    struct Body {
        error: MessageError,
    }
    match serde_json::from_str::<Body>(body) {
        Ok(b) => format!("{} (HTTP {})", b.error.message, status.as_u16()),
        Err(_) => {
            let snippet: String = body.trim().chars().take(200).collect();
            if snippet.is_empty() {
                format!("HTTP {}", status.as_u16())
            } else {
                format!("HTTP {}: {snippet}", status.as_u16())
            }
        }
    }
}

#[derive(Clone)]
pub struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .context("cannot build the HTTP client")?;
        Ok(Client {
            base: base.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn unreachable(&self, err: reqwest::Error) -> anyhow::Error {
        if err.is_connect() || err.is_timeout() {
            anyhow!("cannot reach aihub-web at {}", self.base)
        } else {
            anyhow!(err)
        }
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| self.unreachable(e))?;
        if !status.is_success() {
            bail!(error_text(status, &body));
        }
        serde_json::from_str(&body).with_context(|| format!("unexpected reply from {path}"))
    }

    pub async fn models(&self) -> Result<Models> {
        self.get_json("/api/models", &[]).await
    }

    pub async fn chats(&self, query: &str) -> Result<Vec<Chat>> {
        let q: &[(&str, &str)] = if query.is_empty() {
            &[]
        } else {
            &[("q", query)]
        };
        Ok(self.get_json::<ChatList>("/api/chats", q).await?.chats)
    }

    pub async fn chat(&self, id: &str) -> Result<ChatDetail> {
        self.get_json(&format!("/api/chats/{id}"), &[]).await
    }

    pub async fn delete_chat(&self, id: &str) -> Result<()> {
        let resp = self
            .http
            .delete(format!("{}/api/chats/{id}", self.base))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        bail!(error_text(status, &body))
    }

    /// Sends a message and calls `on_event` for each streamed event until the server ends the stream.
    /// Fails if the connection drops before a `done` or `error` event arrives.
    pub async fn send(&self, req: &SendRequest, mut on_event: impl FnMut(Event)) -> Result<()> {
        let mut resp = self
            .http
            .post(format!("{}/api/chat", self.base))
            .json(req)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(error_text(status, &body));
        }
        let mut decoder = NdjsonDecoder::default();
        let mut finished = false;
        while let Some(chunk) = resp
            .chunk()
            .await
            .context("the connection to aihub-web dropped")?
        {
            for event in decoder.push(&chunk) {
                let event = event?;
                finished |= matches!(event, Event::Done(_) | Event::Error(_));
                on_event(event);
            }
        }
        if !finished || decoder.has_partial_line() {
            bail!("the reply stream ended early");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = r#"{"id":"5fc3a34107d16d50","title":"hi","model":"ollama/qwen3:8b","effort":null,"created_at":1792667042132,"updated_at":1792667225873,"project_id":null,"task_id":null,"snippet":null}"#;
    const MSG: &str = r#"{"id":"aaaaaaaaaaaaaaaa","chat_id":"5fc3a34107d16d50","seq":2,"role":"assistant","content":"hello","thinking":null,"model":"ollama/qwen3:8b","effort":null,"usage":{"total_ms":1200},"status":"done","error":null,"created_at":1792667042999,"images":null}"#;

    #[test]
    fn parses_every_event_kind() {
        let chat = parse_event(&format!(r#"{{"t":"chat","chat":{CHAT},"user":null}}"#)).unwrap();
        assert!(
            matches!(chat, Event::Chat { ref chat, user: None } if chat.id == "5fc3a34107d16d50")
        );
        assert_eq!(
            parse_event(r#"{"t":"note","dropped":3}"#).unwrap(),
            Event::Note { dropped: 3 }
        );
        assert_eq!(
            parse_event(r#"{"t":"think","d":"hm"}"#).unwrap(),
            Event::Think("hm".into())
        );
        assert_eq!(
            parse_event(r#"{"t":"text","d":"hi"}"#).unwrap(),
            Event::Text("hi".into())
        );
        let done = parse_event(&format!(r#"{{"t":"done","message":{MSG}}}"#)).unwrap();
        assert!(matches!(done, Event::Done(ref m) if m.content == "hello" && m.status == "done"));
        assert_eq!(
            parse_event(r#"{"t":"progress","step":1}"#).unwrap(),
            Event::Progress
        );
        assert_eq!(
            parse_event(r#"{"t":"future"}"#).unwrap(),
            Event::Unknown("future".into())
        );
    }

    #[test]
    fn rejects_malformed_events() {
        assert!(parse_event("not json").is_err());
        assert!(parse_event(r#"{"t":"done"}"#).is_err());
        assert!(parse_event(r#"{"t":"chat"}"#).is_err());
    }

    #[test]
    fn decoder_joins_events_split_across_chunks() {
        let mut d = NdjsonDecoder::default();
        assert!(d.push(br#"{"t":"text","d":"he"#).is_empty());
        assert!(d.has_partial_line());
        let events = d.push(b"llo\"}\n{\"t\":\"text\",\"d\":\"!\"}\n");
        let texts: Vec<_> = events.into_iter().map(|e| e.unwrap()).collect();
        assert_eq!(
            texts,
            [Event::Text("hello".into()), Event::Text("!".into())]
        );
        assert!(!d.has_partial_line());
    }

    #[test]
    fn decoder_never_cuts_a_multibyte_character() {
        let line = "{\"t\":\"text\",\"d\":\"日本\"}\n".as_bytes();
        for split in 1..line.len() {
            let mut d = NdjsonDecoder::default();
            let mut events = d.push(&line[..split]);
            events.extend(d.push(&line[split..]));
            let events: Vec<_> = events.into_iter().map(|e| e.unwrap()).collect();
            assert_eq!(events, [Event::Text("日本".into())], "split at {split}");
        }
    }

    #[test]
    fn decoder_skips_blank_lines_and_reports_bad_ones() {
        let mut d = NdjsonDecoder::default();
        let events = d.push(b"\n  \n{\"t\":\"progress\"}\nbroken\n");
        assert_eq!(events.len(), 2);
        assert!(events[0].is_ok());
        assert!(events[1].is_err());
    }

    #[test]
    fn deserializes_real_server_shapes() {
        let chat: Chat = serde_json::from_str(CHAT).unwrap();
        assert_eq!(chat.created_at, 1792667042132);
        let detail: ChatDetail = serde_json::from_str(&format!(
            r#"{{"chat":{CHAT},"messages":[{MSG}],"pending_image":null}}"#
        ))
        .unwrap();
        assert_eq!(detail.messages[0].role, "assistant");

        let models: Models = serde_json::from_str(
            r#"{"models":[
              {"id":"ollama/qwen3:8b","provider":"ollama","name":"qwen3:8b","tier":"local","ctx":40960,"vision":false,
               "efforts":{"levels":["low","high"],"default":"high"},"price":null,"expires":null,"size_bytes":5,"available":true,"reason":null},
              {"id":"images/zimage","provider":"images","name":"Z-Image","tier":"local","available":true,"reason":null,
               "output":"image","edits":false,"est_s":{"square":23},"note":"x"},
              {"id":"ollama/big:cloud","provider":"ollama","name":"big","tier":"paid","available":true,"reason":null,"remote":true}
            ],"default_model":"ollama/qwen3:8b","providers":{"ollama":{"ok":true,"error":null}}}"#,
        )
        .unwrap();
        assert_eq!(models.models.len(), 3);
        assert!(models.models[0].is_text() && !models.models[0].costs_or_leaves());
        assert_eq!(
            models.models[0]
                .efforts
                .as_ref()
                .unwrap()
                .default
                .as_deref(),
            Some("high")
        );
        assert!(!models.models[1].is_text());
        assert!(models.models[2].costs_or_leaves());
    }

    #[test]
    fn send_request_omits_unset_fields() {
        let req = SendRequest {
            chat_id: None,
            text: "hi".into(),
            model: "m".into(),
            effort: None,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"text":"hi","model":"m"}"#
        );
        let req = SendRequest {
            chat_id: Some("c".into()),
            text: "hi".into(),
            model: "m".into(),
            effort: Some("low".into()),
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"chat_id":"c","text":"hi","model":"m","effort":"low"}"#
        );
    }

    #[test]
    fn error_text_prefers_the_servers_message() {
        let body =
            r#"{"error":{"code":"model_unavailable","message":"Set AIHUB_OPENROUTER_KEY first"}}"#;
        assert_eq!(
            error_text(StatusCode::BAD_REQUEST, body),
            "Set AIHUB_OPENROUTER_KEY first (HTTP 400)"
        );
        assert_eq!(error_text(StatusCode::BAD_GATEWAY, ""), "HTTP 502");
        assert_eq!(
            error_text(StatusCode::FORBIDDEN, "Host not allowed."),
            "HTTP 403: Host not allowed."
        );
    }
}
