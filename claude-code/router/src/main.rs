//! Jev steer / queue / interrupt router for Claude Code hooks. A port of scripts/jev_router.py
//! that also runs on Windows (std file locks instead of fcntl) and starts in a few milliseconds.
//!
//! Only messages sent while a turn is running are classified; a message sent to an idle session
//! passes through untouched and never reaches Jev.
//!   steer      allow; Claude Code injects it at the next tool boundary (native)
//!   queue      block, store in a per-session FIFO, replay it from the Stop hook
//!   interrupt  block, then return continue:false at the next tool hook
//! In shadow mode (the default) nothing is blocked or stopped; decisions are only logged.
//! Any error or timeout falls back to "allow", i.e. native behaviour.

use serde_json::{json, Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_URL: &str = "https://api.typesafe.ai/v1/systemone";
const MAX_MESSAGE_CHARS: usize = 2000;
const MAX_TASK_CHARS: usize = 500;

fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default()
}

/// Optional JSON config for clients whose hooks cannot carry plugin options.
fn config() -> Map<String, Value> {
    let path = home().join(".jev-steer-or-queue").join("config.json");
    fs::read_to_string(path).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

/// JEV_ROUTER_<NAME> env overrides the plugin option CLAUDE_PLUGIN_OPTION_<NAME>, then the config file.
fn option(name: &str, default: &str) -> String {
    for key in [format!("JEV_ROUTER_{name}"), format!("CLAUDE_PLUGIN_OPTION_{name}")] {
        if let Ok(v) = std::env::var(&key) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    match config().get(name) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Null) | None => default.to_string(),
        Some(v) => v.to_string().trim().to_string(),
    }
}

fn number(name: &str, default: f64) -> f64 {
    option(name, &default.to_string()).parse().unwrap_or(default)
}

fn api_key() -> String {
    let k = option("TYPESAFE_API_KEY", "");
    if !k.is_empty() {
        return k;
    }
    std::env::var("TYPESAFE_API_KEY").map(|s| s.trim().to_string()).unwrap_or_default()
}

fn data_dir() -> PathBuf {
    let path = std::env::var_os("CLAUDE_PLUGIN_DATA").map(PathBuf::from).unwrap_or_else(|| home().join(".jev-steer-or-queue").join("claude-code"));
    let _ = fs::create_dir_all(path.join("sessions"));
    path
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// First `n` characters (not bytes), as Python's s[:n].
fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------- session files

struct Session {
    state_path: PathBuf,
    queue_path: PathBuf,
    _lock: File,
}

impl Session {
    fn open(root: &Path, session_id: &str) -> std::io::Result<Self> {
        let id = if session_id.is_empty() { "unknown" } else { session_id };
        let safe: String = id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let base = root.join("sessions").join(safe);
        let with = |ext: &str| PathBuf::from(format!("{}{ext}", base.display()));
        // Read-write, not append: Windows refuses to lock a file opened append-only.
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(with(".lock"))?;
        lock.lock()?; // released when the process exits
        Ok(Self { state_path: with(".json"), queue_path: with(".queue.jsonl"), _lock: lock })
    }

    fn load(&self) -> Map<String, Value> {
        fs::read_to_string(&self.state_path).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    fn save(&self, state: &Map<String, Value>) -> std::io::Result<()> {
        let tmp = PathBuf::from(format!("{}.tmp", self.state_path.display()));
        fs::write(&tmp, serde_json::to_string(state)?)?;
        fs::rename(tmp, &self.state_path)
    }

    fn push(&self, prompt: &str) -> std::io::Result<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(&self.queue_path)?;
        writeln!(f, "{}", json!({ "prompt": prompt, "ts": now() }))
    }

    fn items(&self) -> Vec<String> {
        let Ok(text) = fs::read_to_string(&self.queue_path) else { return vec![] };
        let mut out = vec![];
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            // One bad line voids the queue, as in the Python version.
            match serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("prompt").and_then(Value::as_str).map(String::from)) {
                Some(p) => out.push(p),
                None => return vec![],
            }
        }
        out
    }

    fn pop(&self) -> Option<String> {
        let items = self.items();
        let first = items.first()?.clone();
        self.rewrite(&items[1..]);
        Some(first)
    }

    fn drain(&self) -> Vec<String> {
        let items = self.items();
        self.rewrite(&[]);
        items
    }

    fn rewrite(&self, items: &[String]) {
        let body: String = items.iter().map(|p| format!("{}\n", json!({ "prompt": p }))).collect();
        let _ = fs::write(&self.queue_path, body);
    }
}

