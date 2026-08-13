//! resmon-mcp — MCP stdio server exposing the running Resource Monitor app
//! to Claude Code and other MCP clients. Translates MCP tool calls into
//! plain-text requests over the app's named pipe.
//!
//! Register with:  claude mcp add resmon "C:\Program Files\Resource Monitor\resmon-mcp.exe"

#[cfg(not(windows))]
fn main() {
    eprintln!("resmon-mcp targets Windows");
}

#[cfg(windows)]
fn main() {
    windows::run();
}

#[cfg(windows)]
mod windows {
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Write};

    const PIPE: &str = r"\\.\pipe\resmon-mcp";

    /// Identity of this AI session, as reported to the app with every agent
    /// update. One `resmon-mcp.exe` is spawned per client session, so the
    /// process itself is the session — the model is never asked to invent a
    /// key, because a model cannot be relied on to keep one stable.
    struct Session {
        key: String,
        label: std::sync::Mutex<String>,
    }

    fn session() -> &'static Session {
        static S: std::sync::OnceLock<Session> = std::sync::OnceLock::new();
        S.get_or_init(|| {
            // Process id alone would collide after a PID is recycled, which a
            // tray app running for weeks will outlive. Start time separates
            // two processes that happen to share one.
            let started = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            Session {
                key: format!("{}-{}", std::process::id(), started),
                label: std::sync::Mutex::new(String::from("AI assistant")),
            }
        })
    }

    /// Name this session by its client and the folder the client launched us
    /// in — for Claude Code that is the project directory, which is what the
    /// user actually recognises. `clientInfo` is standard MCP, so any client
    /// gets a section of its own without being special-cased.
    fn set_session_label(client: &Value) {
        let name = client["name"].as_str().unwrap_or("").trim().to_string();
        let dir = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        let label = match (name.is_empty(), dir.is_empty()) {
            (false, false) => format!("{} · {}", name, dir),
            (false, true) => name,
            (true, false) => dir,
            (true, true) => "AI assistant".to_string(),
        };
        // Tabs delimit the pipe command, so they can never appear in a label.
        *session().label.lock().unwrap() = label.replace('\t', " ");
    }

    fn pipe_request_once(cmd: &str) -> Result<Value, String> {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(PIPE)
            .map_err(|_| {
                "Resource Monitor is not running. Start the app first.".to_string()
            })?;
        f.write_all(cmd.as_bytes()).map_err(|e| e.to_string())?;
        f.flush().ok();
        // Read exactly one newline-terminated response line; avoids waiting
        // for an EOF the server delivers abruptly (Windows error 233).
        let mut line = String::new();
        BufReader::new(&mut f)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        serde_json::from_str(line.trim()).map_err(|e| format!("bad response from app: {}", e))
    }

    /// One retry covers the brief window while the app recycles its pipe
    /// instance between requests.
    fn pipe_request(cmd: &str) -> Result<Value, String> {
        for _ in 0..2 {
            match pipe_request_once(cmd) {
                Ok(v) => return Ok(v),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }
        pipe_request_once(cmd)
    }

    fn tools() -> Value {
        json!([
            {
                "name": "system_snapshot",
                "description": "Current system usage: CPU, RAM, GPU, disk, network, frame rate, sound level and drive space.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "top_processes",
                "description": "Top apps by one measure, grouped by app name. Measures: cpu (percent), ram (bytes), gpu (percent), disk (bytes/sec), net (bytes/sec; needs administrator).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "metric": {"type": "string", "enum": ["cpu", "ram", "gpu", "disk", "net"]},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10}
                    },
                    "required": ["metric"]
                }
            },
            {
                "name": "app_detail",
                "description": "Everything one app is using, by name (for example 'node.exe'): CPU, RAM, GPU, disk, network, frame rate and its process ids.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            },
            {
                "name": "history",
                "description": "Recent measurements (about 6 minutes), timestamped. Useful for spotting spikes and memory leaks.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "fps_status",
                "description": "Apps currently drawing to the screen and their frame rates (games, video, capture tools).",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "notify",
                "description": "Show the user a desktop notification (for example 'build finished'). Use this when long-running work completes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "message": {"type": "string"}
                    },
                    "required": ["message"]
                }
            },
            {
                "name": "notify_rules",
                "description": "The user's current notification preferences, set by them in the app. Call this if you did not receive them at connection time, or to re-check after a long session, then follow them.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "report_agents",
                "description": "Tell the app what you and your sub-agents are working on, so the user can see current activity. Send the FULL current list every time: it replaces the previous one, and anything you omit is treated as finished. Call it when work starts, changes or completes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agents": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string", "description": "Stable identifier for this agent across calls."},
                                    "title": {"type": "string", "description": "Short name, e.g. 'code review'."},
                                    "status": {"type": "string", "enum": ["running", "waiting", "done", "failed"]},
                                    "detail": {"type": "string", "description": "What it is doing right now, one short line."}
                                },
                                "required": ["title"]
                            }
                        }
                    },
                    "required": ["agents"]
                }
            }
        ])
    }

    /// Compact one-line JSON for the pipe. Framing no longer depends on it
    /// (a request is one pipe message), but compact keeps requests small.
    fn agents_payload(args: &Value) -> String {
        let empty = Vec::new();
        let list = args["agents"].as_array().unwrap_or(&empty);
        let items: Vec<Value> = list
            .iter()
            .map(|a| {
                // The schema says id is a string, but a model may send a
                // number; its text form is still a stable identity, where
                // collapsing it to "" would merge every such agent into one.
                let id = match &a["id"] {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    v => v.to_string(),
                };
                json!({
                    "id": id,
                    "title": a["title"].as_str().unwrap_or(""),
                    "status": a["status"].as_str().unwrap_or("running"),
                    "detail": a["detail"].as_str().unwrap_or(""),
                })
            })
            .collect();
        // serde_json::to_string never emits literal newlines inside strings.
        Value::Array(items).to_string()
    }

    /// The user's instructions, or an empty string when the app is not
    /// running. Never fails: initialize must succeed regardless so the
    /// server still starts and its tools stay usable.
    fn instructions() -> String {
        match pipe_request("rules") {
            Ok(v) => v["instructions"].as_str().unwrap_or("").to_string(),
            Err(_) => String::new(),
        }
    }

    fn call_tool(name: &str, args: &Value) -> Result<Value, String> {
        let cmd = match name {
            "system_snapshot" => "snapshot".to_string(),
            "top_processes" => {
                let metric = args["metric"].as_str().unwrap_or("cpu");
                let limit = args["limit"].as_u64().unwrap_or(10);
                format!("top {} {}", metric, limit)
            }
            "app_detail" => {
                let n = args["name"].as_str().ok_or("name is required")?;
                format!("app {}", n)
            }
            "history" => "history".to_string(),
            "fps_status" => "fps".to_string(),
            "notify_rules" => "rules".to_string(),
            "report_agents" => {
                let sess = session();
                let label = sess.label.lock().unwrap().clone();
                format!("agents {}\t{}\t{}", sess.key, label, agents_payload(args))
            }
            "notify" => {
                let title = args["title"].as_str().unwrap_or("Claude Code");
                let message = args["message"].as_str().ok_or("message is required")?;
                format!("notify {}\t{}", title.replace('\t', " "), message.replace('\t', " "))
            }
            _ => return Err(format!("unknown tool: {}", name)),
        };
        pipe_request(&cmd)
    }

    fn respond(id: Value, result: Value) {
        let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
        println!("{}", msg);
        std::io::stdout().flush().ok();
    }

    fn respond_err(id: Value, code: i64, message: &str) {
        let msg = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        println!("{}", msg);
        std::io::stdout().flush().ok();
    }

    pub fn run() {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(req): Result<Value, _> = serde_json::from_str(&line) else { continue };
            let method = req["method"].as_str().unwrap_or("");
            let id = req["id"].clone();
            match method {
                "initialize" => {
                    let ver = req["params"]["protocolVersion"]
                        .as_str()
                        .unwrap_or("2024-11-05");
                    set_session_label(&req["params"]["clientInfo"]);
                    let mut result = json!({
                        "protocolVersion": ver,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "resourcemonitor.app", "version": "1.0.0"}
                    });
                    // The user's notification preferences ride along here so
                    // the assistant has them without being asked. Omitted
                    // entirely when they have set none.
                    let notes = instructions();
                    if !notes.is_empty() {
                        result["instructions"] = Value::String(notes);
                    }
                    respond(id, result);
                }
                "tools/list" => respond(id, json!({"tools": tools()})),
                "tools/call" => {
                    let name = req["params"]["name"].as_str().unwrap_or("");
                    let args = &req["params"]["arguments"];
                    match call_tool(name, args) {
                        Ok(v) => respond(
                            id,
                            json!({"content": [{"type": "text", "text": v.to_string()}]}),
                        ),
                        Err(e) => respond(
                            id,
                            json!({"content": [{"type": "text", "text": e}], "isError": true}),
                        ),
                    }
                }
                "ping" => respond(id, json!({})),
                m if m.starts_with("notifications/") => {} // no response for notifications
                _ => {
                    if !id.is_null() {
                        respond_err(id, -32601, "method not found");
                    }
                }
            }
        }
    }
}
