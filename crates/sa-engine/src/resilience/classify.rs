//! Failure classification helpers for adapters (spec §9.2 C).
//!
//! Every helper returns an [`AdapterError`] whose `message` is a fixed,
//! operator-facing Chinese sentence chosen by class and whose `code` is a
//! short redacted token (`http:503`, `exit:1`, `net:econnreset`, ...). Raw
//! native text (stderr, provider error bodies) is only inspected, never
//! copied into the result, because it may contain credentials or prompts.
//!
//! Source priority (spec §9.2 C): structured native signals
//! ([`claude_api_retry`], [`codex_error`], [`from_http_status`]), then exit
//! status ([`from_exit`]), then the engine's own timers (the supervisor), and
//! last the text fallback ([`classify_text`], [`StderrTail`]).

use crate::adapter::{AdapterError, ErrorClass};
use serde_json::Value;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Upper bound for any provider-announced Retry-After we accept.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(6 * 3600);

/// Fixed operator-facing message for a class.
pub fn message_for(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Network => "上游网络连接中断",
        ErrorClass::RateLimited => "上游限流（请求过多），稍后重试",
        ErrorClass::Overloaded => "上游服务繁忙或内部错误",
        ErrorClass::Stalled => "执行端长时间无响应，已中断本轮",
        ErrorClass::ProcessCrashed => "执行端进程意外退出",
        ErrorClass::RouteUnavailable => "所选供应商或模型暂时不可用",
        ErrorClass::Auth => "上游认证失败，请检查该供应商的凭据",
        ErrorClass::Billing => "上游额度或账单受限",
        ErrorClass::Permission => "操作被权限策略拒绝",
        ErrorClass::Protocol => "上游请求或协议错误",
        ErrorClass::InputTooLarge => "输入超出模型上下文上限",
        ErrorClass::SessionMismatch => "原生会话不匹配",
        ErrorClass::Cancelled => "已取消",
        ErrorClass::Other => "执行端返回未分类错误",
    }
}

/// Whether the class can, in principle, succeed on a later attempt.
/// `RouteUnavailable` is conditional (see policy) and `SessionMismatch` may be
/// retried once with a fresh session.
pub fn is_retryable(class: ErrorClass) -> bool {
    matches!(
        class,
        ErrorClass::Network
            | ErrorClass::RateLimited
            | ErrorClass::Overloaded
            | ErrorClass::Stalled
            | ErrorClass::ProcessCrashed
            | ErrorClass::RouteUnavailable
            | ErrorClass::SessionMismatch
    )
}

/// Failures that describe the route (provider/model/endpoint) rather than the
/// job. These count toward the circuit breaker.
pub fn is_route_health(class: ErrorClass) -> bool {
    matches!(
        class,
        ErrorClass::Network
            | ErrorClass::RateLimited
            | ErrorClass::Overloaded
            | ErrorClass::Stalled
            | ErrorClass::ProcessCrashed
            | ErrorClass::RouteUnavailable
            | ErrorClass::Auth
            | ErrorClass::Billing
    )
}

/// Keep codes short and free of anything that could be a secret.
fn sanitize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-' | '/'))
        .take(48)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Build a classified error with the class message and a redacted code.
pub fn classified(class: ErrorClass, code: &str) -> AdapterError {
    let mut e = AdapterError::new(class, message_for(class));
    let code = sanitize_code(code);
    if !code.is_empty() {
        e.code = Some(code);
    }
    e
}

/// Class for an upstream HTTP status.
pub fn http_class(status: u16) -> ErrorClass {
    match status {
        401 | 403 | 407 => ErrorClass::Auth,
        402 => ErrorClass::Billing,
        404 => ErrorClass::RouteUnavailable,
        408 => ErrorClass::Network,
        413 => ErrorClass::InputTooLarge,
        429 => ErrorClass::RateLimited,
        400 | 405 | 406 | 409..=412 | 414..=428 | 431 => ErrorClass::Protocol,
        500..=599 => ErrorClass::Overloaded,
        _ => ErrorClass::Other,
    }
}

/// Error for an upstream HTTP status, keeping the status and Retry-After.
pub fn from_http_status(status: u16, retry_after: Option<Duration>) -> AdapterError {
    let mut e = classified(http_class(status), &format!("http:{status}"));
    e.http_status = Some(status);
    e.retry_after = retry_after.map(|d| d.min(MAX_RETRY_AFTER));
    e
}

