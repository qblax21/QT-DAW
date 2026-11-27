//! The tools Claude can use (through `qtdaw-mcp`, see `remote.rs`): read the session,
//! analyze the mix, and change faders, pans, sends, routing and effects.
//!
//! Every change shows in the status bar. The first change in a session saves a copy of the
//! session as it was (`session.before-claude.qtdaw` in the session folder), because there
//! is no undo yet.

use std::sync::atomic::Ordering;
use std::sync::mpsc;

use qtdaw_engine::fx::presets::{CHAIN_PRESETS, PresetFor};
use qtdaw_engine::fx::{EffectKind, ParamSpec, Unit};
use qtdaw_engine::mixcheck::{self, Options};
use qtdaw_engine::render::Snapshot;
use qtdaw_engine::session::{InsertModel, MASTER, MAX_BUSES, MAX_INSERTS, SendModel};
use serde_json::{Map, Value, json};

use crate::app::{Chain, QtDaw};
use crate::mixer::{MAX_DB, MIN_DB};
use crate::remote::Reply;

type R<T> = Result<T, String>;

/// Copy of the session from just before Claude's first change.
pub const BACKUP_FILE: &str = "session.before-claude.qtdaw";

/// A mixer channel named in a request.
#[derive(Debug, Clone, Copy)]
enum Ch {
    Track(usize),
    Bus(usize),
    Master,
}

impl QtDaw {
    /// Answer Claude's requests. Runs every frame.
    pub fn poll_remote(&mut self) {
        let jobs: Vec<_> = self.remote.jobs.try_iter().collect();
        for job in jobs {
            if job.tool == "analyze_mix" {
                self.analyze(&job.args, job.reply);
            } else {
                let result = self.run_tool(&job.tool, &job.args);
                if let Err(e) = &result {
                    self.say(format!("Claude: {e}"));
                }
                let _ = job.reply.send(result);
            }
        }
    }

    fn run_tool(&mut self, tool: &str, a: &Value) -> Reply {
        match tool {
            "get_session" => Ok(self.describe()),
            "list_effects" => Ok(list_effects()),
            "set_channel" => self.set_channel(a),
            "set_send" => self.set_send(a),
            "add_bus" => self.tool_add_bus(a),
            "add_effect" => self.tool_add_effect(a),
            "set_effect" => self.tool_set_effect(a),
            "remove_effect" => self.tool_remove_effect(a),
            "add_chain_preset" => self.tool_add_preset(a),
            "transport" => self.tool_transport(a),
            "save_session" => self.tool_save(),
            _ => Err(format!("unknown tool '{tool}'")),
        }
    }

