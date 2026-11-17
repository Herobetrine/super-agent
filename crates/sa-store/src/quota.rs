//! Native subscription windows shared by the scheduler and read-only views.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_AGE: f64 = 6.0 * 60.0 * 60.0;

/// One subscription usage window as last reported for a harness and provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Harness-native name, such as `five_hour`, `primary`, or `unknown`.
    pub kind: String,
    /// Fraction used; absent when the harness did not report it.
    pub used: Option<f64>,
    /// Unix seconds of the next reset; absent when not reported.
    pub resets_at: Option<f64>,
    /// Requests are being refused because of this window.
    pub exhausted: bool,
    /// Window length in minutes, when reported or known natively.
    pub minutes: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observed {
    pub agent: String,
    pub provider: String,
    pub window: Window,
    pub observed_at: f64,
}

/// Parse only the native subscription carriers; malformed fields stay unknown.
pub fn windows_from_native(agent: &str, frame: &Value) -> Vec<Window> {
    match agent {
        "claude" => claude_windows(frame),
        "codex" => codex_windows(frame),
        _ => vec![],
    }
}

fn claude_window(kind: &str, value: &Value, exhausted: bool) -> Window {
    Window {
        kind: kind.into(),
        used: value.get("utilization").and_then(Value::as_f64),
        resets_at: value.get("resetsAt").and_then(Value::as_f64),
        exhausted,
        minutes: if kind == "five_hour" {
            Some(300)
        } else if kind.starts_with("seven_day") {
            Some(10080)
        } else {
            None
        },
    }
}

fn claude_windows(frame: &Value) -> Vec<Window> {
    let Some(info) = frame.get("rate_limit_info").filter(|v| v.is_object()) else {
        return vec![];
    };
    let named = info.get("rateLimitType").and_then(Value::as_str);
    // A per-window rejection can coexist with allowed overage requests.
    let refused = info.get("status").and_then(Value::as_str) == Some("rejected")
        && info.get("isUsingOverage").and_then(Value::as_bool) != Some(true);
    let mut windows = vec![];
    if let Some(unified) = info.get("unifiedWindows").and_then(Value::as_object) {
        for (kind, value) in unified {
            if value.is_object() {
                windows.push(claude_window(kind, value, refused && named == Some(kind.as_str())));
            }
        }
    }
    if let Some(kind) = named
        && !windows.iter().any(|w| w.kind == kind)
    {
        windows.push(claude_window(kind, info, refused));
    }
    windows
}

fn codex_windows(frame: &Value) -> Vec<Window> {
    let body = match frame.get("method").and_then(Value::as_str) {
        Some("account/rateLimits/updated") => frame.get("params"),
        Some("account/rateLimits/read") => frame.get("result"),
        _ => None,
    };
    let Some(limits) = body.and_then(|v| v.get("rateLimits")).filter(|v| v.is_object()) else {
        return vec![];
    };
    let mut windows = vec![];
    for kind in ["primary", "secondary"] {
        let Some(value) = limits.get(kind).filter(|v| v.is_object()) else {
            continue;
        };
        let percent = value.get("usedPercent").and_then(Value::as_f64);
        windows.push(Window {
            kind: kind.into(),
            used: percent.map(|p| p / 100.0),
            resets_at: value.get("resetsAt").and_then(Value::as_f64),
            exhausted: percent.is_some_and(|p| p >= 100.0),
            minutes: value.get("windowDurationMins").and_then(Value::as_i64),
        });
    }
    if limits.get("rateLimitReachedType").is_some_and(|v| !v.is_null())
        && !windows.iter().any(|w| w.exhausted)
        && let Some((index, _)) = windows
            .iter()
            .enumerate()
            .filter_map(|(i, w)| w.used.map(|used| (i, used)))
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
    {
        windows[index].exhausted = true;
    }
    windows
}

fn exists(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM sqlite_master WHERE type='table' AND name='quota_windows'")?
        .exists([])?)
}

