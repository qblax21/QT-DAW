//! A small local server so Claude can work in QT DAW.
//!
//! Claude talks to the `qtdaw-mcp` program (an MCP server), which connects here over
//! `127.0.0.1` (this computer only) and sends one request per line:
//! `{"tool": "set_channel", "args": {...}}`. The app answers with one line:
//! `{"ok": ...}` or `{"error": "..."}`.
//!
//! The network thread never touches the session: it hands each request to the app, which
//! runs it between frames (see `claude.rs`) and sends the answer back.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use serde_json::{Value, json};

/// Must match `qtdaw-mcp`. Both read `QTDAW_PORT` to override it.
pub const DEFAULT_PORT: u16 = 47_390;

pub fn port() -> u16 {
    std::env::var("QTDAW_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

pub type Reply = Result<Value, String>;

pub struct Job {
    pub tool: String,
    pub args: Value,
    pub reply: mpsc::Sender<Reply>,
}

pub struct Remote {
    pub jobs: mpsc::Receiver<Job>,
    /// Set when the server couldn't start (e.g. another QT DAW already has the port).
    pub error: Option<String>,
}

/// Start listening in the background.
pub fn start(ctx: egui::Context) -> Remote {
    let (tx, rx) = mpsc::channel();
    let port = port();
    let error = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            std::thread::Builder::new()
                .name("claude-remote".into())
                .spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let (tx, ctx) = (tx.clone(), ctx.clone());
                        std::thread::spawn(move || serve(stream, tx, ctx));
                    }
                })
                .ok();
            None
        }
        Err(e) => Some(format!("Claude connection is off: port {port} is busy ({e}). Is QT DAW open twice?")),
    };
    Remote { jobs: rx, error }
}

fn serve(stream: TcpStream, tx: mpsc::Sender<Job>, ctx: egui::Context) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(req) => {
                let tool = req["tool"].as_str().unwrap_or_default().to_string();
                let args = req.get("args").cloned().unwrap_or(json!({}));
                let (reply_tx, reply_rx) = mpsc::channel();
                if tx.send(Job { tool, args, reply: reply_tx }).is_err() {
                    return;
                }
                // Wake the app up so it handles the request now, not on the next mouse move.
                ctx.request_repaint();
                match reply_rx.recv_timeout(Duration::from_secs(300)) {
                    Ok(Ok(v)) => json!({ "ok": v }),
                    Ok(Err(e)) => json!({ "error": e }),
                    Err(_) => json!({ "error": "QT DAW didn't answer in time (is the window minimized or busy?)" }),
                }
            }
            Err(e) => json!({ "error": format!("bad request: {e}") }),
        };
        if writeln!(out, "{answer}").is_err() {
            return;
        }
    }
}
