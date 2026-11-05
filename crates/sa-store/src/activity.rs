//! Shared progress item type and helpers that reproduce JavaScript value
//! semantics (truthiness, optional chaining, `Number.isSafeInteger`) on
//! `serde_json::Value`.

use serde::Serialize;
use serde_json::Value;

/// One step of a batched tool call (`steps` in native-activity.cjs).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Step {
    pub tool: String,
    pub state: &'static str,
}

/// A progress item as produced by the JavaScript projection. Serializes with
/// the same keys as the JS object (`taskId` stays camelCase because the item
/// is an opaque JSON payload on `EventView`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Activity {
    pub label: String,
    pub category: &'static str,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(rename = "taskId", skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<Step>>,
}

impl Activity {
    pub fn new(label: impl Into<String>, category: &'static str, state: &'static str) -> Self {
        Self {
            label: label.into(),
            category,
            state,
            tool: None,
            task_id: None,
            title: None,
            steps: None,
        }
    }

    pub fn with_tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("activity serializes")
    }
}

/// `v?.[key]` on a JSON value: only objects have properties.
pub(crate) fn get<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v?.as_object()?.get(key)
}

/// `typeof v?.[key] === 'string' ? v[key] : undefined`
pub(crate) fn get_str<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a str> {
    get(v, key)?.as_str()
}

/// JavaScript truthiness of a JSON value (`undefined` is `None`).
pub(crate) fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// `v === true`
pub(crate) fn is_true(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

/// `Number.isSafeInteger(v) ? v : None`
pub(crate) fn safe_integer(v: Option<&Value>) -> Option<i64> {
    const MAX: f64 = 9_007_199_254_740_991.0;
    let n = v?.as_number()?;
    if let Some(i) = n.as_i64() {
        return (i.unsigned_abs() <= MAX as u64).then_some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() <= MAX).then_some(f as i64)
}

/// The characters JavaScript's `\s` and `String.prototype.trim` treat as
/// whitespace (WhiteSpace + LineTerminator).
pub(crate) fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Regex character class equivalent of JavaScript `\s`.
pub(crate) const JS_SPACE_CLASS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// JavaScript `String.prototype.length` (UTF-16 code units).
pub(crate) fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}