fn row_window(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Window> {
    Ok(Window {
        kind: row.get(start)?,
        used: row.get(start + 1)?,
        resets_at: row.get(start + 2)?,
        exhausted: row.get::<_, i64>(start + 3)? != 0,
        minutes: row.get(start + 4)?,
    })
}

/// Current observations, without creating a table in old or read-only journals.
pub fn load(conn: &Connection, now: f64) -> Result<Vec<Observed>> {
    if !exists(conn)? {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT agent,provider,kind,used,resets_at,exhausted,minutes,observed_at
         FROM quota_windows WHERE resets_at > ?1 OR (resets_at IS NULL AND observed_at >= ?2)
         ORDER BY agent,provider,kind",
    )?;
    let rows = stmt.query_map(params![now, now - MAX_AGE], |row| {
        Ok(Observed {
            agent: row.get(0)?,
            provider: row.get(1)?,
            window: row_window(row, 2)?,
            observed_at: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn materially_changed(previous: &Window, next: &Window) -> bool {
    previous.exhausted != next.exhausted
        || match (previous.resets_at, next.resets_at) {
            (Some(a), Some(b)) => (a - b).abs() > 60.0,
            (None, None) => false,
            _ => true,
        }
        || previous.used.map(|v| (v * 100.0).round()) != next.used.map(|v| (v * 100.0).round())
}

/// Refresh the complete row even when the change is too small to journal.
/// The writer owns schema initialization and transaction boundaries.
pub fn upsert(conn: &Connection, agent: &str, provider: &str, window: &Window, now: f64) -> Result<bool> {
    let previous = conn
        .query_row(
            "SELECT kind,used,resets_at,exhausted,minutes FROM quota_windows
             WHERE agent=?1 AND provider=?2 AND kind=?3",
            params![agent, provider, window.kind],
            |row| row_window(row, 0),
        )
        .optional()?;
    let changed = previous.as_ref().is_none_or(|old| materially_changed(old, window));
    conn.execute(
        "INSERT INTO quota_windows(agent,provider,kind,used,resets_at,exhausted,minutes,observed_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(agent,provider,kind) DO UPDATE SET used=excluded.used,resets_at=excluded.resets_at,
           exhausted=excluded.exhausted,minutes=excluded.minutes,observed_at=excluded.observed_at",
        params![agent, provider, window.kind, window.used, window.resets_at, window.exhausted as i64, window.minutes, now],
    )?;
    Ok(changed)
}

pub fn clear_expired(conn: &Connection, now: f64) -> Result<usize> {
    if !exists(conn)? {
        return Ok(0);
    }
    Ok(conn.execute(
        "DELETE FROM quota_windows WHERE resets_at <= ?1 OR (resets_at IS NULL AND observed_at < ?2)",
        params![now, now - MAX_AGE],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE quota_windows(agent TEXT NOT NULL,provider TEXT NOT NULL,kind TEXT NOT NULL,
             used REAL,resets_at REAL,exhausted INTEGER NOT NULL DEFAULT 0,minutes INTEGER,observed_at REAL NOT NULL,
             PRIMARY KEY(agent,provider,kind));",
        ).unwrap();
        conn
    }

    fn window(kind: &str, used: Option<f64>, reset: Option<f64>) -> Window {
        Window { kind: kind.into(), used, resets_at: reset, exhausted: false, minutes: None }
    }

    #[test]
    fn claude_unified_windows_only_mark_the_refused_named_window() {
        let mut frame = json!({"type":"rate_limit_event", "rate_limit_info":{
            "status":"rejected", "rateLimitType":"five_hour", "utilization":0.99,
            "unifiedWindows":{
                "five_hour":{"utilization":1.0,"resetsAt":2000,"status":"allowed"},
                "seven_day":{"utilization":0.7,"resetsAt":3000,"status":"rejected"}
            }
        }});
        let windows = windows_from_native("claude", &frame);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], Window { exhausted:true, minutes:Some(300), ..window("five_hour", Some(1.0), Some(2000.0)) });
        assert_eq!(windows[1], Window { minutes:Some(10080), ..window("seven_day", Some(0.7), Some(3000.0)) });
        frame["rate_limit_info"]["isUsingOverage"] = json!(true);
        assert!(windows_from_native("claude", &frame).iter().all(|w| !w.exhausted));
        frame["rate_limit_info"]["isUsingOverage"] = json!(false);
        frame["rate_limit_info"]["status"] = json!("allowed_warning");
        assert!(windows_from_native("claude", &frame).iter().all(|w| !w.exhausted));
    }

    #[test]
    fn claude_repeated_info_supports_top_level_missing_and_unknown_fields() {
        let frame = json!({"type":"result", "rate_limit_info":{
            "status":"rejected", "rateLimitType":"seven_day_opus", "utilization":1.2,"resetsAt":2222,
            "unifiedWindows":{"five_hour":{},"broken":null}
        }});
        let windows = windows_from_native("claude", &frame);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], Window { minutes:Some(300), ..window("five_hour", None, None) });
        assert_eq!(windows[1], Window { exhausted:true, minutes:Some(10080), ..window("seven_day_opus", Some(1.2), Some(2222.0)) });
        let unknown = json!({"rate_limit_info":{"rateLimitType":"overage","utilization":"1","resetsAt":[],"status":"rejected"}});
        assert_eq!(windows_from_native("claude", &unknown), vec![Window { exhausted:true, ..window("overage", None, None) }]);
        let unnamed = json!({"rate_limit_info":{"status":"rejected","unifiedWindows":{"five_hour":{"status":"rejected"}}}});
        assert!(!windows_from_native("claude", &unnamed)[0].exhausted);
    }

    #[test]
    fn codex_notification_and_read_use_only_rate_limits() {
        let limits = json!({"limitId":"codex", "primary":{"usedPercent":31,"windowDurationMins":300,"resetsAt":1234},
            "secondary":{"usedPercent":101,"windowDurationMins":10080,"resetsAt":5678}});
        let notification = json!({"method":"account/rateLimits/updated","params":{"rateLimits":limits}});
        let read = json!({"method":"account/rateLimits/read","result":{"rateLimits":limits,
            "rateLimitsByLimitId":{"other":{"primary":{"usedPercent":100}}}}});
        let windows = windows_from_native("codex", &notification);
        assert_eq!(windows, windows_from_native("codex", &read));
        assert_eq!(windows, vec![
            Window { minutes:Some(300), ..window("primary", Some(0.31), Some(1234.0)) },
            Window { exhausted:true, minutes:Some(10080), ..window("secondary", Some(1.01), Some(5678.0)) },
        ]);
    }

    #[test]
    fn codex_0169_read_primary_weekly_window_with_null_secondary() {
        // Window values match the installed 0.169.0 read response; the extra
        // account metadata is synthetic and must not contribute any windows.
        let result = json!({
            "rateLimits": {
                "primary": {"usedPercent":53,"windowDurationMins":10080,"resetsAt":1795279813},
                "secondary": null
            },
            "accountId": "synthetic-account",
            "planType": "plus",
            "credits": {"hasCredits":false,"unlimited":false,"balance":null},
            "rateLimitsByLimitId": {"ignored":{"primary":{"usedPercent":100,"windowDurationMins":300}}},
            "rateLimitResetCredits": null
        });
        let expected = vec![Window {
            kind: "primary".into(),
            used: Some(0.53),
            resets_at: Some(1795279813.0),
            exhausted: false,
            minutes: Some(10080),
        }];
        let full = json!({"method":"account/rateLimits/read","result":result});
        let journal = json!({"method":"account/rateLimits/read","result":{"rateLimits":result["rateLimits"]}});
        assert_eq!(windows_from_native("codex", &full), expected);
        assert_eq!(windows_from_native("codex", &journal), expected);
    }

    #[test]
    fn codex_reached_hint_marks_highest_known_percent_without_overriding_a_full_window() {
        let mut frame = json!({"method":"account/rateLimits/updated","params":{"rateLimits":{
            "primary":{"usedPercent":45}, "secondary":{"usedPercent":90}, "rateLimitReachedType":"plan"
        }}});
        let windows = windows_from_native("codex", &frame);
        assert!(!windows[0].exhausted);
        assert!(windows[1].exhausted);
        frame["params"]["rateLimits"]["primary"]["usedPercent"] = json!(100);
        let windows = windows_from_native("codex", &frame);
        assert!(windows[0].exhausted);
        assert!(!windows[1].exhausted);
        frame["params"]["rateLimits"]["primary"]["usedPercent"] = json!(45);
        frame["params"]["rateLimits"]["rateLimitReachedType"] = Value::Null;
        assert!(windows_from_native("codex", &frame).iter().all(|w| !w.exhausted));
    }

    #[test]
    fn parsers_tolerate_odd_and_missing_fields() {
        for value in [Value::Null, json!([]), json!(true), json!(5), json!({}), json!({"rate_limit_info":[]})] {
            assert!(windows_from_native("claude", &value).is_empty());
            assert!(windows_from_native("codex", &value).is_empty());
        }
        let frame = json!({"method":"account/rateLimits/updated","params":{"rateLimits":{
            "primary":{"usedPercent":"100","resetsAt":false,"windowDurationMins":[]},
            "secondary":null,"rateLimitReachedType":"plan"
        }}});
        assert_eq!(windows_from_native("codex", &frame), vec![window("primary", None, None)]);
        assert!(windows_from_native("deepseek", &frame).is_empty());
        assert!(windows_from_native("codex", &json!({"method":"account/rateLimits/read","result":{"rateLimitsByLimitId":{"primary":{}}}})).is_empty());
        assert!(windows_from_native("claude", &json!({"rate_limit_info":{"unifiedWindows":[]}})).is_empty());
    }

    #[test]
    fn missing_readonly_table_is_empty_without_creating_it() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA query_only=ON").unwrap();
        assert!(load(&conn, 1000.0).unwrap().is_empty());
        assert_eq!(clear_expired(&conn, 1000.0).unwrap(), 0);
        assert!(!exists(&conn).unwrap());
    }

    #[test]
    fn expiry_and_cleanup_share_exact_boundaries() {
        let conn = db();
        let now = 50000.0;
        for (kind, reset, observed) in [
            ("future", Some(now + 1.0), 0.0),
            ("reset_now", Some(now), now),
            ("reset_past", Some(now - 1.0), now),
            ("fresh", None, now),
            ("six_hours", None, now - MAX_AGE),
            ("stale", None, now - MAX_AGE - 0.1),
        ] {
            upsert(&conn, "claude", "native", &window(kind, None, reset), observed).unwrap();
        }
        let current = load(&conn, now).unwrap();
        assert_eq!(current.iter().map(|v| v.window.kind.as_str()).collect::<Vec<_>>(), vec!["fresh", "future", "six_hours"]);
        assert_eq!(clear_expired(&conn, now).unwrap(), 3);
        assert_eq!(load(&conn, now).unwrap(), current);
        let remaining:i64 = conn.query_row("SELECT count(*) FROM quota_windows", [], |row| row.get(0)).unwrap();
        assert_eq!(remaining, 3);
    }

    #[test]
    fn material_changes_are_distinct_from_row_refreshes() {
        let conn = db();
        let mut value = window("primary", Some(0.31), Some(10000.0));
        assert!(upsert(&conn, "codex", "native", &value, 1.0).unwrap());
        assert!(!upsert(&conn, "codex", "native", &value, 2.0).unwrap());
        value.used = Some(0.314);
        value.resets_at = Some(10060.0);
        value.minutes = Some(300);
        assert!(!upsert(&conn, "codex", "native", &value, 3.0).unwrap());
        let stored = load(&conn, 3.0).unwrap();
        assert_eq!(stored[0].window, value);
        assert_eq!(stored[0].observed_at, 3.0);
        value.used = Some(0.316);
        assert!(upsert(&conn, "codex", "native", &value, 4.0).unwrap());
        value.resets_at = Some(10121.0);
        assert!(upsert(&conn, "codex", "native", &value, 5.0).unwrap());
        value.exhausted = true;
        assert!(upsert(&conn, "codex", "native", &value, 6.0).unwrap());
        value.exhausted = false;
        assert!(upsert(&conn, "codex", "native", &value, 7.0).unwrap());
        value.resets_at = None;
        assert!(upsert(&conn, "codex", "native", &value, 8.0).unwrap());
        value.used = None;
        assert!(upsert(&conn, "codex", "native", &value, 9.0).unwrap());
        assert!(!upsert(&conn, "codex", "native", &value, 10.0).unwrap());
        value.used = Some(0.32);
        value.resets_at = Some(10121.0);
        assert!(upsert(&conn, "codex", "native", &value, 11.0).unwrap());
        assert!(upsert(&conn, "codex", "other", &value, 12.0).unwrap());
        assert!(upsert(&conn, "claude", "native", &value, 13.0).unwrap());
        assert_eq!(load(&conn, 13.0).unwrap().len(), 3);
    }
}
