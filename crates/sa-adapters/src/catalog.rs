//! Complete, bounded model listings. Pagination preserves credential origin.
use crate::common::get_json_with;
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub fn efforts(row: &Value) -> Vec<String> {
    let levels = row
        .get("reasoningEfforts")
        .or_else(|| row.get("supported_reasoning_levels"))
        .or_else(|| row.get("supportedReasoningEfforts"))
        .or_else(|| row.get("supportedEffortLevels"));
    let mut out = vec![];
    if let Some(levels) = levels {
        if let Some(array) = levels.as_array() {
            for entry in array {
                if let Some(s) = entry
                    .as_str()
                    .or_else(|| entry["effort"].as_str())
                    .or_else(|| entry["reasoning_effort"].as_str())
                {
                    out.push(s.to_string());
                }
            }
        } else if let Some(object) = levels.as_object() {
            out.extend(object.keys().cloned());
        }
    } else if let Some(caps) = row["capabilities"]["effort"].as_object() {
        for (level, capability) in caps {
            if capability["supported"] == true {
                out.push(level.clone());
            }
        }
    }
    out.retain(|s| {
        !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    let mut seen = HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    out
}

/// Normalize provider metadata without guessing capabilities from model names.
pub fn model(row: &Value) -> Option<Value> {
    let id = row["id"]
        .as_str()
        .or_else(|| row["slug"].as_str())
        .or_else(|| row["value"].as_str())
        .or_else(|| row.as_str())
        .or_else(|| row["name"].as_str().and_then(|s| s.strip_prefix("models/")))?;
    if !sa_store::providers::valid_model_id(id) {
        return None;
    }
    let label = row["display_name"]
        .as_str()
        .or_else(|| row["displayName"].as_str())
        .or_else(|| row["label"].as_str())
        .or_else(|| row["name"].as_str().filter(|s| !s.starts_with("models/")))
        .unwrap_or(id);
    let mut out: serde_json::Map<String, Value> = row
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| {
            [
                "name",
                "resolvedModel",
                "capabilities",
                "api_capabilities",
                "input_modalities",
                "output_modalities",
                "context_window",
                "max_output_tokens",
                "min_client_version",
                "minimum_client_version",
                "minimum_harness_version",
                "client_requirements",
                "supportedGenerationMethods",
                "input",
                "inputModalities",
                "contextWindow",
                "maxTokens",
                "compat",
                "systemPromptUpdate",
                "toolUpdate",
            ]
            .contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.insert("id".into(), json!(id));
    out.insert("label".into(), json!(label));
    if row["reasoningEfforts"].is_object() {
        out.insert(
            "reasoningEffortMapping".into(),
            row["reasoningEfforts"].clone(),
        );
    } else if let Some(mapping) = row.get("reasoningEffortMapping") {
        out.insert("reasoningEffortMapping".into(), mapping.clone());
    }
    let levels = efforts(row);
    if !levels.is_empty()
        || [
            "reasoningEfforts",
            "supported_reasoning_levels",
            "supportedReasoningEfforts",
            "supportedEffortLevels",
        ]
        .iter()
        .any(|k| row.get(*k).is_some())
        || row
            .get("capabilities")
            .and_then(|c| c.get("effort"))
            .is_some()
        || row["supportsEffort"] == false
    {
        out.insert("reasoningEfforts".into(), json!(levels));
    }
    let default = row["defaultReasoningEffort"]
        .as_str()
        .or_else(|| row["default_reasoning_level"].as_str());
    if let Some(default) = default.filter(|s| levels.iter().any(|e| e == s)) {
        out.insert("defaultReasoningEffort".into(), json!(default));
    }
    Some(Value::Object(out))
}

pub fn supplement(rows: &mut [Value], metadata: &[Value]) {
    for row in rows {
        if row.get("reasoningEfforts").is_some() {
            continue;
        }
        if let Some(info) = metadata
            .iter()
            .find(|m| m["id"] == row["id"] || m["resolvedModel"] == row["id"])
        {
            let levels = efforts(info);
            if !levels.is_empty() {
                row["reasoningEfforts"] = json!(levels);
                if info["reasoningEfforts"].is_object() {
                    row["reasoningEffortMapping"] = info["reasoningEfforts"].clone();
                } else if let Some(mapping) = info.get("reasoningEffortMapping") {
                    row["reasoningEffortMapping"] = mapping.clone();
                }
                if let Some(default) = info.get("defaultReasoningEffort") {
                    row["defaultReasoningEffort"] = default.clone();
                }
            }
        }
    }
}

/// A provider capability must also be representable by the selected harness.
pub fn restrict_efforts(rows: &mut [Value], allowed: &[&str]) {
    for row in rows {
        if row.get("reasoningEfforts").is_none() {
            continue;
        }
        let levels: Vec<String> = efforts(row)
            .into_iter()
            .filter(|s| allowed.contains(&s.as_str()))
            .collect();
        if row["defaultReasoningEffort"]
            .as_str()
            .is_some_and(|s| !levels.iter().any(|l| l == s))
        {
            row.as_object_mut()
                .unwrap()
                .remove("defaultReasoningEffort");
        }
        row["reasoningEfforts"] = json!(levels);
    }
}

pub async fn fetch(
    url: &str,
    headers: &[(String, String)],
    cancel: &CancellationToken,
) -> Result<Vec<Value>> {
    let original = url::Url::parse(url).map_err(|_| anyhow!("模型目录地址无效"))?;
    let mut next = original.clone();
    let mut visited = HashSet::new();
    let mut ids = HashSet::new();
    let mut rows = vec![];
    let deadline = Instant::now() + Duration::from_secs(30);
    for _ in 0..64 {
        ensure!(
            next.origin() == original.origin()
                && next.username().is_empty()
                && next.password().is_none()
                && next.fragment().is_none(),
            "模型分页地址与认证来源不匹配"
        );
        ensure!(visited.insert(next.to_string()), "模型目录返回重复分页游标");
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "模型目录分页超时，未取得完整列表");
        let body = get_json_with(
            next.as_str(),
            headers,
            remaining.min(Duration::from_secs(10)),
            4 * 1024 * 1024,
            cancel,
        )
        .await?;
        let entries = body["data"]
            .as_array()
            .or_else(|| body["models"].as_array())
            .or_else(|| body.as_array())
            .ok_or_else(|| anyhow!("模型目录响应缺少模型列表"))?;
        for entry in entries {
            if let Some(row) = model(entry) {
                if ids.insert(row["id"].as_str().unwrap_or_default().to_string()) {
                    rows.push(row);
                }
                ensure!(
                    rows.len() <= 16_384,
                    "模型目录超过 16384 项，未返回截断列表"
                );
            }
        }
        if let Some(token) = body["nextPageToken"].as_str().filter(|v| !v.is_empty()) {
            next = original.clone();
            next.query_pairs_mut().append_pair("pageToken", token);
        } else if body["has_more"] == true {
            let cursor = body["last_id"]
                .as_str()
                .or_else(|| entries.last().and_then(|m| m["id"].as_str()))
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("模型目录声明还有分页但缺少游标"))?;
            next = original.clone();
            next.query_pairs_mut().append_pair("after_id", cursor);
        } else if let Some(link) = body["next"].as_str().filter(|v| !v.is_empty()) {
            next = next.join(link).map_err(|_| anyhow!("模型分页地址无效"))?;
        } else {
            ensure!(!rows.is_empty(), "端点未返回可用模型");
            return Ok(rows);
        }
    }
    bail!("模型目录超过 64 页，未取得完整列表")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_are_taken_from_metadata_only() {
        assert!(efforts(&json!({"id":"gpt-made-up"})).is_empty());
        assert_eq!(
            efforts(&json!({"supported_reasoning_levels":[{"effort":"high"}]})),
            ["high"]
        );
        assert_eq!(
            efforts(
                &json!({"capabilities":{"effort":{"low":{"supported":true},"max":{"supported":false}}}})
            ),
            ["low"]
        );
        assert_eq!(
            model(&json!({"name":"models/gemini-x"})).unwrap()["id"],
            "gemini-x"
        );
        let mut rows = vec![model(&json!({"id":"x","reasoningEfforts":false})).unwrap()];
        supplement(&mut rows, &[json!({"id":"x","reasoningEfforts":["high"]})]);
        assert_eq!(rows[0]["reasoningEfforts"], json!([]));
        for key in [
            "supported_reasoning_levels",
            "supportedReasoningEfforts",
            "supportedEffortLevels",
        ] {
            let mut source = json!({"id":"x"});
            source[key] = json!([]);
            let mut rows = vec![model(&source).unwrap()];
            supplement(&mut rows, &[json!({"id":"x","reasoningEfforts":["high"]})]);
            assert_eq!(rows[0]["reasoningEfforts"], json!([]));
        }
        let invalid_default =
            model(&json!({"id":"x","reasoningEfforts":["low"],"defaultReasoningEffort":"high"}))
                .unwrap();
        assert!(invalid_default.get("defaultReasoningEffort").is_none());
        let mut unsupported = vec![model(&json!({"id":"x","capabilities":{"effort":{"low":{"supported":false},"high":{"supported":false}}}})).unwrap()];
        supplement(
            &mut unsupported,
            &[json!({"id":"x","reasoningEfforts":["high"]})],
        );
        assert_eq!(unsupported[0]["reasoningEfforts"], json!([]));
    }
}
