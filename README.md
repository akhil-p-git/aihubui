# aihubui

A terminal client for the aihub web chat (`~/work/Dev/aihub`). It talks to the same HTTP API as the browser page, so your chats are shared with the web UI, and aihub keeps enforcing its own rules: which models are usable, which keys are set, and the 30-day retention.

```text
┌ Chats (2) ───────────────────┐┌ Reply with exactly one word: pong ───────────────┐
│> Reply with exactly one wor… ││you                                               │
│  qwen3:8b · now              ││  Reply with exactly one word: pong               │
│  Generate an image of a ba…  ││                                                  │
│  qwen3:8b · 1m               ││qwen3:8b                                          │
│                              ││  pong                                            │
│                              │┌ qwen3:8b [local] intensity off ──────────────────┐
│                              ││Type a message                                    │
└──────────────────────────────┘└──────────────────────────────────────────────────┘
```

## Install

Needs Linux, Rust (built and tested with 1.96.0 on Arch), and a running `aihub-web` (`systemctl --user start aihub-web`).

```bash
cd ~/work/Dev/aihubui
cargo build --release
ln -sfn "$PWD/target/release/aihubui" ~/.local/bin/aihubui   # ~/.local/bin must be on your PATH
```

## Use

```bash
aihubui                          # talks to http://127.0.0.1:8787
aihubui http://127.0.0.1:9000    # another address (plain http only)
```

If the server isn't running, aihubui says so and prints the `systemctl` command to start it; `r` retries.

| Where | Key | Action |
| --- | --- | --- |
| chat list | `j` / `k`, arrows | move |
| | `Enter` | open the chat and start writing |
| | `n` | new chat |
| | `d` | delete the chat (asks first) |
| | `/` | search chats; `Esc` clears the search |
| | `m`, `e` | pick the model, cycle its intensity (reasoning effort) |
| | `Tab` / `i` | go to the message box |
| | `q` | quit |
| message box | `Enter` | send |
| | `Alt+Enter`, `Ctrl+J` | newline (pasted text keeps its newlines and is never sent by itself) |
| | `Ctrl+O`, `Ctrl+E` | pick the model, cycle intensity |
| | `Ctrl+W`, `Ctrl+U` | delete a word, clear the box |
| | `Esc` | stop the reply while one streams, otherwise back to the list |
| anywhere | `PgUp` / `PgDn` | scroll the conversation |
| | `Ctrl+C` | stop a reply, or quit when idle |

## Behavior worth knowing

- **Paid models ask first.** Sending to a paid model, or an Ollama cloud model whose prompt leaves this machine, shows a confirmation. Approval lasts for that model until aihubui restarts. A model whose key is missing is listed but can't be picked; the reason is shown next to it.
- **Sending goes to the open chat.** Opening a chat (`Enter`) moves you to its message box, and `Enter` there appends to that chat. Use `n` first for a new one. The box title shows which model will answer.
- **Stopping keeps the partial reply.** `Esc` cuts the connection; aihub stores what had streamed, and aihubui reloads the chat after a moment to show it.
- **Image models are hidden from the picker**, and images in existing chats show as a `[image WxH, not shown here]` placeholder. Generate images in the web UI.
- **Limits of v1:** the message box has no cursor movement (type at the end, `Backspace`, `Ctrl+W`, `Ctrl+U`), replies are shown as plain text rather than rendered Markdown, and projects and scheduled tasks aren't shown.

## Develop

```bash
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
```

`tests/client.rs` runs the real HTTP client against a fake server on a loopback port, including replies split mid-event and mid-character. The wire format comes from `app/aihub_web/chat.py` and `db.py` in the aihub repo.
