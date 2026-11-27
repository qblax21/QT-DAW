//! `qtdaw-mcp`: lets Claude (Claude Desktop, Claude Code...) analyze and mix in QT DAW.
//!
//! It is an MCP ("Model Context Protocol") server. Claude starts this program and talks to
//! it over stdin/stdout, one JSON-RPC message per line. It lists the mixing tools, and
//! passes each tool call on to the running QT DAW app over a local connection
//! (127.0.0.1, port 47390 or `QTDAW_PORT`), where the work is actually done.
//!
//! Nothing but protocol messages may be printed to stdout; diagnostics go to stderr.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::{Value, json};

/// Must match `remote.rs` in the app.
const DEFAULT_PORT: u16 = 47_390;
const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
QT DAW is the user's own DAW; these tools read and change the session that's open in it.
A good first pass at a mix:
1. get_session (tracks, buses, routing, effects) and list_effects (effect settings, presets).
2. analyze_mix: loudness, peaks, tone per track and the master, and which tracks mask each other.
3. Make moderate changes: balance faders first, then cut low end that isn't needed (EQ low_cut),
   EQ cuts where tracks mask each other, compression where a track's loudness range is large,
   reverb/delay on buses with sends. Prefer cuts to boosts, and small steps (1-3 dB).
4. analyze_mix again to check, and repeat if needed.
Typical targets for a finished master: about -14 LUFS integrated for streaming (louder genres
-10 to -8), true peak at or below -1 dBTP, no clipped samples, stereo correlation above 0.
Explain what you changed and why in plain language: the user is learning to mix.
Changes can't be undone yet: the app keeps a copy of the session from before your first
change (session.before-claude.qtdaw in the session folder). Only save_session when the user
agrees. You can't record or delete tracks.";

fn port() -> u16 {
    std::env::var("QTDAW_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

fn main() {
    let port = port();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        // Some tools put an invisible byte-order mark before the first line.
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, port),
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } })),
        };
        if let Some(r) = reply {
            if writeln!(out, "{r}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

/// Answer one message. Notifications (no `id`) get no answer.
fn handle(msg: &Value, port: u16) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method")?.as_str().unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = PROTOCOLS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOLS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "qtdaw", "title": "QT DAW", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => Ok(call(&params, port)),
        _ => Err(json!({ "code": -32601, "message": format!("unknown method '{method}'") })),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    })
}

/// Run a tool in the app and wrap its answer the way MCP expects.
fn call(params: &Value, port: u16) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let (text, is_error) = if !tools().iter().any(|t| t["name"] == name) {
        (format!("unknown tool '{name}'"), true)
    } else {
        match ask_app(port, name, &args) {
            Ok(v) => (if v.is_string() { v.as_str().unwrap_or_default().to_string() } else { v.to_string() }, false),
            Err(e) => (e, true),
        }
    };
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn ask_app(port: u16, tool: &str, args: &Value) -> Result<Value, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|_| {
        "QT DAW isn't running (or its Claude connection is off). Ask the user to open QT DAW, then try again.".to_string()
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(320))).ok();
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{}", json!({ "tool": tool, "args": args })).map_err(|e| format!("lost the connection to QT DAW: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|e| format!("QT DAW didn't answer: {e}"))?;
    let reply: Value = serde_json::from_str(&line).map_err(|_| "QT DAW closed the connection".to_string())?;
    match reply.get("ok") {
        Some(v) => Ok(v.clone()),
        None => Err(reply["error"].as_str().unwrap_or("unknown error").to_string()),
    }
}

/// A track or bus: its name, its id, or "master".
fn channel_arg(what: &str) -> Value {
    json!({ "type": ["string", "integer"], "description": format!("{what}: a track or bus name (or its id), or \"master\"") })
}