    /// Measuring renders the whole song, so it runs on its own thread and answers when done.
    fn analyze(&mut self, a: &Value, reply: mpsc::Sender<Reply>) {
        let opts = (|| -> R<Options> {
            Ok(Options {
                start_sec: num(a, "start_sec")?.map(f64::from),
                end_sec: num(a, "end_sec")?.map(f64::from),
                detail: flag(a, "detail")?.unwrap_or(false),
            })
        })();
        let opts = match opts {
            Ok(o) => o,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let snap = Snapshot { session: self.session.clone(), audio: self.audio.clone() };
        self.say("Claude is listening to the mix (analyzing)...");
        std::thread::spawn(move || {
            let result = mixcheck::analyze(&snap, opts)
                .map_err(|e| format!("{e:#}"))
                .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()));
            let _ = reply.send(result);
        });
    }

    // ------------------------------------------------------------------ helpers

    /// Save a copy of the session before Claude's first change (once per session).
    fn backup(&mut self) {
        if self.claude_backup {
            return;
        }
        let Some(folder) = &self.folder else { return };
        if let Ok(json) = serde_json::to_string_pretty(&self.session) {
            if std::fs::write(folder.join(BACKUP_FILE), json).is_ok() {
                self.claude_backup = true;
            }
        }
    }

    fn changed(&mut self, msg: &str) -> Reply {
        self.dirty = true;
        self.say(format!("Claude: {msg}"));
        Ok(json!({ "done": msg }))
    }

    fn channel(&self, v: &Value) -> R<Ch> {
        let s = &self.session;
        if let Some(id) = v.as_u64() {
            if id == MASTER {
                return Ok(Ch::Master);
            }
            if let Some(i) = s.tracks.iter().position(|t| t.id == id) {
                return Ok(Ch::Track(i));
            }
            if let Some(i) = s.buses.iter().position(|b| b.id == id) {
                return Ok(Ch::Bus(i));
            }
            return Err(format!("there is no track or bus with id {id}"));
        }
        let name = v.as_str().ok_or("a channel is a track or bus name, its id, or \"master\"")?.trim();
        if name.eq_ignore_ascii_case("master") {
            return Ok(Ch::Master);
        }
        let tracks: Vec<usize> = (0..s.tracks.len()).filter(|&i| s.tracks[i].name.eq_ignore_ascii_case(name)).collect();
        let buses: Vec<usize> = (0..s.buses.len()).filter(|&i| s.buses[i].name.eq_ignore_ascii_case(name)).collect();
        match (tracks.as_slice(), buses.as_slice()) {
            ([i], []) => Ok(Ch::Track(*i)),
            ([], [i]) => Ok(Ch::Bus(*i)),
            ([], []) => {
                let names: Vec<String> = s.tracks.iter().map(|t| &t.name).chain(s.buses.iter().map(|b| &b.name)).map(|n| format!("\"{n}\"")).collect();
                Err(format!("no track or bus is called \"{name}\". Channels: {}, \"master\"", names.join(", ")))
            }
            _ => Err(format!("more than one channel is called \"{name}\"; use its id instead")),
        }
    }

    fn ch_name(&self, ch: Ch) -> String {
        match ch {
            Ch::Track(i) => self.session.tracks[i].name.clone(),
            Ch::Bus(i) => self.session.buses[i].name.clone(),
            Ch::Master => "Master".into(),
        }
    }

    fn chain_of(&self, ch: Ch) -> Chain {
        match ch {
            Ch::Track(i) => Chain::Track(self.session.tracks[i].id),
            Ch::Bus(i) => Chain::Bus(self.session.buses[i].id),
            Ch::Master => Chain::Master,
        }
    }

    /// A bus (by name or id), or the master.
    fn destination(&self, v: &Value) -> R<u64> {
        match self.channel(v)? {
            Ch::Master => Ok(MASTER),
            Ch::Bus(i) => Ok(self.session.buses[i].id),
            Ch::Track(i) => Err(format!("\"{}\" is a track; tracks can only play into a bus or the master", self.session.tracks[i].name)),
        }
    }

    /// Which channel holds effect `id`, and where in its chain.
    fn find_effect(&self, id: u64) -> R<(Ch, usize)> {
        let s = &self.session;
        let pos = |list: &[InsertModel]| list.iter().position(|m| m.id == id);
        for (i, t) in s.tracks.iter().enumerate() {
            if let Some(p) = pos(&t.inserts) {
                return Ok((Ch::Track(i), p));
            }
        }
        for (i, b) in s.buses.iter().enumerate() {
            if let Some(p) = pos(&b.inserts) {
                return Ok((Ch::Bus(i), p));
            }
        }
        pos(&s.master_inserts).map(|p| (Ch::Master, p)).ok_or(format!("there is no effect with id {id} (get_session lists them)"))
    }

    fn chain_list(&mut self, ch: Ch) -> &mut Vec<InsertModel> {
        match ch {
            Ch::Track(i) => &mut self.session.tracks[i].inserts,
            Ch::Bus(i) => &mut self.session.buses[i].inserts,
            Ch::Master => &mut self.session.master_inserts,
        }
    }

    // ------------------------------------------------------------------ reading

    fn describe(&self) -> Value {
        let s = &self.session;
        let sr = s.sample_rate as f64;
        let end = s
            .tracks
            .iter()
            .flat_map(|t| &t.clips)
            .filter_map(|c| Some(c.start + self.audio.get(&c.file)?.frames() as i64))
            .max()
            .unwrap_or(0);
        let tracks: Vec<Value> = s
            .tracks
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "name": t.name,
                    "volume_db": r2(t.volume_db),
                    "pan": r2(t.pan),
                    "mute": t.mute,
                    "solo": t.solo,
                    "output": s.output_name(t.output),
                    "input": t.input + 1,
                    "recorded_clips": t.clips.len(),
                    "sends": t.sends.iter().map(|x| json!({
                        "bus": s.output_name(x.bus),
                        "level_db": r2(x.level_db),
                        "pre_fader": x.pre_fader,
                    })).collect::<Vec<_>>(),
                    "effects": t.inserts.iter().map(effect_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        let buses: Vec<Value> = s
            .buses
            .iter()
            .map(|b| {
                let fed: Vec<&str> = s
                    .tracks
                    .iter()
                    .filter(|t| t.output == b.id || t.sends.iter().any(|x| x.bus == b.id))
                    .map(|t| t.name.as_str())
                    .collect();
                json!({
                    "id": b.id,
                    "name": b.name,
                    "volume_db": r2(b.volume_db),
                    "balance": r2(b.pan),
                    "mute": b.mute,
                    "solo": b.solo,
                    "fed_by": fed,
                    "effects": b.inserts.iter().map(effect_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "name": s.name,
            "saved_in": self.folder.as_ref().map(|f| f.display().to_string()),
            "unsaved_changes": self.dirty,
            "sample_rate": s.sample_rate,
            "tempo_bpm": s.tempo,
            "beats_per_bar": s.beats_per_bar,
            "length_sec": (end as f64 / sr * 10.0).round() / 10.0,
            "playing": self.busy(),
            "position_sec": (self.shared.playhead.load(Ordering::Relaxed) as f64 / sr * 10.0).round() / 10.0,
            "master": { "volume_db": r2(s.master_db), "effects": s.master_inserts.iter().map(effect_json).collect::<Vec<_>>() },
            "tracks": tracks,
            "buses": buses,
        })
    }

    // ------------------------------------------------------------------ mixing

    fn set_channel(&mut self, a: &Value) -> Reply {
        let ch = self.channel(need(a, "channel")?)?;
        let volume = num(a, "volume_db")?;
        let pan = num(a, "pan")?;
        let mute = flag(a, "mute")?;
        let solo = flag(a, "solo")?;
        let name = text(a, "name")?.map(str::trim).filter(|n| !n.is_empty());
        let output = match a.get("output").filter(|v| !v.is_null()) {
            Some(v) => Some(self.destination(v)?),
            None => None,
        };
        if volume.is_none() && pan.is_none() && mute.is_none() && solo.is_none() && name.is_none() && output.is_none() {
            return Err("nothing to change: give volume_db, pan, mute, solo, output or name".into());
        }
        if matches!(ch, Ch::Master) && (pan.is_some() || mute.is_some() || solo.is_some() || name.is_some()) {
            return Err("the master only has volume_db (and effects)".into());
        }
        if output.is_some() && !matches!(ch, Ch::Track(_)) {
            return Err("only tracks have an output; buses always play into the master".into());
        }
        self.backup();
        let who = self.ch_name(ch);
        let out_name = output.map(|o| self.session.output_name(o).to_string());
        let mut done = Vec::new();
        let vol = volume.map(|v| v.clamp(MIN_DB, MAX_DB));
        let pan = pan.map(|p| p.clamp(-1.0, 1.0));
        if let Some(v) = vol {
            done.push(format!("volume {v:.1} dB"));
        }
        if let Some(p) = pan {
            done.push(format!("pan {}", pan_text(p)));
        }
        if let Some(m) = mute {
            done.push(if m { "muted".into() } else { "unmuted".into() });
        }
        if let Some(x) = solo {
            done.push(if x { "soloed".into() } else { "unsoloed".into() });
        }
        if let Some(n) = name {
            done.push(format!("renamed to \"{n}\""));
        }
        if let Some(o) = &out_name {
            done.push(format!("output to {o}"));
        }
        match ch {
            Ch::Track(i) => {
                let t = &mut self.session.tracks[i];
                if let Some(v) = vol {
                    t.volume_db = v;
                }
                if let Some(p) = pan {
                    t.pan = p;
                }
                if let Some(m) = mute {
                    t.mute = m;
                }
                if let Some(x) = solo {
                    t.solo = x;
                }
                if let Some(n) = name {
                    t.name = n.to_string();
                }
                if let Some(o) = output {
                    t.output = o;
                    // A track can't also send to the bus it plays into.
                    t.sends.retain(|x| x.bus != o);
                }
            }
            Ch::Bus(i) => {
                let b = &mut self.session.buses[i];
                if let Some(v) = vol {
                    b.volume_db = v;
                }
                if let Some(p) = pan {
                    b.pan = p;
                }
                if let Some(m) = mute {
                    b.mute = m;
                }
                if let Some(x) = solo {
                    b.solo = x;
                }
                if let Some(n) = name {
                    b.name = n.to_string();
                }
            }
            Ch::Master => {
                if let Some(v) = vol {
                    self.session.master_db = v;
                }
            }
        }
        self.changed(&format!("{who}: {}", done.join(", ")))
    }

    fn set_send(&mut self, a: &Value) -> Reply {
        let Ch::Track(ti) = self.channel(need(a, "track")?)? else {
            return Err("sends go from a track to a bus; 'track' must be a track".into());
        };
        let bus = self.destination(need(a, "bus")?)?;
        if bus == MASTER {
            return Err("'bus' must be a bus (every track already plays into the master unless routed elsewhere)".into());
        }
        let level = num(a, "level_db")?.map(|v| v.clamp(MIN_DB, MAX_DB));
        let pre = flag(a, "pre_fader")?;
        let remove = flag(a, "remove")?.unwrap_or(false);
        let bus_name = self.session.output_name(bus).to_string();
        let t = &self.session.tracks[ti];
        if t.output == bus {
            return Err(format!("\"{}\" already plays into {bus_name} (its output); a send there would double it", t.name));
        }
        let exists = t.sends.iter().any(|x| x.bus == bus);
        if remove && !exists {
            return Err(format!("\"{}\" has no send to {bus_name}", t.name));
        }
        if !exists && t.sends.len() >= MAX_BUSES {
            return Err("this track already has a send to every bus".into());
        }
        self.backup();
        let t = &mut self.session.tracks[ti];
        let who = t.name.clone();
        if remove {
            t.sends.retain(|x| x.bus != bus);
            return self.changed(&format!("removed {who}'s send to {bus_name}"));
        }
        if !exists {
            t.sends.push(SendModel { bus, level_db: -10.0, pre_fader: false });
        }
        let send = t.sends.iter_mut().find(|x| x.bus == bus).expect("just ensured");
        if let Some(l) = level {
            send.level_db = l;
        }
        if let Some(p) = pre {
            send.pre_fader = p;
        }
        let msg = format!(
            "{who} send to {bus_name} {:.1} dB{}",
            send.level_db,
            if send.pre_fader { " (pre-fader)" } else { "" }
        );
        self.changed(&msg)
    }

    fn tool_add_bus(&mut self, a: &Value) -> Reply {
        let name = text(a, "name")?.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        self.backup();
        let Some(id) = self.session.add_bus(name) else {
            return Err(format!("a session can have up to {MAX_BUSES} buses"));
        };
        self.rebuild_buses();
        let name = self.session.output_name(id).to_string();
        self.changed(&format!("added bus \"{name}\""))?;
        Ok(json!({ "done": format!("added bus \"{name}\""), "bus_id": id, "name": name }))
    }

    // ------------------------------------------------------------------ effects

    fn tool_add_effect(&mut self, a: &Value) -> Reply {
        let ch = self.channel(need(a, "channel")?)?;
        let kind = parse_kind(need(a, "effect")?)?;
        let len = self.chain_list(ch).len();
        if len >= MAX_INSERTS {
            return Err(format!("{} already has {MAX_INSERTS} effects (the most a channel can have)", self.ch_name(ch)));
        }
        let pos = match a.get("position").filter(|v| !v.is_null()) {
            Some(v) => (v.as_u64().ok_or("'position' must be a whole number (0 = first)")? as usize).min(len),
            None => len,
        };
        let mut ins = InsertModel::new(0, kind, &kind.defaults());
        let set = apply_settings(&mut ins, a.get("settings"))?;
        let id = self.session.new_insert(kind, &kind.defaults()).id;
        ins.id = id;
        self.backup();
        self.chain_list(ch).insert(pos, ins);
        self.sync_inserts();
        let mut msg = format!("added {} to {} (slot {})", kind.name(), self.ch_name(ch), pos + 1);
        if !set.is_empty() {
            msg += &format!(": {}", set.join(", "));
        }
        self.changed(&msg)?;
        Ok(json!({ "done": msg, "effect_id": id }))
    }

    fn tool_set_effect(&mut self, a: &Value) -> Reply {
        let id = need(a, "effect_id")?.as_u64().ok_or("'effect_id' must be a number")?;
        let (ch, pos) = self.find_effect(id)?;
        let mut m = self.chain_list(ch)[pos].clone();
        let mut done = apply_settings(&mut m, a.get("settings"))?;
        if let Some(b) = flag(a, "bypass")? {
            m.bypass = b;
            done.push(if b { "bypassed".into() } else { "on".into() });
        }
        if done.is_empty() {
            return Err("nothing to change: give settings and/or bypass".into());
        }
        self.backup();
        let msg = format!("{} {}: {}", self.ch_name(ch), m.kind.name(), done.join(", "));
        self.chain_list(ch)[pos] = m;
        self.changed(&msg)
    }

    fn tool_remove_effect(&mut self, a: &Value) -> Reply {
        let id = need(a, "effect_id")?.as_u64().ok_or("'effect_id' must be a number")?;
        let (ch, pos) = self.find_effect(id)?;
        self.backup();
        let m = self.chain_list(ch).remove(pos);
        self.view.open_fx.retain(|x| *x != id);
        self.sync_inserts();
        let msg = format!("removed {} from {}", m.kind.name(), self.ch_name(ch));
        self.changed(&msg)
    }

    fn tool_add_preset(&mut self, a: &Value) -> Reply {
        let ch = self.channel(need(a, "channel")?)?;
        let want = text(a, "preset")?.ok_or("'preset' is required")?;
        let preset = CHAIN_PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(want.trim())).ok_or_else(|| {
            let names: Vec<&str> = CHAIN_PRESETS.iter().map(|p| p.name).collect();
            format!("no preset called \"{want}\". Presets: {}", names.join(", "))
        })?;
        let before: Vec<u64> = self.chain_list(ch).iter().map(|m| m.id).collect();
        if before.len() >= MAX_INSERTS {
            return Err(format!("{} already has {MAX_INSERTS} effects", self.ch_name(ch)));
        }
        self.backup();
        self.add_preset(self.chain_of(ch), preset);
        let added: Vec<Value> = self
            .chain_list(ch)
            .iter()
            .filter(|m| !before.contains(&m.id))
            .map(|m| json!({ "effect_id": m.id, "effect": m.kind.name() }))
            .collect();
        let mut msg = format!("added the {} preset to {}", preset.name, self.ch_name(ch));
        if added.len() < preset.chain.len() {
            msg += &format!(" (only {} of {} effects fit)", added.len(), preset.chain.len());
        }
        let meant_for = match (preset.target, ch) {
            (PresetFor::Track, Ch::Track(_)) | (PresetFor::Bus, Ch::Bus(_)) | (PresetFor::Master, Ch::Master) => None,
            (t, _) => Some(format!("note: this preset is meant for a {}", for_text(t))),
        };
        self.changed(&msg)?;
        Ok(json!({ "done": msg, "added": added, "note": meant_for }))
    }

    // ------------------------------------------------------------------ transport

    fn tool_transport(&mut self, a: &Value) -> Reply {
        let action = text(a, "action")?.ok_or("'action' is required: play, stop or go_to")?;
        let sr = self.sample_rate();
        match action {
            "play" => {
                if !self.busy() {
                    self.shared.request(qtdaw_engine::engine::Request::Play);
                }
                self.say("Claude: play");
            }
            "stop" => {
                self.stop();
                self.say("Claude: stop");
            }
            "go_to" => {
                let at = num(a, "position_sec")?.ok_or("'position_sec' is required for go_to")?.max(0.0);
                self.shared.locate((at as f64 * sr) as i64);
                self.say(format!("Claude: go to {at:.1} s"));
            }
            other => return Err(format!("unknown action '{other}': use play, stop or go_to")),
        }
        Ok(json!({ "done": action }))
    }

    fn tool_save(&mut self) -> Reply {
        self.save();
        if self.dirty {
            return Err(self.status_text().unwrap_or("save failed").to_string());
        }
        self.say("Claude: saved the session");
        Ok(json!({ "saved_in": self.folder.as_ref().map(|f| f.display().to_string()) }))
    }
}

// ---------------------------------------------------------------------- free helpers

fn need<'a>(a: &'a Value, key: &str) -> R<&'a Value> {
    a.get(key).filter(|v| !v.is_null()).ok_or(format!("'{key}' is required"))
}

