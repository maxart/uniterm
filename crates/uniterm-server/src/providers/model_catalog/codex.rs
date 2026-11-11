//! Codex owns picker IDs and reasoning capabilities; labels are never launch IDs.
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use uniterm_core::agent_detail::bounded_text;
use uniterm_core::agent_models::{EffortChoice, EffortSupport, ModelCatalog, ModelChoice};

const CACHE_LIMIT: u64 = 2 * 1024 * 1024;

pub(super) async fn discover(
    command: &str,
    cwd: &Path,
    refresh: bool,
) -> Result<ModelCatalog, String> {
    if !refresh {
        if let Some(catalog) = cached().await {
            return Ok(catalog);
        }
    }
    match discover_native(command, cwd).await {
        Ok(catalog) => Ok(catalog),
        Err(error) => {
            if refresh {
                if let Some(mut catalog) = cached().await {
                    catalog.notice =
                        format!("Live Codex discovery unavailable; {}", catalog.notice);
                    return Ok(catalog);
                }
            }
            Err(error)
        }
    }
}

async fn cached() -> Option<ModelCatalog> {
    let path = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .map(|home| home.join("models_cache.json"));
    if let Some(path) = path {
        if let Ok(Some(catalog)) = tokio::task::spawn_blocking(move || read_cache(&path)).await {
            return Some(catalog);
        }
    }
    None
}

async fn discover_native(command: &str, cwd: &Path) -> Result<ModelCatalog, String> {
    let response = super::rpc(command, &["app-server"], cwd, &[
        (serde_json::json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"uniterm","title":"Uniterm","version":env!("CARGO_PKG_VERSION")}}}).to_string(), Some(("id".into(), "1".into()))),
        (serde_json::json!({"method":"initialized","params":{}}).to_string(), None),
        (serde_json::json!({"id":2,"method":"model/list","params":{"limit":256,"includeHidden":false}}).to_string(), Some(("id".into(), "2".into()))),
    ]).await?;
    let result = response.get("result").ok_or(
        "Codex could not return its model catalog; check its native model picker and sign-in",
    )?;
    let mut catalog = parse(result, false)?;
    catalog.notice = if result
        .get("nextCursor")
        .is_some_and(|cursor| !cursor.is_null())
    {
        "Models reported by Codex; only the first 256 are shown".into()
    } else {
        "Models and reasoning levels reported by Codex".into()
    };
    Ok(catalog)
}

fn read_cache(path: &Path) -> Option<ModelCatalog> {
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > CACHE_LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(CACHE_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > CACHE_LIMIT {
        return None;
    }
    let data: Value = serde_json::from_slice(&bytes).ok()?;
    let mut catalog = parse(&data, true).ok()?;
    catalog.notice = match data.get("fetched_at").and_then(Value::as_str) {
        Some(time) => format!(
            "Cached Codex model catalog from {}; availability may have changed",
            bounded_text(time, 80)
        ),
        None => "Cached Codex model catalog; availability may have changed".into(),
    };
    Some(catalog)
}

fn identifier(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then(|| value.to_owned())
}

fn parse(data: &Value, cached: bool) -> Result<ModelCatalog, String> {
    let rows = data
        .get(if cached { "models" } else { "data" })
        .and_then(Value::as_array)
        .ok_or("Codex returned an unreadable model catalog")?;
    let mut models: Vec<ModelChoice> = Vec::new();
    for row in rows.iter().take(256) {
        if row.get("hidden").and_then(Value::as_bool) == Some(true)
            || (cached
                && row
                    .get("visibility")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v != "list"))
        {
            continue;
        }
        let Some(id) = identifier(row.get(if cached { "slug" } else { "model" })) else {
            continue;
        };
        if models.iter().any(|model| model.id == id) {
            continue;
        }
        let levels = row
            .get(if cached {
                "supported_reasoning_levels"
            } else {
                "supportedReasoningEfforts"
            })
            .and_then(Value::as_array);
        let mut efforts: Vec<EffortChoice> = Vec::new();
        if let Some(levels) = levels {
            for level in levels.iter().take(32) {
                let Some(id) =
                    identifier(level.get(if cached { "effort" } else { "reasoningEffort" }))
                else {
                    continue;
                };
                if efforts.iter().any(|effort| effort.id == id) {
                    continue;
                }
                efforts.push(EffortChoice {
                    label: id.clone(),
                    id,
                    description: bounded_text(
                        level
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        240,
                    ),
                });
            }
        }
        let default_effort = identifier(row.get(if cached {
            "default_reasoning_level"
        } else {
            "defaultReasoningEffort"
        }))
        .filter(|default| efforts.iter().any(|effort| effort.id == *default));
        let effort_support = if !efforts.is_empty() {
            EffortSupport::Supported
        } else if levels.is_some_and(Vec::is_empty) {
            EffortSupport::Unsupported
        } else {
            EffortSupport::Unknown
        };
        models.push(ModelChoice {
            label: bounded_text(
                row.get(if cached {
                    "display_name"
                } else {
                    "displayName"
                })
                .and_then(Value::as_str)
                .filter(|label| !label.trim().is_empty())
                .unwrap_or(&id),
                160,
            ),
            description: bounded_text(
                row.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                320,
            ),
            id,
            efforts,
            default_effort,
            effort_support,
            ..Default::default()
        });
    }
    if models.is_empty() {
        return Err(
            "Codex has no visible model choices; open its native model picker and check sign-in"
                .into(),
        );
    }
    Ok(ModelCatalog {
        provider: "codex".into(),
        models,
        ..Default::default()
    })
}