fn tools() -> Vec<Value> {
    let ro = json!({ "readOnlyHint": true, "openWorldHint": false });
    let rw = json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false });
    vec![
        json!({
            "name": "get_session",
            "title": "Get session",
            "description": "The open session: tempo, length, every track (volume, pan, mute/solo, output, sends, effects with their settings and effect_id), buses, and the master.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": ro,
        }),
        json!({
            "name": "list_effects",
            "title": "List effects",
            "description": "The built-in effects with every setting (key, unit, range, default), the chain presets, and how signal flows through tracks, buses and the master.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": ro,
        }),
        json!({
            "name": "analyze_mix",
            "title": "Analyze mix",
            "description": "Render the mix offline (with all effects; solo is ignored, mutes are kept) and measure the master and each track: \
                integrated and max short-term loudness (LUFS), loudness range (LU), true peak (master), sample peak, RMS, how much of the time it plays, \
                stereo correlation (master), average level per frequency region (tone_db, in the order of tone_regions), and masking: pairs of tracks \
                that are both strong at the same frequencies at the same time. Track numbers are each track's own contribution after its effects, fader \
                and bus fader. Takes a few seconds for a full song.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start_sec": { "type": "number", "description": "Measure from here (default: the start)." },
                    "end_sec": { "type": "number", "description": "Measure up to here (default: the end of the last recording)." },
                    "detail": { "type": "boolean", "description": "Also return the full 1/3-octave spectrum (30 bands) of everything." },
                },
            },
            "annotations": ro,
        }),
        json!({
            "name": "set_channel",
            "title": "Set channel",
            "description": "Change a track's or bus's volume, pan, mute, solo or name, a track's output (the master or a bus), or the master volume. Give only what should change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": channel_arg("Which channel"),
                    "volume_db": { "type": "number", "description": "Fader in dB (-96 to +6; 0 = unity)." },
                    "pan": { "type": "number", "description": "-1 = left, 0 = center, 1 = right (balance for buses)." },
                    "mute": { "type": "boolean" },
                    "solo": { "type": "boolean" },
                    "output": channel_arg("Tracks only: where the track plays into"),
                    "name": { "type": "string", "description": "New name." },
                },
                "required": ["channel"],
            },
            "annotations": rw,
        }),
        json!({
            "name": "set_send",
            "title": "Set send",
            "description": "Send a copy of a track to a bus (for shared reverb/delay), change a send's level or pre/post-fader, or remove it. New sends start at -10 dB, post-fader.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "track": channel_arg("The track"),
                    "bus": channel_arg("The bus"),
                    "level_db": { "type": "number", "description": "Send level in dB (-96 to +6)." },
                    "pre_fader": { "type": "boolean", "description": "true = the send ignores the track's fader." },
                    "remove": { "type": "boolean", "description": "true = remove this send." },
                },
                "required": ["track", "bus"],
            },
            "annotations": rw,
        }),
        json!({
            "name": "add_bus",
            "title": "Add bus",
            "description": "Add a stereo aux bus (it plays into the master). Returns its bus_id. Then add effects to it and route or send tracks to it.",
            "inputSchema": { "type": "object", "properties": { "name": { "type": "string", "description": "e.g. \"Reverb\" or \"Drums\"." } } },
            "annotations": rw,
        }),
        json!({
            "name": "add_effect",
            "title": "Add effect",
            "description": "Add a built-in effect to a channel's chain (up to 8 per channel; they run first to last). Returns its effect_id. Settings you leave out use the defaults.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": channel_arg("Which channel"),
                    "effect": { "type": "string", "enum": ["eq", "compressor", "gate", "de_esser", "limiter", "delay", "reverb"] },
                    "settings": { "type": "object", "description": "Setting keys from list_effects, e.g. {\"low_cut\": 90, \"mid1_freq\": 300, \"mid1_gain\": -3}. Choice settings take the option name.", "additionalProperties": { "type": ["number", "string"] } },
                    "position": { "type": "integer", "description": "Slot to insert at, 0 = first (default: last)." },
                },
                "required": ["channel", "effect"],
            },
            "annotations": rw,
        }),
        json!({
            "name": "set_effect",
            "title": "Set effect",
            "description": "Change an effect's settings and/or bypass it. Get effect_id from get_session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "effect_id": { "type": "integer" },
                    "settings": { "type": "object", "description": "Only the settings to change.", "additionalProperties": { "type": ["number", "string"] } },
                    "bypass": { "type": "boolean", "description": "true = switch the effect off (kept in place)." },
                },
                "required": ["effect_id"],
            },
            "annotations": rw,
        }),
        json!({
            "name": "remove_effect",
            "title": "Remove effect",
            "description": "Remove an effect from its chain.",
            "inputSchema": { "type": "object", "properties": { "effect_id": { "type": "integer" } }, "required": ["effect_id"] },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false },
        }),
        json!({
            "name": "add_chain_preset",
            "title": "Add chain preset",
            "description": "Add a ready-made effect chain (from list_effects) to the end of a channel's chain, e.g. \"Lead Vocal\" on a vocal track or \"Reverb Bus\" on a bus. Returns the new effect_ids, so you can then fine-tune them with set_effect.",
            "inputSchema": {
                "type": "object",
                "properties": { "channel": channel_arg("Which channel"), "preset": { "type": "string" } },
                "required": ["channel", "preset"],
            },
            "annotations": rw,
        }),
        json!({
            "name": "transport",
            "title": "Transport",
            "description": "Play, stop, or move the playhead, so the user can listen to a part of the song.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["play", "stop", "go_to"] },
                    "position_sec": { "type": "number", "description": "For go_to." },
                },
                "required": ["action"],
            },
            "annotations": rw,
        }),
        json!({
            "name": "save_session",
            "title": "Save session",
            "description": "Save the session to disk (like Ctrl+S). Only when the user agrees with the changes.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": rw,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn rpc(method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
    }

    #[test]
    fn handshake_and_tool_list() {
        let r = handle(&rpc("initialize", json!({ "protocolVersion": "2025-03-26" })), 1).unwrap();
        assert_eq!(r["id"], 7);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        let r = handle(&rpc("initialize", json!({ "protocolVersion": "1999-01-01" })), 1).unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOLS[0]);
        assert!(handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), 1).is_none());
        let r = handle(&rpc("tools/list", json!({})), 1).unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 12);
        for t in tools {
            assert!(t["description"].as_str().unwrap().len() > 20, "{}", t["name"]);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
        assert_eq!(handle(&rpc("nope", json!({})), 1).unwrap()["error"]["code"], -32601);
    }

    #[test]
    fn calls_are_forwarded_to_the_app() {
        // A fake app: answers "ok" with the tool name, or an error for set_effect.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut s = stream.unwrap();
                let mut line = String::new();
                BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let answer = if req["tool"] == "set_effect" {
                    json!({ "error": "there is no effect with id 9" })
                } else {
                    json!({ "ok": { "tool": req["tool"], "args": req["args"] } })
                };
                writeln!(s, "{answer}").unwrap();
            }
        });
        let r = handle(&rpc("tools/call", json!({ "name": "get_session", "arguments": {} })), port).unwrap();
        assert_eq!(r["result"]["isError"], false);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("get_session"));
        let r = handle(&rpc("tools/call", json!({ "name": "set_effect", "arguments": { "effect_id": 9 } })), port).unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("id 9"));
    }

    #[test]
    fn a_closed_app_is_a_friendly_error() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let r = handle(&rpc("tools/call", json!({ "name": "get_session" })), port).unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("isn't running"));
        let r = handle(&rpc("tools/call", json!({ "name": "rm_rf" })), port).unwrap();
        assert_eq!(r["result"]["isError"], true);
    }
}
