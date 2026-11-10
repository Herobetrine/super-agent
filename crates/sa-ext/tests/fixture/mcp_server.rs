//! Disposable MCP server for the sa-ext integration tests (a Rust stand-in
//! for `tests-node/fixtures/challenge-mcp.cjs`). Hand-written JSON-RPC, std
//! only, so it exercises the rmcp client against independent wire code.
//!
//! `sa-ext-fixture-mcp <secret-file>`        newline-delimited stdio
//! `sa-ext-fixture-mcp <secret-file> http`   streamable HTTP (stateless JSON
//!                                           responses) on 127.0.0.1; prints
//!                                           `{"port": N}` first
//!
//! Writes `<secret-file>.pid` on start. With `FIXTURE_TOKEN` set, HTTP
//! requests must carry `Authorization: Bearer $FIXTURE_TOKEN`.
//!
//! Tools (two `tools/list` pages): `challenge` appends to
//! `<secret-file>.calls` and returns the secret; `env` returns the value of
//! the variable `arguments.name` (or `<unset>`); `fail` returns `isError`;
//! `big` returns ~600 KiB; `sleep` appends to `<secret-file>.sleeps`, then
//! blocks for two minutes.
//! Optional `<secret-file>.catalog` JSON `{count, descriptionRepeats}` replaces
//! the tool directory with generated tools, paged in groups of 100. Each
//! description repeat is one supplementary Unicode character (two UTF-16 units).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

fn secret() -> String {
    std::env::args().nth(1).expect("secret file argument")
}

fn append(suffix: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{}{suffix}", secret()))
        .unwrap();
    f.write_all(b"1\n").unwrap();
}

fn text(s: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": s.into()}]})
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": true}
    })
}

/// The JSON-RPC response for one message; `None` for notifications.
fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => {
            append(".initializes");
            std::fs::write(
                format!("{}.initialize-cwd", secret()),
                std::env::current_dir()
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes(),
            )
            .unwrap();
            let reject = format!("{}.reject-init", secret());
            if std::fs::remove_file(reject).is_ok() {
                Err(json!({"code": -32603, "message": "fixture refused initialization"}))
            } else {
                Ok(json!({
                    "protocolVersion": params["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "studio-proof", "version": "1"}
                }))
            }
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(
            if let Ok(raw) = std::fs::read_to_string(format!("{}.catalog", secret())) {
                let catalog: Value = serde_json::from_str(&raw).unwrap();
                let count = catalog["count"].as_u64().unwrap() as usize;
                let repeats = catalog["descriptionRepeats"].as_u64().unwrap() as usize;
                let start = params["cursor"]
                    .as_str()
                    .unwrap_or("0")
                    .parse::<usize>()
                    .unwrap();
                let end = (start + 100).min(count);
                let description = "🧪".repeat(repeats);
                let tools: Vec<Value> = (start..end)
                    .map(|i| tool(&format!("generated_{i}"), &description))
                    .collect();
                let mut page = json!({"tools": tools});
                if end < count {
                    page["nextCursor"] = json!(end.to_string());
                }
                page
            } else {
                match params["cursor"].as_str() {
                    None => json!({
                        "tools": [tool("challenge", "Read the one-time verification code from the test service.")],
                        "nextCursor": "page-2"
                    }),
                    Some(_) => {
                        json!({"tools": [tool("env", ""), tool("fail", "Always fails."), tool("big", "Large"), tool("sleep", "Blocks")]})
                    }
                }
            },
        ),
        "tools/call" => match params["name"].as_str().unwrap_or_default() {
            "challenge" => {
                append(".calls");
                match params["arguments"]["fixture"].as_str() {
                    Some("disconnect") => std::process::exit(0),
                    Some("identity") => Ok(text(
                        json!({
                            "pid": std::process::id(), "cwd": std::env::current_dir().unwrap()
                        })
                        .to_string(),
                    )),
                    _ => Ok(text(std::fs::read_to_string(secret()).unwrap())),
                }
            }
            "env" => {
                let name = params["arguments"]["name"].as_str().unwrap_or_default();
                Ok(text(
                    std::env::var(name).unwrap_or_else(|_| "<unset>".into()),
                ))
            }
            "fail" => Ok(json!({"content": [{"type": "text", "text": "boom"}], "isError": true})),
            "big" => Ok(text("x".repeat(600 * 1024))),
            "sleep" => {
                append(".sleeps");
                std::thread::sleep(std::time::Duration::from_secs(120));
                Ok(text("woke"))
            }
            _ => Err(json!({"code": -32602, "message": "unknown tool"})),
        },
        _ => Err(json!({"code": -32601, "message": "method not found"})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

fn stdio() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(resp) = handle(&msg) {
            writeln!(out, "{resp}").unwrap();
            out.flush().unwrap();
        }
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: Option<&Value>) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let kind = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let head = format!(
        "HTTP/1.1 {status}\r\n{kind}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn serve(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut length = 0usize;
    let mut auth = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let (k, v) = line.split_once(':').unwrap_or((&line, ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = v.trim().parse().unwrap_or(0),
            "authorization" => auth = v.trim().to_string(),
            _ => {}
        }
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if path != "/mcp" {
        return respond(&mut stream, "404 Not Found", None);
    }
    if let Ok(token) = std::env::var("FIXTURE_TOKEN")
        && auth != format!("Bearer {token}")
    {
        return respond(&mut stream, "401 Unauthorized", None);
    }
    if method != "POST" {
        return respond(&mut stream, "405 Method Not Allowed", None);
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return respond(&mut stream, "400 Bad Request", None);
    };
    match handle(&msg) {
        Some(resp) => respond(&mut stream, "200 OK", Some(&resp)),
        None => respond(&mut stream, "202 Accepted", None),
    }
}

fn main() {
    std::fs::write(format!("{}.pid", secret()), std::process::id().to_string()).unwrap();
    if std::env::args().nth(2).as_deref() == Some("http") {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        println!("{}", json!({"port": listener.local_addr().unwrap().port()}));
        std::io::stdout().flush().unwrap();
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || serve(stream));
        }
    } else {
        stdio();
    }
}