fn num(a: &Value, key: &str) -> R<Option<f32>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().filter(|x| x.is_finite()).map(|x| Some(x as f32)).ok_or(format!("'{key}' must be a number")),
    }
}

fn flag(a: &Value, key: &str) -> R<Option<bool>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or(format!("'{key}' must be true or false")),
    }
}

fn text<'a>(a: &'a Value, key: &str) -> R<Option<&'a str>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or(format!("'{key}' must be text")),
    }
}

fn r2(x: f32) -> f64 {
    (x as f64 * 100.0).round() / 100.0
}

fn pan_text(p: f32) -> String {
    match (p * 100.0).round() as i32 {
        0 => "center".into(),
        n if n < 0 => format!("{} L", -n),
        n => format!("{n} R"),
    }
}

fn for_text(t: PresetFor) -> &'static str {
    match t {
        PresetFor::Track => "track",
        PresetFor::Bus => "bus",
        PresetFor::Master => "master",
    }
}

/// The name used in requests ("eq", "compressor", "de_esser"...).
fn kind_key(k: EffectKind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

fn parse_kind(v: &Value) -> R<EffectKind> {
    let norm = |s: &str| s.trim().to_lowercase().replace([' ', '-'], "_");
    let want = norm(v.as_str().ok_or("'effect' must be an effect name, like \"compressor\"")?);
    EffectKind::ALL
        .into_iter()
        .find(|&k| kind_key(k) == want || norm(k.name()) == want || norm(k.short()) == want || (want == "deesser" && k == EffectKind::DeEsser))
        .ok_or_else(|| {
            let keys: Vec<String> = EffectKind::ALL.into_iter().map(kind_key).collect();
            format!("unknown effect \"{want}\". Effects: {}", keys.join(", "))
        })
}

fn unit_text(u: Unit) -> &'static str {
    match u {
        Unit::Db => "dB",
        Unit::Hz => "Hz",
        Unit::Ms => "ms",
        Unit::Ratio => "ratio (n:1)",
        Unit::Percent => "%",
        Unit::Q => "Q (higher = narrower)",
        Unit::Choice(_) => "choice",
    }
}