// ---------------------------------------------------------------- Jev

fn ask_jev(message: &str, active_task: &str) -> Value {
    let key = api_key();
    if key.is_empty() {
        return json!({ "error": "missing_api_key" });
    }
    // The question text is kept word for word from upstream: the thresholds were measured on it.
    let body = json!({
        "model": option("JEV_MODEL", "jev-latest"),
        "state": { "new_message": cut(message, MAX_MESSAGE_CHARS), "active_task": cut(active_task, MAX_TASK_CHARS) },
        "questions": {
            "timing": {
                "type": "choice",
                "instructions": "`new_message` 是用户在编码助手执行 `active_task` 期间发来的新消息。这条消息与当前正在执行的任务是什么时间关系？",
                "criteria": {
                    "steer": "现在就改变正在执行的任务的要求、范围或做法，任务继续",
                    "queue": "与当前任务无关或明确要求当前任务完成后再处理的新请求",
                    "interrupt": "明确要求立刻停止或放弃当前正在执行的任务",
                    "unclear": "无法判断与当前任务的关系，或同时包含多种意图"
                }
            },
            "explicit_stop": {
                "type": "noul",
                "instructions": "`new_message` 是否明确要求立刻停止当前正在执行的任务？",
                "criteria": {
                    "true": "明确的立即停止、取消或不要继续当前任务",
                    "false": "没有明确要求停止，包括普通纠正、补充要求或后续任务"
                }
            }
        }
    });
    let started = Instant::now();
    let ms = |s: Instant| s.elapsed().as_millis() as u64;
    // native-tls uses the OS certificate store, so TLS-inspecting antivirus roots are trusted too.
    let Ok(tls) = native_tls::TlsConnector::new() else { return json!({ "error": "TlsError", "ms": ms(started) }) };
    let agent = ureq::AgentBuilder::new().tls_connector(Arc::new(tls)).timeout(Duration::from_secs_f64(number("TIMEOUT_S", 2.0).max(0.1))).build();
    let resp = agent.post(&option("TYPESAFE_API_URL", DEFAULT_URL)).set("Authorization", &format!("Bearer {key}")).set("Content-Type", "application/json").send_string(&body.to_string());
    let text = match resp {
        Ok(r) => r.into_string(),
        Err(ureq::Error::Status(code, _)) => return json!({ "error": format!("http_{code}"), "ms": ms(started) }),
        Err(e) => return json!({ "error": format!("{:?}", e.kind()), "ms": ms(started) }),
    };
    let Ok(parsed) = text.map_err(|_| ()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|_| ())) else {
        return json!({ "error": "JSONDecodeError", "ms": ms(started) });
    };
    let answers = parsed.get("answers").cloned().unwrap_or(Value::Null);
    let timing = answers.get("timing").cloned().unwrap_or(Value::Null);
    json!({
        "choice": timing.get("choice").cloned().unwrap_or(Value::Null),
        "probabilities": timing.get("probabilities").filter(|p| p.is_object()).cloned().unwrap_or(json!({})),
        "explicit_stop": answers.get("explicit_stop").and_then(|e| e.get("noul")).and_then(Value::as_f64).unwrap_or(0.0),
        "ms": ms(started),
    })
}

/// Only confident queue / interrupt change behaviour; everything else stays native.
fn decide(jev: &Value) -> &'static str {
    let p = |k: &str| jev.get("probabilities").and_then(|p| p.get(k)).and_then(Value::as_f64).unwrap_or(0.0);
    let stop = jev.get("explicit_stop").and_then(Value::as_f64).unwrap_or(0.0);
    let t = number("INTERRUPT_THRESHOLD", 0.9);
    if p("interrupt") >= t && stop >= t {
        return "interrupt";
    }
    if p("queue") >= number("QUEUE_THRESHOLD", 0.9) && stop < 0.5 {
        return "queue";
    }
    "steer"
}

// ---------------------------------------------------------------- hook handlers

type State = Map<String, Value>;
type Record = Map<String, Value>;

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) | None => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(_) => true,
    }
}

