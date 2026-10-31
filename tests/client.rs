//! Runs the real HTTP client against a one-shot fake aihub-web on a loopback port.

use std::time::Duration;

use aihubui::api::{Client, Event, SendRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// A response written in pieces, with a pause between pieces so each travels in its own TCP segment.
struct Reply {
    head: &'static str,
    chunks: Vec<Vec<u8>>,
    chunked: bool,
}

fn chunked(chunks: Vec<Vec<u8>>) -> Reply {
    Reply {
        head: "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\ntransfer-encoding: chunked\r\n\r\n",
        chunks,
        chunked: true,
    }
}

fn plain(status: &'static str, body: &str) -> Reply {
    Reply {
        head: status,
        chunks: vec![body.as_bytes().to_vec()],
        chunked: false,
    }
}

/// Accepts one connection, returns the raw request text it received.
async fn serve_once(reply: Reply) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length || n == 0 {
                    break;
                }
            }
        }
        if reply.chunked {
            socket.write_all(reply.head.as_bytes()).await.unwrap();
            for chunk in &reply.chunks {
                socket
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .unwrap();
                socket.write_all(chunk).await.unwrap();
                socket.write_all(b"\r\n").await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        } else {
            let body = &reply.chunks[0];
            let head = format!(
                "{}\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
                reply.head,
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        }
        let _ = socket.shutdown().await;
        String::from_utf8_lossy(&request).to_string()
    });
    (base, handle)
}

const CHAT: &str = r#"{"id":"0123456789abcdef","title":"hi","model":"ollama/qwen3:8b","effort":null,"created_at":1,"updated_at":2}"#;

fn msg(role: &str, content: &str) -> String {
    format!(
        r#"{{"id":"aaaaaaaaaaaaaaaa","chat_id":"0123456789abcdef","role":"{role}","content":"{content}","model":"ollama/qwen3:8b","status":"done","error":null,"created_at":3}}"#
    )
}

fn request() -> SendRequest {
    SendRequest {
        chat_id: None,
        text: "hello".into(),
        model: "ollama/qwen3:8b".into(),
        effort: None,
    }
}

#[tokio::test]
async fn send_streams_events_even_when_they_arrive_split_across_chunks() {
    let lines = format!(
        "{{\"t\":\"chat\",\"chat\":{CHAT},\"user\":{}}}\n{{\"t\":\"text\",\"d\":\"日本\"}}\n{{\"t\":\"done\",\"message\":{}}}\n",
        msg("user", "hello"),
        msg("assistant", "日本")
    );
    let bytes = lines.as_bytes();
    // cut in the middle of the first event and in the middle of the 3-byte character
    let cut_a = 20;
    let cut_b = lines.find("日").unwrap() + 1;
    let chunks = vec![
        bytes[..cut_a].to_vec(),
        bytes[cut_a..cut_b].to_vec(),
        bytes[cut_b..].to_vec(),
    ];
    let (base, server) = serve_once(chunked(chunks)).await;

    let mut events = Vec::new();
    Client::new(&base)
        .unwrap()
        .send(&request(), |e| events.push(e))
        .await
        .unwrap();

    assert!(
        matches!(&events[0], Event::Chat { chat, user: Some(u) } if chat.id == "0123456789abcdef" && u.content == "hello")
    );
    assert_eq!(events[1], Event::Text("日本".into()));
    assert!(matches!(&events[2], Event::Done(m) if m.content == "日本"));
    assert_eq!(events.len(), 3);

    let raw = server.await.unwrap();
    let lower = raw.to_ascii_lowercase();
    assert!(raw.starts_with("POST /api/chat HTTP/1.1"), "{raw}");
    assert!(lower.contains("content-type: application/json"), "{raw}");
    assert!(
        !lower.contains("\r\norigin:"),
        "a TUI must not send an Origin header: {raw}"
    );
    assert!(
        raw.ends_with(r#"{"text":"hello","model":"ollama/qwen3:8b"}"#),
        "{raw}"
    );
}

#[tokio::test]
async fn send_reports_the_servers_error_message_for_a_rejected_request() {
    let body =
        r#"{"error":{"code":"model_unavailable","message":"Set AIHUB_OPENROUTER_KEY first"}}"#;
    let (base, _server) = serve_once(plain("HTTP/1.1 400 Bad Request", body)).await;
    let err = Client::new(&base)
        .unwrap()
        .send(&request(), |_| {})
        .await
        .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "Set AIHUB_OPENROUTER_KEY first (HTTP 400)"
    );
}

