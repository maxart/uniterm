//! Pi's native RPC catalog retains account/config model names and effort maps.
use std::path::Path;

use serde_json::Value;
use uniterm_core::agent_models::{EffortChoice, EffortSupport, ModelCatalog, ModelChoice};

const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub(super) async fn discover(command: &str, cwd: &Path) -> Result<ModelCatalog, String> {
    let args = [
        "--mode",
        "rpc",
        "--no-session",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        "--no-approve",
        "--offline",
    ];
    let response = super::rpc(
        command,
        &args,
        cwd,
        &[(
            "{\"id\":\"catalog\",\"type\":\"get_available_models\"}".into(),
            Some(("id".into(), "catalog".into())),
        )],
    )
    .await?;
    parse(&response)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('-')
        && !value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
}

fn label(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(160)
        .collect()
}

fn parse(response: &Value) -> Result<ModelCatalog, String> {
    if response.get("success").and_then(Value::as_bool) != Some(true)
        || response.get("command").and_then(Value::as_str) != Some("get_available_models")
    {
        return Err("Pi did not return its available model catalog".into());
    }
    let entries = response
        .pointer("/data/models")
        .and_then(Value::as_array)
        .ok_or("Pi returned an unsupported model catalog format")?;
    let mut catalog = ModelCatalog {
        provider: "pi".into(),
        notice: "Available account and models.json models from Pi. Extension-only providers are excluded; discovery makes no model requests.".into(),
        ..Default::default()
    };
    for entry in entries.iter().take(2048) {
        if entry
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "chat")
        {
            continue;
        }
        let (Some(provider), Some(id)) = (
            entry.get("provider").and_then(Value::as_str),
            entry.get("id").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !valid_id(provider) || !valid_id(id) {
            continue;
        }
        let reasoning = entry.get("reasoning").and_then(Value::as_bool);
        let support = match reasoning {
            Some(true) => EffortSupport::Supported,
            Some(false) => EffortSupport::Unsupported,
            None => EffortSupport::Unknown,
        };
        // Pi's own getSupportedThinkingLevels: null disables a level;
        // xhigh/max require an explicit mapping. No name-based guessing.
        let efforts = if reasoning == Some(true) {
            LEVELS
                .iter()
                .filter(|level| {
                    let mapping = entry
                        .get("thinkingLevelMap")
                        .and_then(|map| map.get(**level));
                    !mapping.is_some_and(Value::is_null)
                        && (!matches!(**level, "xhigh" | "max") || mapping.is_some())
                })
                .map(|level| EffortChoice {
                    id: (*level).into(),
                    label: (*level).into(),
                    description: String::new(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let choice = ModelChoice {
            id: format!("{provider}/{id}"),
            label: label(entry.get("name").and_then(Value::as_str).unwrap_or(id)),
            description: format!("Pi provider: {}", label(provider)),
            efforts,
            effort_support: support,
            ..Default::default()
        };
        if !catalog.models.iter().any(|old| old.id == choice.id) {
            catalog.models.push(choice);
        }
    }
    Ok(catalog)
}

/// Preserve prompts/session ids while replacing only explicit Pi selection flags.
pub(crate) fn selection_argv(
    argv: &[String],
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Vec<String>, String> {
    if argv.is_empty() {
        return Err("Pi launch command is empty".into());
    }
    if model.is_some_and(|value| !valid_id(value)) {
        return Err("Invalid Pi model ID".into());
    }
    if effort.is_some_and(|value| !LEVELS.contains(&value)) {
        return Err("Invalid Pi thinking level".into());
    }
    if model.is_none() && effort.is_none() {
        return Ok(argv.to_vec());
    }
    let mut output = vec![argv[0].clone()];
    if let Some(model) = model {
        output.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = effort {
        output.extend(["--thinking".into(), effort.into()]);
    }
    let mut index = 1;
    while index < argv.len() {
        let arg = &argv[index];
        if arg == "--" {
            output.extend_from_slice(&argv[index..]);
            break;
        }
        let replace = (model.is_some() && matches!(arg.as_str(), "--model" | "--provider"))
            || ((model.is_some() || effort.is_some()) && arg == "--thinking");
        if replace {
            if index + 1 >= argv.len() {
                return Err("Pi selection flag has no value".into());
            }
            index += 2;
        } else if (model.is_some()
            && (arg.starts_with("--model=") || arg.starts_with("--provider=")))
            || ((model.is_some() || effort.is_some()) && arg.starts_with("--thinking="))
        {
            index += 1;
        } else {
            output.push(arg.clone());
            index += 1;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_catalog_preserves_ids_and_uses_per_model_thinking_maps() {
        let value = serde_json::json!({"success":true,"command":"get_available_models","data":{"models":[
            {"provider":"openai-codex","id":"reasoner","name":"Reasoner\u{1b}","reasoning":true,"thinkingLevelMap":{"off":null,"minimal":null,"xhigh":"xhigh","max":null},"headers":{"authorization":"PRIVATE"}},
            {"provider":"custom","id":"plain","name":"Plain","reasoning":false},
            {"provider":"custom","id":"unknown"},
            {"provider":"bad\nprovider","id":"bad","reasoning":true}
        ]}});
        let catalog = parse(&value).unwrap();
        assert_eq!(catalog.models.len(), 3);
        assert_eq!(catalog.models[0].id, "openai-codex/reasoner");
        assert_eq!(catalog.models[0].label, "Reasoner");
        assert_eq!(
            catalog.models[0]
                .efforts
                .iter()
                .map(|effort| effort.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "medium", "high", "xhigh"]
        );
        assert_eq!(catalog.models[1].effort_support, EffortSupport::Unsupported);
        assert_eq!(catalog.models[2].effort_support, EffortSupport::Unknown);
        assert!(!serde_json::to_string(&catalog).unwrap().contains("PRIVATE"));
    }

    #[test]
    fn pi_selection_keeps_resume_and_prompt_and_overrides_provider() {
        let argv = [
            "pi",
            "--session",
            "session-id",
            "--provider",
            "old",
            "--model",
            "old-model",
            "--thinking",
            "low",
            "--",
            "--model",
        ]
        .map(String::from);
        let result = selection_argv(&argv, Some("custom/new"), Some("high")).unwrap();
        assert_eq!(
            result,
            [
                "pi",
                "--model",
                "custom/new",
                "--thinking",
                "high",
                "--session",
                "session-id",
                "--",
                "--model"
            ]
        );
        assert_eq!(
            selection_argv(&result, Some("custom/new"), Some("high")).unwrap(),
            result
        );
        assert!(selection_argv(&argv, Some("bad\nmodel"), None).is_err());
        let legacy = [
            "pi",
            "--model=old",
            "--thinking=high",
            "--",
            "literal prompt",
        ]
        .map(String::from);
        assert_eq!(
            selection_argv(&legacy, Some("custom/new"), None).unwrap(),
            ["pi", "--model", "custom/new", "--", "literal prompt"]
        );
    }
}