fn setting_value(s: &ParamSpec, v: f32) -> Value {
    match s.unit {
        Unit::Choice(names) => json!(names.get(v.round() as usize).copied().unwrap_or("?")),
        _ => json!(r2(v)),
    }
}

fn effect_json(m: &InsertModel) -> Value {
    let settings: Map<String, Value> =
        m.kind.params().iter().zip(m.values()).map(|(s, v)| (s.key.to_string(), setting_value(s, v))).collect();
    json!({ "effect_id": m.id, "effect": kind_key(m.kind), "bypass": m.bypass, "settings": settings })
}

/// Set an effect's settings from `{"key": value}`; choice settings also take the option's
/// name. Returns a description of each change. Leaves `m` untouched on error.
fn apply_settings(m: &mut InsertModel, settings: Option<&Value>) -> R<Vec<String>> {
    let Some(v) = settings.filter(|v| !v.is_null()) else { return Ok(Vec::new()) };
    let obj = v.as_object().ok_or("'settings' must be an object, like {\"threshold\": -20}")?;
    let mut out = m.clone();
    let mut done = Vec::new();
    for (key, v) in obj {
        let Some(i) = out.kind.param_index(key) else {
            let keys: Vec<&str> = out.kind.params().iter().map(|s| s.key).collect();
            return Err(format!("{} has no setting '{key}'. Its settings: {}", out.kind.name(), keys.join(", ")));
        };
        let spec = &out.kind.params()[i];
        let value = match (v, spec.unit) {
            (Value::String(s), Unit::Choice(names)) => names
                .iter()
                .position(|n| n.eq_ignore_ascii_case(s.trim()))
                .ok_or(format!("'{key}' must be one of: {}", names.join(", ")))? as f32,
            _ => v.as_f64().filter(|x| x.is_finite()).ok_or(format!("'{key}' must be a number"))? as f32,
        };
        out.set(i, value);
        done.push(format!("{key} {}", spec.format(out.get(i))));
    }
    *m = out;
    Ok(done)
}

