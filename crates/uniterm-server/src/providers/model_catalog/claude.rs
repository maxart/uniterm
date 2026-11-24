//! The native SDK initialization reports the same model capabilities as Claude's picker.
use serde_json::Value;
use std::path::Path;
use uniterm_core::agent_detail::bounded_text;
use uniterm_core::agent_models::{EffortChoice, EffortSupport, ModelCatalog, ModelChoice};

pub(super) async fn discover(command: &str, cwd: &Path) -> Result<ModelCatalog, String> {
    let response = super::rpc(command, &[
        "--print", "--verbose", "--input-format", "stream-json", "--output-format", "stream-json",
        "--no-session-persistence", "--strict-mcp-config", "--mcp-config", "{\"mcpServers\":{}}",
        "--settings", "{\"disableAllHooks\":true}", "--tools", "", "--no-chrome", "--disable-slash-commands",
    ], cwd, &[(serde_json::json!({
        "type":"control_request", "request_id":"uniterm-models", "request":{"subtype":"initialize"}
    }).to_string(), Some(("/response/request_id".into(), "uniterm-models".into())))]).await?;
    parse(&response)
}

fn identifier(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then(|| value.to_owned())
}

fn parse(response: &Value) -> Result<ModelCatalog, String> {
    if response.get("type").and_then(Value::as_str) != Some("control_response")
        || response
            .pointer("/response/subtype")
            .and_then(Value::as_str)
            != Some("success")
    {
        return Err(
            "Claude could not return its model catalog; check its native model picker and sign-in"
                .into(),
        );
    }
    let rows = response.pointer("/response/response/models").and_then(Value::as_array)
        .ok_or("This Claude version did not report model choices; update Claude or use its native model picker")?;
    let mut models: Vec<ModelChoice> = Vec::new();
    for row in rows.iter().take(256) {
        if row.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(id) = identifier(row.get("value")) else {
            continue;
        };
        // `default` clears the override; the common picker already supplies that action.
        if id == "default" || models.iter().any(|model| model.id == id) {
            continue;
        }
        let mut efforts: Vec<EffortChoice> = Vec::new();
        let supports = row.get("supportsEffort").and_then(Value::as_bool);
        let levels = row.get("supportedEffortLevels").and_then(Value::as_array);
        if supports != Some(false) {
            if let Some(levels) = levels {
                for level in levels.iter().take(32) {
                    let Some(id) = identifier(Some(level)) else {
                        continue;
                    };
                    if efforts.iter().any(|effort| effort.id == id) {
                        continue;
                    }
                    efforts.push(EffortChoice {
                        label: id.clone(),
                        id,
                        ..Default::default()
                    });
                }
            }
        }
        let effort_support = if !efforts.is_empty() {
            EffortSupport::Supported
        } else if supports == Some(false) || levels.is_some_and(Vec::is_empty) {
            EffortSupport::Unsupported
        } else {
            EffortSupport::Unknown
        };
        let aliases = identifier(row.get("resolvedModel"))
            .filter(|resolved| *resolved != id)
            .into_iter()
            .collect();
        models.push(ModelChoice {
            label: bounded_text(
                row.get("displayName")
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
            aliases,
            efforts,
            effort_support,
            ..Default::default()
        });
    }
    if models.is_empty() {
        return Err("Claude reported no selectable models; open its native model picker and check account restrictions".into());
    }
    Ok(ModelCatalog {
        provider: "claude".into(), models,
        notice: "Model choices and reasoning levels reported by Claude; default effort follows its native settings".into(),
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
        return Err("Choose a reasoning level advertised by Claude".into());
    }
    Ok(vec!["--effort".into(), effort.into()])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(models: Value) -> Value {
        serde_json::json!({"type":"control_response","response":{"subtype":"success","request_id":"uniterm-models","response":{"models":models}}})
    }
    #[test]
    fn native_choices_keep_aliases_and_model_specific_effort_without_guessing_defaults() {
        let catalog = parse(&response(serde_json::json!([
            {"value":"sonnet","resolvedModel":"claude-sonnet-fixture","displayName":"Sonnet fixture","supportsEffort":true,"supportedEffortLevels":["low","medium","high"]},
            {"value":"opus","supportsEffort":true,"supportedEffortLevels":["low","medium","high","xhigh","max"]},
            {"value":"default"},{"value":"restricted","disabled":true}
        ]))).unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].id, "sonnet");
        assert_eq!(catalog.models[0].aliases, ["claude-sonnet-fixture"]);
        assert_eq!(catalog.models[0].efforts.len(), 3);
        assert_eq!(catalog.models[1].efforts.len(), 5);
        assert!(catalog.models[0].default_effort.is_none());
        assert_eq!(effort_args("max").unwrap(), ["--effort", "max"]);
    }

    #[test]
    fn unavailable_capabilities_and_untrusted_metadata_are_not_invented() {
        let catalog = parse(&response(serde_json::json!([
            {"value":"unknown","displayName":"Name\u{001b}\n"},
            {"value":"no-effort","supportsEffort":false,"supportedEffortLevels":["high"]},
            {"value":"bad\nmodel"}
        ])))
        .unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].label, "Name ");
        assert_eq!(catalog.models[0].effort_support, EffortSupport::Unknown);
        assert_eq!(catalog.models[1].effort_support, EffortSupport::Unsupported);
        assert!(catalog.models[1].efforts.is_empty());
        assert!(parse(&response(serde_json::json!([]))).is_err());
        assert!(parse(&serde_json::json!({"error":"secret"}))
            .unwrap_err()
            .find("secret")
            .is_none());
        assert!(effort_args("high\n").is_err());
    }
}