fn on_prompt(payload: &Value, session: &Session, state: &mut State, record: &mut Record, jev: impl Fn(&str, &str) -> Value) -> Option<Value> {
    let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
    let prompt_id = payload.get("prompt_id").cloned().filter(|v| truthy(Some(v)));
    // A mid-turn message carries the running turn's prompt_id; a new turn gets a fresh one.
    // This stays correct after Esc, which fires no hook at all.
    let mid_turn = match &prompt_id {
        Some(id) => state.get("prompt_id") == Some(id) && truthy(state.get("active")),
        None => truthy(state.get("active")),
    };
    record.insert("mid_turn".into(), json!(mid_turn));

    if !mid_turn {
        let leftover = session.drain();
        state.clear();
        state.insert("active".into(), json!(true));
        state.insert("prompt_id".into(), prompt_id.unwrap_or(Value::Null));
        state.insert("task".into(), json!(cut(&prompt, MAX_TASK_CHARS)));
        state.insert("stop".into(), json!(false));
        if !leftover.is_empty() {
            record.insert("dropped".into(), json!(leftover.len()));
            let list: Vec<String> = leftover.iter().map(|p| format!("- {}", cut(p, 200))).collect();
            return Some(json!({ "systemMessage": format!("The last turn did not end normally; {} queued message(s) were not sent (dropped, resend if needed):\n{}", leftover.len(), list.join("\n")) }));
        }
        return None;
    }

    let task = state.get("task").and_then(Value::as_str).unwrap_or("").to_string();
    let answer = jev(&prompt, &task);
    let action = if answer.get("error").is_some() { "steer" } else { decide(&answer) };
    let prob = answer.get("probabilities").and_then(|p| p.get(action)).and_then(Value::as_f64).unwrap_or(0.0);
    record.insert("jev".into(), answer);
    record.insert("action".into(), json!(action));
    if option("MODE", "shadow") != "active" || action == "steer" {
        return None;
    }
    let label = format!("{action} {prob:.2}");
    if action == "queue" {
        let _ = session.push(&prompt);
        return Some(json!({ "decision": "block", "reason": format!("Queued (Jev {label}); it will be sent when this turn ends: {}", cut(&prompt, 120)) }));
    }
    state.insert("stop".into(), json!(true));
    Some(json!({ "decision": "block", "reason": format!("Stopping the current task (Jev {label}) at the next tool boundary.") }))
}

fn on_tool(state: &mut State, session: &Session, record: &mut Record) -> Option<Value> {
    if !truthy(state.get("stop")) {
        return None;
    }
    state.insert("active".into(), json!(false));
    state.insert("stop".into(), json!(false));
    record.insert("action".into(), json!("hard_stop"));
    let mut reason = String::from("Jev: stopped the current task as you asked.");
    let dropped = session.drain();
    if !dropped.is_empty() {
        let list: Vec<String> = dropped.iter().map(|p| cut(p, 120)).collect();
        reason += &format!(" {} queued message(s) were not run: {}", dropped.len(), list.join("; "));
    }
    Some(json!({ "continue": false, "stopReason": reason }))
}

fn on_stop(state: &mut State, session: &Session, record: &mut Record) -> Option<Value> {
    let queued = if option("MODE", "shadow") == "active" { session.pop() } else { None };
    let Some(queued) = queued.filter(|q| !q.is_empty()) else {
        state.insert("active".into(), json!(false));
        state.insert("stop".into(), json!(false));
        return None;
    };
    record.insert("action".into(), json!("dequeue"));
    state.insert("task".into(), json!(cut(&queued, MAX_TASK_CHARS)));
    state.insert("stop".into(), json!(false));
    Some(json!({ "hookSpecificOutput": {
        "hookEventName": "Stop",
        "additionalContext": format!("The user sent this message while the previous task was running and it was queued. Handle it now:\n{queued}"),
    }}))
}

