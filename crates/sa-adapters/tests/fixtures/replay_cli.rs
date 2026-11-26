//! Stdio peer used by the replay integration tests. It never runs tools.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    net::TcpStream,
    process::ExitCode,
};

fn subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|actual| subset(value, actual))),
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() == actual.len() && expected.iter().zip(actual).all(|(e, a)| subset(e, a))
        }
        _ => expected == actual,
    }
}

fn read_frame(input: &mut impl BufRead) -> Result<Value> {
    let mut line = String::new();
    ensure!(input.read_line(&mut line)? != 0, "received EOF");
    serde_json::from_str(&line).with_context(|| format!("received invalid JSON {line:?}"))
}

fn send(output: &mut impl Write, frame: &Value) -> Result<()> {
    writeln!(output, "{frame}")?;
    output.flush()?;
    Ok(())
}

// Adapter::run also performs local CLI discovery. These replies serve only those
// invocations and never run against the turn transcript.
fn probe(args: &[String], input: &mut impl BufRead, output: &mut impl Write) -> Result<bool> {
    if args == ["debug", "models"] {
        send(output, &json!({"models":[{"slug":"codex-fixture"}]}))?;
    } else if args.last().is_some_and(|arg| arg == "models")
        && args.first().is_some_and(|arg| arg == "--settings")
    {
        writeln!(output, "gemini-fixture\tFixture Gemini")?;
        output.flush()?;
    } else if args.ends_with(&["auth".into(), "status".into()]) {
        send(output, &json!({"loggedIn":true,"apiProvider":"firstParty"}))?;
    } else if args.iter().any(|arg| arg == "--no-session-persistence") {
        let frame = read_frame(input)?;
        let expected = json!({"type":"control_request","request_id":"catalog","request":{"subtype":"initialize"}});
        ensure!(
            subset(&expected, &frame),
            "expected {expected}, received {frame}"
        );
        send(
            output,
            &json!({"type":"control_response","response":{"request_id":"catalog","subtype":"success",
                "response":{"models":[{"value":"claude-fixture","displayName":"Fixture Claude"}]}}}),
        )?;
    } else {
        return Ok(false);
    }
    Ok(true)
}

fn match_frame(
    input: &mut impl BufRead,
    expected: &Value,
    own_id: Option<&Value>,
) -> Result<Value> {
    let actual = read_frame(input)?;
    ensure!(
        subset(expected, &actual) && own_id.is_none_or(|id| actual.get("id") == Some(id)),
        "received {actual}"
    );
    Ok(actual)
}