#[tokio::test]
async fn send_fails_when_the_stream_ends_without_a_final_event() {
    let line = format!(
        "{{\"t\":\"chat\",\"chat\":{CHAT},\"user\":null}}\n{{\"t\":\"text\",\"d\":\"par\"}}\n"
    );
    let (base, _server) = serve_once(chunked(vec![line.into_bytes()])).await;
    let mut events = Vec::new();
    let err = Client::new(&base)
        .unwrap()
        .send(&request(), |e| events.push(e))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("ended early"), "{err:#}");
    assert_eq!(
        events.len(),
        2,
        "events received before the drop are still delivered"
    );
}

#[tokio::test]
async fn send_surfaces_an_unreadable_event_instead_of_skipping_it() {
    let (base, _server) = serve_once(chunked(vec![b"this is not json\n".to_vec()])).await;
    let err = Client::new(&base)
        .unwrap()
        .send(&request(), |_| {})
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("unreadable stream event"),
        "{err:#}"
    );
}

#[tokio::test]
async fn reads_models_chats_and_a_chat_with_its_messages() {
    let models = r#"{"models":[{"id":"ollama/qwen3:8b","provider":"ollama","name":"qwen3:8b","tier":"local","available":true,"reason":null}],"default_model":"ollama/qwen3:8b","providers":{}}"#;
    let (base, server) = serve_once(plain("HTTP/1.1 200 OK", models)).await;
    let m = Client::new(&base).unwrap().models().await.unwrap();
    assert_eq!(m.default_model.as_deref(), Some("ollama/qwen3:8b"));
    assert!(
        server
            .await
            .unwrap()
            .starts_with("GET /api/models HTTP/1.1")
    );

    let (base, server) = serve_once(plain(
        "HTTP/1.1 200 OK",
        &format!(r#"{{"chats":[{CHAT}]}}"#),
    ))
    .await;
    let chats = Client::new(&base)
        .unwrap()
        .chats("bald eagle")
        .await
        .unwrap();
    assert_eq!(chats.len(), 1);
    assert!(
        server
            .await
            .unwrap()
            .starts_with("GET /api/chats?q=bald+eagle HTTP/1.1")
    );

    let (base, server) = serve_once(plain(
        "HTTP/1.1 200 OK",
        &format!(r#"{{"chat":{CHAT},"messages":[{}]}}"#, msg("user", "hi")),
    ))
    .await;
    let detail = Client::new(&base)
        .unwrap()
        .chat("0123456789abcdef")
        .await
        .unwrap();
    assert_eq!(detail.messages[0].content, "hi");
    assert!(
        server
            .await
            .unwrap()
            .starts_with("GET /api/chats/0123456789abcdef HTTP/1.1")
    );
}

#[tokio::test]
async fn delete_succeeds_on_204_and_reports_a_404() {
    let (base, server) = serve_once(plain("HTTP/1.1 204 No Content", "")).await;
    Client::new(&base)
        .unwrap()
        .delete_chat("0123456789abcdef")
        .await
        .unwrap();
    assert!(
        server
            .await
            .unwrap()
            .starts_with("DELETE /api/chats/0123456789abcdef HTTP/1.1")
    );

    let body = r#"{"error":{"code":"not_found","message":"There is no such chat."}}"#;
    let (base, _server) = serve_once(plain("HTTP/1.1 404 Not Found", body)).await;
    let err = Client::new(&base)
        .unwrap()
        .delete_chat("0123456789abcdef")
        .await
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "There is no such chat. (HTTP 404)");
}

#[tokio::test]
async fn an_unreachable_server_gets_a_message_that_names_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let err = Client::new(&base).unwrap().models().await.unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!("cannot reach aihub-web at {base}")
    );
}