/// Error for a harness process that ended without a terminal record.
/// `code` is `None` when the process was killed by a signal.
pub fn from_exit(code: Option<i32>) -> AdapterError {
    match code {
        Some(0) => classified(ErrorClass::ProcessCrashed, "exit:0-no-terminal"),
        Some(c) => classified(ErrorClass::ProcessCrashed, &format!("exit:{c}")),
        None => classified(ErrorClass::ProcessCrashed, "exit:signal"),
    }
}

/// Parse an HTTP `Retry-After` value: delta-seconds (fractions accepted) or
/// an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`). Dates in the past give
/// zero. The result is clamped to [`MAX_RETRY_AFTER`].
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<f64>() {
        if secs.is_finite() && secs >= 0.0 {
            return Some(Duration::from_secs_f64(secs.min(MAX_RETRY_AFTER.as_secs_f64())));
        }
        return None;
    }
    let at = parse_imf_fixdate(v)?;
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let delta = (at - now).max(0) as u64;
    Some(Duration::from_secs(delta).min(MAX_RETRY_AFTER))
}

/// `retry-after-ms` style header (milliseconds).
pub fn parse_retry_after_ms(value: &str) -> Option<Duration> {
    let ms = value.trim().parse::<f64>().ok()?;
    (ms.is_finite() && ms >= 0.0)
        .then(|| Duration::from_secs_f64((ms / 1000.0).min(MAX_RETRY_AFTER.as_secs_f64())))
}

/// IMF-fixdate to unix seconds.
fn parse_imf_fixdate(v: &str) -> Option<i64> {
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let rest = v.split_once(", ").map(|(_, r)| r).unwrap_or(v);
    let mut parts = rest.split_ascii_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let m: i64 = hms.next()?.parse().ok()?;
    let s: i64 = hms.next()?.parse().ok()?;
    if !matches!(parts.next(), Some("GMT") | Some("UTC")) || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + h * 3600 + m * 60 + s)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Claude Code `system/api_retry` frame (or its `error` string alone).