fn replay(
    script: &str,
    rpc: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
    receipt: &mut impl Write,
) -> Result<u8> {
    let mut requests = Vec::new();
    let mut peer_id = None;
    let mut next_peer_id = 9000;
    for (index, line) in script.lines().enumerate() {
        let line_number = index + 1;
        let step = serde_json::from_str::<Value>(line)
            .with_context(|| format!("invalid transcript JSON on line {line_number}"))?;
        let fields = step.as_object().context("step must be an object")?;
        let to = fields.get("to");
        let mut actions = fields.iter().filter(|(key, _)| *key != "to");
        let (action, value) = actions.next().context("step has no action")?;
        ensure!(
            actions.next().is_none(),
            "line {line_number} must have one action"
        );
        ensure!(
            to.is_none() || matches!(action.as_str(), "respond" | "respond_error"),
            "line {line_number}: to is only valid for a response"
        );
        let mut execute = || -> Result<Option<u8>> {
            match action.as_str() {
                "expect" => {
                    let mut expected = value.clone();
                    let notification = expected
                        .as_object_mut()
                        .and_then(|fields| fields.shift_remove("notification"))
                        .is_some_and(|flag| flag == true);
                    let actual =
                        match_frame(input, expected.get("record").unwrap_or(&expected), None)?;
                    ensure!(
                        !notification || actual.get("id").is_none(),
                        "expected notification without id, received {actual}"
                    );
                    if let Some(id) = actual.get("id").filter(|id| !id.is_null()) {
                        let method = actual["method"].as_str().map(str::to_owned);
                        requests.push((method, id.clone()));
                    }
                    writeln!(receipt, "{line_number}")?;
                }
                "respond" | "respond_error" => {
                    let method = to
                        .map(|value| value.as_str().context("to must name a method"))
                        .transpose()?;
                    let index = requests
                        .iter()
                        .rposition(|(captured, _)| {
                            method.is_none() || captured.as_deref() == method
                        })
                        .with_context(|| match method {
                            Some(method) => format!("no pending request for {method}"),
                            None => "no preceding request id".into(),
                        })?;
                    let (_, id) = requests.remove(index);
                    let key = if action == "respond" {
                        "result"
                    } else {
                        "error"
                    };
                    let mut frame = json!({"id":id});
                    frame[key] = value.clone();
                    if rpc {
                        frame["jsonrpc"] = json!("2.0");
                    }
                    send(output, &frame)?;
                }
                "emit" => send(output, value)?,
                "request" => {
                    ensure!(
                        peer_id.is_none(),
                        "previous peer request has no expected response"
                    );
                    let mut frame = value.clone();
                    ensure!(
                        frame["method"].is_string(),
                        "request method must be a string"
                    );
                    let id = json!(next_peer_id);
                    next_peer_id += 1;
                    frame["id"] = id.clone();
                    peer_id = Some(id);
                    if rpc {
                        frame["jsonrpc"] = json!("2.0");
                    }
                    send(output, &frame)?;
                }
                "expect_response" => {
                    let id = peer_id.take().context("no preceding peer request")?;
                    match_frame(input, value, Some(&id))?;
                    writeln!(receipt, "{line_number}")?;
                }
                "emit_raw" => {
                    output.write_all(value.as_str().context("raw text required")?.as_bytes())?;
                    output.flush()?;
                }
                "stderr" => eprintln!("{}", value.as_str().context("stderr text required")?),
                "wait_for_close" => {
                    ensure!(value == true, "wait_for_close requires true");
                    let mut extra = String::new();
                    ensure!(
                        input.read_line(&mut extra)? == 0,
                        "received extra output {extra:?}"
                    );
                }
                "exit" => {
                    let code = value.as_u64().and_then(|code| u8::try_from(code).ok());
                    return Ok(Some(code.context("invalid exit code")?));
                }
                _ => bail!("unknown action {action}"),
            }
            Ok(None)
        };
        if let Some(code) =
            execute().with_context(|| format!("line {line_number}: expected {step}"))?
        {
            return Ok(code);
        }
    }
    Ok(0)
}

fn run() -> Result<u8> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    if probe(&args, &mut input, &mut output)? {
        return Ok(0);
    }
    let rpc = args.first().is_some_and(|arg| arg == "acp");
    if !rpc && !args.iter().any(|arg| arg == "app-server") {
        for flag in ["--input-format", "--output-format"] {
            ensure!(
                args.windows(2)
                    .any(|pair| pair[0] == flag && pair[1] == "stream-json"),
                "expected {flag} stream-json, received arguments {args:?}"
            );
        }
        ensure!(
            args.iter().any(|arg| arg == "--include-partial-messages"),
            "expected --include-partial-messages, received arguments {args:?}"
        );
    }
    let script =
        fs::read_to_string(env::var_os("SA_REPLAY_SCRIPT").context("missing script path")?)?;
    let mut receipt = OpenOptions::new()
        .create(true)
        .append(true)
        .open(env::var_os("SA_REPLAY_RECEIPT").context("missing receipt path")?)?;
    // For cancellation, keep this socket open for the lifetime of the peer.
    let _lifetime = env::var("SA_REPLAY_LIFETIME")
        .ok()
        .map(TcpStream::connect)
        .transpose()?;
    replay(&script, rpc, &mut input, &mut output, &mut receipt)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let diagnostic = format!("replay mismatch: {error:#}");
            if let Some(path) = env::var_os("SA_REPLAY_RECEIPT")
                && let Ok(mut receipt) = OpenOptions::new().create(true).append(true).open(path)
            {
                let _ = writeln!(receipt, "{diagnostic}");
            }
            eprintln!("{diagnostic}");
            ExitCode::from(97)
        }
    }
}