fn list_effects() -> Value {
    let effects: Vec<Value> = EffectKind::ALL
        .into_iter()
        .map(|k| {
            let settings: Vec<Value> = k
                .params()
                .iter()
                .map(|s| {
                    let mut j = json!({
                        "key": s.key,
                        "label": s.label,
                        "unit": unit_text(s.unit),
                        "default": setting_value(s, s.default),
                    });
                    if let Unit::Choice(names) = s.unit {
                        j["options"] = json!(names);
                    } else {
                        j["min"] = json!(s.min);
                        j["max"] = json!(s.max);
                    }
                    j
                })
                .collect();
            json!({
                "effect": kind_key(k),
                "name": k.name(),
                "what_it_does": k.description(),
                "shows_gain_reduction": k.shows_reduction(),
                "settings": settings,
            })
        })
        .collect();
    let presets: Vec<Value> = CHAIN_PRESETS
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "for": for_text(p.target),
                "what_it_does": p.hint,
                "effects": p.chain.iter().map(|(k, _)| k.name()).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "effects": effects,
        "chain_presets": presets,
        "signal_flow": "Track: recorded audio -> track effects (mono) -> fader -> pan -> output (master or a bus) and sends. \
            Bus: effects (stereo) -> fader -> balance -> master. Master: master fader -> master effects -> speakers.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_names_and_settings_parse() {
        assert_eq!(parse_kind(&json!("Compressor")).unwrap(), EffectKind::Compressor);
        assert_eq!(parse_kind(&json!("de-esser")).unwrap(), EffectKind::DeEsser);
        assert_eq!(parse_kind(&json!("Channel EQ")).unwrap(), EffectKind::Eq);
        assert!(parse_kind(&json!("flanger")).is_err());

        let mut m = InsertModel::new(1, EffectKind::Delay, &EffectKind::Delay.defaults());
        let done = apply_settings(&mut m, Some(&json!({ "feedback": 45, "sync": "1/4" }))).unwrap();
        assert_eq!(done.len(), 2);
        assert_eq!(m.get(EffectKind::Delay.param_index("feedback").unwrap()), 45.0);
        // A bad key changes nothing.
        let before = m.clone();
        assert!(apply_settings(&mut m, Some(&json!({ "feedback": 10, "nope": 1 }))).is_err());
        assert_eq!(m, before);
        // Out-of-range values are clamped to the setting's range.
        apply_settings(&mut m, Some(&json!({ "feedback": 1000 }))).unwrap();
        assert!(m.get(EffectKind::Delay.param_index("feedback").unwrap()) <= 100.0);
    }
}