///
/// Known `error` values (Claude Code headless docs, 2026-11-03):
/// `authentication_failed`, `billing_error`, `rate_limit`, `invalid_request`,
/// `server_error`, `max_output_tokens`, `unknown`; the spec additionally
/// lists `overloaded`, `oauth_org_not_allowed`, `account_on_hold` and
/// `model_not_found`. `error_status: null` means no HTTP response.
/// `retry_delay_ms` becomes `retry_after`.
pub fn claude_api_retry(frame: &Value) -> AdapterError {
    let error = frame.get("error").and_then(Value::as_str).unwrap_or("");
    let status = frame
        .get("error_status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok());
    let mut e = claude_error_string(error, status);
    if let Some(ms) = frame.get("retry_delay_ms").and_then(Value::as_f64)
        && ms.is_finite()
        && ms >= 0.0
    {
        e.retry_after = Some(Duration::from_secs_f64((ms / 1000.0).min(MAX_RETRY_AFTER.as_secs_f64())));
    }
    e
}

/// Classify a Claude error string with an optional HTTP status.
pub fn claude_error_string(error: &str, status: Option<u16>) -> AdapterError {
    let class = match error {
        "rate_limit" | "rate_limit_error" => Some(ErrorClass::RateLimited),
        "overloaded" | "overloaded_error" | "server_error" | "api_error" => Some(ErrorClass::Overloaded),
        "authentication_failed" | "authentication_error" | "oauth_org_not_allowed" | "permission_error" => {
            Some(ErrorClass::Auth)
        }
        "billing_error" | "account_on_hold" => Some(ErrorClass::Billing),
        "model_not_found" | "not_found_error" => Some(ErrorClass::RouteUnavailable),
        "invalid_request" | "invalid_request_error" => Some(ErrorClass::Protocol),
        "request_too_large" | "max_output_tokens" => Some(ErrorClass::InputTooLarge),
        _ => None,
    };
    let code = if error.is_empty() {
        "claude:unknown".to_owned()
    } else {
        format!("claude:{error}")
    };
    match (class, status) {
        (Some(c), _) => {
            let mut e = classified(c, &code);
            e.http_status = status;
            e
        }
        (None, Some(s)) => {
            let mut e = classified(http_class(s), &format!("http:{s}"));
            e.http_status = Some(s);
            e
        }
        // `unknown` without a status: the request got no response at all.
        (None, None) => classified(ErrorClass::Network, &code),
    }
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Codex `turn.error` / `TurnError` object, a bare `codexErrorInfo` value, or
/// a JSON-RPC `error` object (`{code, message, data}`).
///
/// `codexErrorInfo` variant names follow the app-server v2 protocol as known
/// at writing (`contextWindowExceeded`, `usageLimitExceeded`,
/// `httpConnectionFailed{httpStatusCode}`, `responseStreamConnectionFailed`,
/// `responseStreamDisconnected`, `responseTooManyFailedAttempts`,
/// `internalServerError`, `unauthorized`, `badRequest`, `sandboxError`,
/// `other`). **Not verified against the installed
/// `codex app-server generate-json-schema`**; unknown names fall back to the
/// HTTP status and then to the message text.
pub fn codex_error(error: &Value) -> AdapterError {
    let info = error
        .get("codexErrorInfo")
        .or_else(|| error.get("data").and_then(|d| d.get("codexErrorInfo")))
        .or_else(|| error.get("data").filter(|d| d.is_string()))
        .unwrap_or(error);
    let (name, inner) = match info {
        Value::String(s) => (s.as_str(), None),
        Value::Object(m) if m.len() == 1 => {
            let (k, v) = m.iter().next().expect("one entry");
            (k.as_str(), Some(v))
        }
        _ => ("", None),
    };
    let status = inner
        .and_then(|v| v.get("httpStatusCode"))
        .or_else(|| error.get("httpStatusCode"))
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok());
    let code = format!("codex:{}", norm(name));
    let by_name = match norm(name).as_str() {
        "contextwindowexceeded" => Some(ErrorClass::InputTooLarge),
        "usagelimitexceeded" => Some(ErrorClass::Billing),
        "unauthorized" => Some(ErrorClass::Auth),
        "badrequest" => Some(ErrorClass::Protocol),
        "sandboxerror" => Some(ErrorClass::Permission),
        "internalservererror" => Some(ErrorClass::Overloaded),
        "httpconnectionfailed" | "responsestreamconnectionfailed" | "responsestreamdisconnected" => {
            Some(status.map(http_class).unwrap_or(ErrorClass::Network))
        }
        "responsetoomanyfailedattempts" => Some(status.map(http_class).unwrap_or(ErrorClass::Overloaded)),
        _ => None,
    };
    let mut e = match (by_name, status) {
        (Some(c), _) => classified(c, &code),
        (None, Some(s)) => classified(http_class(s), &format!("http:{s}")),
        (None, None) => {
            let msg = error.get("message").and_then(Value::as_str).unwrap_or("");
            classify_text(msg)
        }
    };
    e.http_status = e.http_status.or(status);
    e
}

/// Regex-like fallback over free text (stderr tail, an error message). Returns
/// the class and a redacted code token, or `None` when nothing matched.
pub fn match_text(text: &str) -> Option<(ErrorClass, String)> {
    let t = text.to_ascii_lowercase();
    const NET: &[&str] = &[
        "econnreset",
        "etimedout",
        "econnrefused",
        "eai_again",
        "enotfound",
        "epipe",
        "econnaborted",
        "enetunreach",
        "ehostunreach",
        "socket hang up",
        "connection reset",
        "connection refused",
        "connection closed",
        "stream disconnected",
        "network error",
        "fetch failed",
        "timed out",
        "tls handshake",
    ];
    // Specific before generic: auth/billing/size words win over status digits.
    let words: &[(&[&str], ErrorClass, &str)] = &[
        (
            &[
                "invalid api key",
                "invalid x-api-key",
                "incorrect api key",
                "unauthorized",
                "authentication_failed",
                "authentication failed",
            ],
            ErrorClass::Auth,
            "auth",
        ),
        (
            &[
                "insufficient_quota",
                "billing",
                "credit balance",
                "quota exceeded",
                "usage limit",
                "payment required",
            ],
            ErrorClass::Billing,
            "billing",
        ),
        (
            &[
                "context length",
                "context_length_exceeded",
                "context window",
                "prompt is too long",
                "request too large",
                "payload too large",
            ],
            ErrorClass::InputTooLarge,
            "too_large",
        ),
        (
            &["model_not_found", "model not found", "does not exist", "no such model"],
            ErrorClass::RouteUnavailable,
            "route",
        ),
        (
            &["rate limit", "rate_limit", "too many requests"],
            ErrorClass::RateLimited,
            "rate_limit",
        ),
        (
            &[
                "overloaded",
                "service unavailable",
                "bad gateway",
                "gateway timeout",
                "internal server error",
                "server_error",
            ],
            ErrorClass::Overloaded,
            "overloaded",
        ),
    ];
    for (needles, class, code) in words {
        if needles.iter().any(|n| t.contains(n)) {
            return Some((*class, format!("text:{code}")));
        }
    }
    if let Some(status) = status_token(&t) {
        return Some((http_class(status), format!("http:{status}")));
    }
    if let Some(n) = NET.iter().find(|n| t.contains(**n)) {
        return Some((ErrorClass::Network, format!("net:{}", n.replace(' ', "_"))));
    }
    // Existing Chinese adapter messages (AC:295-309, E:254).
    if text.contains("当前不可用") || text.contains("暂时不可用") || text.contains("模型失效") {
        return Some((ErrorClass::RouteUnavailable, "text:route".into()));
    }
    if text.contains("断开了会话") || text.contains("执行失败") {
        return Some((ErrorClass::ProcessCrashed, "text:crash".into()));
    }
    None
}

/// A standalone 3-digit HTTP status mentioned next to `http`, `status`,
/// `code` or `error` (so `line 500` alone is not a match).
fn status_token(t: &str) -> Option<u16> {
    const KNOWN: &[u16] = &[401, 402, 403, 404, 408, 413, 429, 500, 502, 503, 504, 529];
    if !["http", "status", "code", "error", "api"].iter().any(|w| t.contains(w)) {
        return None;
    }
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 3
                && let Ok(n) = t[start..i].parse::<u16>()
                && KNOWN.contains(&n)
            {
                return Some(n);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Classify free text; unmatched text becomes `Other`. The text itself is
/// never copied into the result.
pub fn classify_text(text: &str) -> AdapterError {
    match match_text(text) {
        Some((class, code)) => {
            let mut e = classified(class, &code);
            if let Some(s) = code.strip_prefix("http:").and_then(|s| s.parse().ok()) {
                e.http_status = Some(s);
            }
            e
        }
        None => classified(ErrorClass::Other, "unclassified"),
    }
}

/// Bounded in-memory tail of a harness's stderr (spec §9.2 B). Only used for
/// classification; `Debug` never shows the content.
pub struct StderrTail {
    buf: VecDeque<u8>,
    cap: usize,
}

impl StderrTail {
    pub const DEFAULT_CAP: usize = 8 * 1024;

    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let bytes = &bytes[bytes.len().saturating_sub(self.cap)..];
        let over = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        self.buf.drain(..over);
        self.buf.extend(bytes);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Text-fallback classification of the tail.
    pub fn classify(&self) -> Option<(ErrorClass, String)> {
        let (a, b) = self.buf.as_slices();
        let mut v = Vec::with_capacity(a.len() + b.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        match_text(&String::from_utf8_lossy(&v))
    }

    /// Error for a process exit, refined by the tail when it is conclusive.
    pub fn exit_error(&self, code: Option<i32>) -> AdapterError {
        let mut e = from_exit(code);
        if let Some((class, tail_code)) = self.classify() {
            let exit = e.code.take().unwrap_or_default();
            e = classified(class, &format!("{exit}/{tail_code}"));
        }
        e
    }
}

impl Default for StderrTail {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAP)
    }
}

impl std::fmt::Debug for StderrTail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StderrTail({} bytes, redacted)", self.buf.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_mapping() {
        assert_eq!(http_class(429), ErrorClass::RateLimited);
        assert_eq!(http_class(503), ErrorClass::Overloaded);
        assert_eq!(http_class(529), ErrorClass::Overloaded);
        assert_eq!(http_class(401), ErrorClass::Auth);
        assert_eq!(http_class(402), ErrorClass::Billing);
        assert_eq!(http_class(404), ErrorClass::RouteUnavailable);
        assert_eq!(http_class(413), ErrorClass::InputTooLarge);
        let e = from_http_status(503, Some(Duration::from_secs(9)));
        assert_eq!(e.code.as_deref(), Some("http:503"));
        assert_eq!(e.retry_after, Some(Duration::from_secs(9)));
    }

    #[test]
    fn retry_after_forms() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777); // Sun, 06 Nov 1994 08:49:37 GMT
        assert_eq!(parse_retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("1.5", now), Some(Duration::from_millis(1500)));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:50:07 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-3", now), None);
        assert_eq!(parse_retry_after("99999999", now), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after_ms("2500"), Some(Duration::from_millis(2500)));
    }

    #[test]
    fn claude_frames() {
        let e = claude_api_retry(&json!({"type":"system","subtype":"api_retry","error":"rate_limit","error_status":429,"retry_delay_ms":4000}));
        assert_eq!(e.class, ErrorClass::RateLimited);
        assert_eq!(e.retry_after, Some(Duration::from_secs(4)));
        assert_eq!(e.http_status, Some(429));
        assert_eq!(claude_error_string("authentication_failed", Some(401)).class, ErrorClass::Auth);
        assert_eq!(claude_error_string("billing_error", None).class, ErrorClass::Billing);
        assert_eq!(claude_error_string("server_error", Some(500)).class, ErrorClass::Overloaded);
        assert_eq!(claude_error_string("unknown", None).class, ErrorClass::Network);
        assert_eq!(claude_error_string("unknown", Some(503)).class, ErrorClass::Overloaded);
        assert_eq!(claude_error_string("model_not_found", Some(404)).class, ErrorClass::RouteUnavailable);
    }

    #[test]
    fn codex_errors() {
        let e = codex_error(&json!({"message":"x","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":503}}}));
        assert_eq!(e.class, ErrorClass::Overloaded);
        assert_eq!(e.http_status, Some(503));
        let e = codex_error(&json!({"message":"x","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":null}}}));
        assert_eq!(e.class, ErrorClass::Network);
        assert_eq!(codex_error(&json!({"codexErrorInfo":"contextWindowExceeded"})).class, ErrorClass::InputTooLarge);
        assert_eq!(codex_error(&json!("usageLimitExceeded")).class, ErrorClass::Billing);
        assert_eq!(codex_error(&json!({"codexErrorInfo":"unauthorized"})).class, ErrorClass::Auth);
        let rpc = codex_error(&json!({"code":-32000,"message":"stream error: HTTP 502 Bad Gateway"}));
        assert_eq!(rpc.class, ErrorClass::Overloaded);
        assert!(!rpc.message.contains("Bad Gateway"));
    }

    #[test]
    fn text_fallback_and_redaction() {
        let secret = "sk-live-THIS-IS-SECRET";
        for (text, class) in [
            ("Error: read ECONNRESET", ErrorClass::Network),
            ("connect ETIMEDOUT 1.2.3.4:443", ErrorClass::Network),
            ("getaddrinfo EAI_AGAIN api.openai.com", ErrorClass::Network),
            ("socket hang up", ErrorClass::Network),
            ("API Error: 529 {\"type\":\"overloaded_error\"}", ErrorClass::Overloaded),
            ("HTTP 429 Too Many Requests", ErrorClass::RateLimited),
            ("unexpected status 503", ErrorClass::Overloaded),
            ("Codex 原生请求失败（HTTP 504）", ErrorClass::Overloaded),
            ("所选供应商、模型或执行端当前不可用", ErrorClass::RouteUnavailable),
        ] {
            let e = classify_text(&format!("{text} key={secret}"));
            assert_eq!(e.class, class, "{text}");
            assert!(!e.message.contains(secret) && !e.code.clone().unwrap_or_default().contains("sk-"));
        }
        assert_eq!(classify_text("line 500 of file").class, ErrorClass::Other);
        assert_eq!(classify_text(&format!("invalid api key {secret}")).class, ErrorClass::Auth);
    }

    #[test]
    fn stderr_tail_is_bounded_and_redacted() {
        let mut t = StderrTail::new(16);
        t.push(b"0123456789");
        t.push(b"abcdefghij ECONNRESET");
        assert_eq!(t.len(), 16);
        assert_eq!(t.classify().map(|c| c.0), Some(ErrorClass::Network));
        assert!(!format!("{t:?}").contains("ECONN"));
        let e = t.exit_error(Some(1));
        assert_eq!(e.class, ErrorClass::Network);
        assert_eq!(e.code.as_deref(), Some("exit:1/net:econnreset"));
        assert_eq!(StderrTail::default().exit_error(None).class, ErrorClass::ProcessCrashed);
    }
}