fn run() -> Option<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    let payload: Value = serde_json::from_str(if input.trim().is_empty() { "{}" } else { &input }).ok()?;
    let event = payload.get("hook_event_name").and_then(Value::as_str).unwrap_or("");
    let root = data_dir();
    let sid = match payload.get("session_id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    let session = Session::open(&root, &sid).ok()?;
    let mut state = session.load();
    let mut record = Record::new();
    record.insert("ts".into(), json!((now() * 1000.0).round() / 1000.0));
    record.insert("event".into(), json!(event));
    record.insert("session".into(), payload.get("session_id").cloned().unwrap_or(Value::Null));
    record.insert("mode".into(), json!(option("MODE", "shadow")));

    let out = match event {
        "UserPromptSubmit" => {
            let out = on_prompt(&payload, &session, &mut state, &mut record, ask_jev);
            if option("LOG_PROMPTS", "true").to_lowercase() != "false" {
                record.insert("prompt".into(), json!(cut(payload.get("prompt").and_then(Value::as_str).unwrap_or(""), 300)));
            }
            out
        }
        "PreToolUse" | "PostToolUse" => on_tool(&mut state, &session, &mut record),
        "Stop" => on_stop(&mut state, &session, &mut record),
        "StopFailure" => {
            state.insert("active".into(), json!(false));
            state.insert("stop".into(), json!(false));
            None
        }
        _ => None,
    };
    let _ = session.save(&state);

    if event == "UserPromptSubmit" || record.contains_key("action") {
        if let Some(o) = &out {
            record.insert("output".into(), o.clone());
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(root.join("log.jsonl")) {
            let _ = writeln!(f, "{}", Value::Object(record));
        }
    }
    if let Some(o) = out {
        print!("{o}");
    }
    Some(())
}

fn main() {
    // Never break the host session: every failure exits 0 with no output.
    let _ = run();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> (PathBuf, Session) {
        let root = std::env::temp_dir().join(format!("jev-router-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sessions")).unwrap();
        let s = Session::open(&root, "s/1").unwrap();
        (root, s)
    }

    #[test]
    fn decide_needs_confident_answers() {
        assert_eq!(decide(&json!({ "probabilities": { "queue": 0.95 }, "explicit_stop": 0.1 })), "queue");
        assert_eq!(decide(&json!({ "probabilities": { "queue": 0.95 }, "explicit_stop": 0.6 })), "steer");
        assert_eq!(decide(&json!({ "probabilities": { "interrupt": 0.95 }, "explicit_stop": 0.95 })), "interrupt");
        assert_eq!(decide(&json!({ "probabilities": { "interrupt": 0.95 }, "explicit_stop": 0.5 })), "steer");
        assert_eq!(decide(&json!({ "probabilities": { "steer": 0.99 } })), "steer");
        assert_eq!(decide(&json!({})), "steer");
    }

    #[test]
    fn queue_is_fifo_and_session_ids_are_sanitised() {
        let (root, s) = session("fifo");
        assert!(s.state_path.ends_with("s_1.json"));
        s.push("first").unwrap();
        s.push("second").unwrap();
        assert_eq!(s.pop().as_deref(), Some("first"));
        assert_eq!(s.drain(), vec!["second".to_string()]);
        assert_eq!(s.pop(), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn idle_prompt_starts_a_turn_without_asking_jev() {
        let (root, s) = session("idle");
        let mut state = State::new();
        let mut rec = Record::new();
        let out = on_prompt(&json!({ "prompt": "fix the bug", "prompt_id": "p1" }), &s, &mut state, &mut rec, |_, _| panic!("Jev must not be asked"));
        assert_eq!(out, None);
        assert_eq!(state.get("active"), Some(&json!(true)));
        assert_eq!(state.get("task"), Some(&json!("fix the bug")));
        assert_eq!(rec.get("mid_turn"), Some(&json!(false)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mid_turn_prompt_is_classified_and_shadow_mode_changes_nothing() {
        let (root, s) = session("shadow");
        let mut state: State = serde_json::from_value(json!({ "active": true, "prompt_id": "p1", "task": "fix the bug", "stop": false })).unwrap();
        let mut rec = Record::new();
        std::env::remove_var("JEV_ROUTER_MODE");
        let out = on_prompt(&json!({ "prompt": "stop now", "prompt_id": "p1" }), &s, &mut state, &mut rec, |m, t| {
            assert_eq!((m, t), ("stop now", "fix the bug"));
            json!({ "probabilities": { "interrupt": 0.97 }, "explicit_stop": 0.98 })
        });
        assert_eq!(out, None); // shadow
        assert_eq!(rec.get("action"), Some(&json!("interrupt")));
        assert_eq!(state.get("stop"), Some(&json!(false)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stop_flag_ends_the_turn_at_the_next_tool_and_reports_dropped_queue() {
        let (root, s) = session("stop");
        s.push("later thing").unwrap();
        let mut state: State = serde_json::from_value(json!({ "active": true, "stop": true })).unwrap();
        let mut rec = Record::new();
        let out = on_tool(&mut state, &s, &mut rec).unwrap();
        assert_eq!(out["continue"], json!(false));
        assert!(out["stopReason"].as_str().unwrap().contains("1 queued message(s) were not run: later thing"));
        assert_eq!(state.get("active"), Some(&json!(false)));
        assert_eq!(on_tool(&mut state, &s, &mut rec), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cut_counts_characters_not_bytes() {
        assert_eq!(cut("已排队消息", 3), "已排队");
    }
}