pub(crate) fn effort_args(effort: &str) -> Result<Vec<String>, String> {
    if effort.is_empty()
        || effort.len() > 64
        || !effort
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("Choose a reasoning level advertised by Codex".into());
    }
    Ok(vec![
        "-c".into(),
        format!("model_reasoning_effort=\"{effort}\""),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_keeps_luna_id_label_and_advertised_reasoning_distinct() {
        let data = serde_json::json!({"models":[{
            "slug":"gpt-6-luna","display_name":"GPT-6-Luna","visibility":"list",
            "default_reasoning_level":"medium", "supported_reasoning_levels":[
                {"effort":"low"},{"effort":"medium"},{"effort":"high"},{"effort":"xhigh"},{"effort":"max"}
            ]
        },{"slug":"internal","visibility":"hide"}]});
        let catalog = parse(&data, true).unwrap();
        assert_eq!(catalog.models.len(), 1);
        let model = &catalog.models[0];
        assert_eq!(model.id, "gpt-6-luna");
        assert_eq!(model.label, "GPT-6-Luna");
        assert_eq!(model.default_effort.as_deref(), Some("medium"));
        assert_eq!(model.effort_support, EffortSupport::Supported);
        assert_eq!(
            model
                .efforts
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn native_catalog_distinguishes_unknown_and_unsupported_and_sanitizes() {
        let catalog = parse(
            &serde_json::json!({"data":[
                {"model":"unknown", "displayName":"Label\u{001b}\n"},
                {"model":"none", "supportedReasoningEfforts":[]},
                {"model":"hidden", "hidden":true}, {"model":"bad\nname"}
            ]}),
            false,
        )
        .unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].label, "Label ");
        assert_eq!(catalog.models[0].effort_support, EffortSupport::Unknown);
        assert_eq!(catalog.models[1].effort_support, EffortSupport::Unsupported);
        assert_eq!(
            effort_args("medium").unwrap(),
            ["-c", "model_reasoning_effort=\"medium\""]
        );
        assert!(effort_args("high\"; bad").is_err());
    }

    #[test]
    fn native_luna_catalog_reads_camel_case_protocol_capabilities() {
        let catalog = parse(
            &serde_json::json!({"data":[{
            "id":"catalog-row", "model":"gpt-6-luna", "displayName":"GPT-6-Luna",
            "defaultReasoningEffort":"medium", "supportedReasoningEfforts":[
                {"reasoningEffort":"low"}, {"reasoningEffort":"medium"},
                {"reasoningEffort":"high"}, {"reasoningEffort":"xhigh"}, {"reasoningEffort":"max"}
            ]
        }],"nextCursor":null}),
            false,
        )
        .unwrap();
        let model = &catalog.models[0];
        assert_eq!(
            model.id, "gpt-6-luna",
            "the catalog row ID is not the launch ID"
        );
        assert_eq!(model.label, "GPT-6-Luna");
        assert_eq!(model.default_effort.as_deref(), Some("medium"));
        assert_eq!(model.efforts.len(), 5);
        assert_eq!(model.effort_support, EffortSupport::Supported);
    }
}
