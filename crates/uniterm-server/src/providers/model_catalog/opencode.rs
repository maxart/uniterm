//! Versioned OpenCode catalogs and explicit interactive model selection.
use std::path::Path;

use serde_json::Value;
use uniterm_core::agent_models::{EffortChoice, EffortSupport, ModelCatalog, ModelChoice};

const V1: &str = "opencode-v1";
const V1_NO_VARIANT: &str = "opencode-v1-no-variant";
const V2: &str = "opencode-v2-mini";

pub(super) async fn discover(command: &str, cwd: &Path) -> Result<ModelCatalog, String> {
    let version = super::run(command, &["--version"], cwd, None).await?;
    match major_version(&version) {
        Some(2) => {
            // OpenCode uses deep-object query parameters, not a JSON string.
            // Raw method/path ignores --param, so use the operation ID too.
            let location = format!("location[directory]={}", cwd.display());
            let output = super::run(
                command,
                &["api", "model.list", "--param", &location],
                cwd,
                None,
            )
            .await;
            let result = output.and_then(|output| {
                let value: Value = serde_json::from_str(&output).map_err(|_| {
                    "OpenCode returned an unsupported model catalog format".to_string()
                })?;
                parse_v2(&value)
            });
            // A failed catalog request does not erase the already established
            // launch interface used by the custom-model fallback.
            Ok(result.unwrap_or_else(|notice| unavailable(V2, notice)))
        }
        Some(1) => {
            let help = super::run(command, &["--help"], cwd, None).await?;
            let variant = help.contains("--variant");
            let result = super::run(command, &["models", "--verbose"], cwd, None)
                .await
                .and_then(|output| parse_v1(&output, variant));
            Ok(result.unwrap_or_else(|notice| {
                unavailable(if variant { V1 } else { V1_NO_VARIANT }, notice)
            }))
        }
        _ => Err("Cannot determine this OpenCode version's model selection interface".into()),
    }
}

fn unavailable(mode: &str, notice: String) -> ModelCatalog {
    ModelCatalog {
        provider: "opencode".into(),
        launch_mode: mode.into(),
        notice,
        ..Default::default()
    }
}

fn major_version(output: &str) -> Option<u32> {
    let version = output
        .trim()
        .strip_prefix("opencode ")
        .unwrap_or(output.trim());
    let version = version.strip_prefix('v').unwrap_or(version);
    version.split_once('.')?.0.parse().ok()
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

fn choice(entry: &Value, qualified_id: String, variants_supported: bool) -> Option<ModelChoice> {
    if !valid_id(&qualified_id)
        || entry.get("enabled").and_then(Value::as_bool) == Some(false)
        || entry
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "chat")
    {
        return None;
    }
    let variants = entry.get("variants");
    let variant_ids: Vec<&str> = match variants {
        Some(Value::Array(entries)) => entries
            .iter()
            .filter_map(|variant| variant.get("id").and_then(Value::as_str))
            .take(64)
            .collect(),
        Some(Value::Object(entries)) => entries
            .iter()
            .filter(|(_, value)| value.get("disabled").and_then(Value::as_bool) != Some(true))
            .map(|(id, _)| id.as_str())
            .take(64)
            .collect(),
        _ => Vec::new(),
    };
    let efforts: Vec<_> = if variants_supported {
        variant_ids
            .into_iter()
            .filter(|id| valid_id(id) && !id.contains('#'))
            .map(|id| EffortChoice {
                id: id.into(),
                label: label(id),
                description: "OpenCode model variant".into(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let effort_support = if !variants_supported || variants.is_some() && efforts.is_empty() {
        EffortSupport::Unsupported
    } else if !efforts.is_empty() {
        EffortSupport::Supported
    } else {
        EffortSupport::Unknown
    };
    Some(ModelChoice {
        label: label(
            entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&qualified_id),
        ),
        description: label(&qualified_id),
        id: qualified_id,
        efforts,
        effort_support,
        ..Default::default()
    })
}

fn parse_v2(value: &Value) -> Result<ModelCatalog, String> {
    let entries = value
        .get("data")
        .unwrap_or(value)
        .as_array()
        .ok_or("OpenCode returned an unsupported model catalog format")?;
    let mut catalog = ModelCatalog {
        provider: "opencode".into(), launch_mode: V2.into(),
        notice: "Native available models and variants. Explicit selection opens OpenCode's minimal interactive interface.".into(),
        ..Default::default()
    };
    for entry in entries.iter().take(2048) {
        let (Some(provider), Some(id)) = (
            entry.get("providerID").and_then(Value::as_str),
            entry.get("id").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !valid_id(provider) || !valid_id(id) || provider.contains('#') || id.contains('#') {
            continue;
        }
        if let Some(choice) = choice(entry, format!("{provider}/{id}"), true) {
            if !catalog.models.iter().any(|old| old.id == choice.id) {
                catalog.models.push(choice);
            }
        }
    }
    Ok(catalog)
}

fn parse_v1(output: &str, variants_supported: bool) -> Result<ModelCatalog, String> {
    let mut catalog = ModelCatalog {
        provider: "opencode".into(),
        launch_mode: if variants_supported { V1 } else { V1_NO_VARIANT }.into(),
        notice: if variants_supported { "Native available models and variants." } else { "This OpenCode version has no interactive variant flag; model selection remains available." }.into(),
        ..Default::default()
    };
    // Native v1 verbose output is an ID line followed by a pretty JSON object.
    // Streaming JSON boundaries avoid braces in strings or private metadata
    // becoming parser delimiters. Only whitelisted fields leave this function.
    let mut remaining = output.trim();
    while !remaining.is_empty() && catalog.models.len() < 2048 {
        let (id, after_id) = remaining
            .split_once('\n')
            .ok_or("OpenCode returned an incomplete verbose model catalog")?;
        let id = id.trim();
        if !valid_id(id) || !id.contains('/') {
            return Err("OpenCode returned an unsupported verbose model catalog".into());
        }
        let mut stream = serde_json::Deserializer::from_str(after_id).into_iter::<Value>();
        let value = stream
            .next()
            .and_then(Result::ok)
            .ok_or("OpenCode returned an unsupported verbose model catalog")?;
        remaining = after_id[stream.byte_offset()..].trim_start();
        if let Some(choice) = choice(&value, id.into(), variants_supported) {
            if !catalog.models.iter().any(|old| old.id == choice.id) {
                catalog.models.push(choice);
            }
        }
    }
    Ok(catalog)
}

/// Preserve the session and initial prompt while honoring the discovered CLI.
pub(crate) fn selection_argv_for_mode(
    argv: &[String],
    model: Option<&str>,
    effort: Option<&str>,
    mode: &str,
) -> Result<Vec<String>, String> {
    if argv.is_empty() {
        return Err("OpenCode launch command is empty".into());
    }
    if model.is_some_and(|id| !valid_id(id))
        || effort.is_some_and(|id| !valid_id(id) || id.contains('#'))
    {
        return Err("Invalid OpenCode model or variant ID".into());
    }
    if model.is_none() && effort.is_none() {
        return Ok(argv.to_vec());
    }
    let (model, embedded_variant) = model.map_or((None, None), |model| {
        model
            .split_once('#')
            .map_or((Some(model), None), |(base, variant)| {
                (Some(base), Some(variant))
            })
    });
    let effort = effort.or(embedded_variant);
    if effort.is_some_and(|id| !valid_id(id) || id.contains('#')) {
        return Err("Invalid OpenCode variant ID".into());
    }
    if effort.is_some() && model.is_none() {
        return Err("Choose an OpenCode model before its variant".into());
    }
    let v2 = match mode {
        V2 => true,
        V1 => false,
        "" => return Err("Open the model picker and wait for OpenCode interface discovery before selecting a model".into()),
        V1_NO_VARIANT if effort.is_none() => false,
        V1_NO_VARIANT => {
            return Err("This OpenCode version cannot select an interactive variant".into())
        }
        _ => return Err("Unknown OpenCode model selection interface".into()),
    };
    let mut output = vec![argv[0].clone()];
    let mut index = 1;
    if argv.get(index).is_some_and(|arg| arg == "mini") {
        if !v2 {
            return Err("OpenCode mini requires its version 2 model catalog".into());
        }
        index += 1;
    }
    if v2 {
        output.push("mini".into());
    }
    if let Some(model) = model {
        if !valid_id(model) {
            return Err("Invalid OpenCode model ID".into());
        }
        let selected = if v2 {
            effort.map_or_else(|| model.to_owned(), |variant| format!("{model}#{variant}"))
        } else {
            model.into()
        };
        output.extend(["--model".into(), selected]);
    }
    if !v2 {
        if let Some(effort) = effort {
            output.extend(["--variant".into(), effort.into()]);
        }
    }
    while index < argv.len() {
        let arg = &argv[index];
        if matches!(arg.as_str(), "--model" | "-m" | "--variant") {
            if index + 1 >= argv.len() {
                return Err("OpenCode selection flag has no value".into());
            }
            index += 2;
        } else if arg.starts_with("--model=") || arg.starts_with("--variant=") {
            index += 1;
        } else if matches!(
            arg.as_str(),
            "--session"
                | "-s"
                | "--prompt"
                | "--server"
                | "--agent"
                | "--log-level"
                | "--replay-limit"
        ) {
            let Some(value) = argv.get(index + 1) else {
                return Err("OpenCode launch flag has no value".into());
            };
            output.extend([arg.clone(), value.clone()]);
            index += 2;
        } else if arg.starts_with('-') && arg != "--" {
            output.push(arg.clone());
            index += 1;
        } else {
            // Converting an unrelated command/directory into a prompt would
            // change its meaning. Overwatch's native commands use named flags.
            return Err("Cannot apply OpenCode model selection to this launch command".into());
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_v2_catalog_filters_models_and_keeps_only_public_metadata() {
        let value = serde_json::json!({"location":{"directory":"/work/project"},"data":[
            {"providerID":"openai","id":"reasoner","name":"Reasoner\u{1b}","enabled":true,"variants":[{"id":"low"},{"id":"deep-custom"}],"headers":{"Authorization":"PRIVATE"}},
            {"providerID":"custom","id":"disabled","enabled":false},
            {"providerID":"custom","id":"image","type":"image"},
            {"providerID":"custom","id":"plain","variants":[]}
        ]});
        let catalog = parse_v2(&value).unwrap();
        assert_eq!(catalog.launch_mode, V2);
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].id, "openai/reasoner");
        assert_eq!(catalog.models[0].label, "Reasoner");
        assert_eq!(catalog.models[0].efforts[1].id, "deep-custom");
        assert_eq!(catalog.models[1].effort_support, EffortSupport::Unsupported);
        assert!(!serde_json::to_string(&catalog).unwrap().contains("PRIVATE"));
        assert_eq!(parse_v2(&value["data"]).unwrap(), catalog);
    }

    #[test]
    fn opencode_v1_verbose_json_boundaries_and_variant_capabilities() {
        let output = "custom/model\n{\n\"name\":\"Custom { model }\",\n\"variants\":{\"high\":{},\"hidden\":{\"disabled\":true}}}\ncustom/other\n{\"name\":\"Other\",\"variants\":{}}\n";
        let catalog = parse_v1(output, true).unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].efforts.len(), 1);
        assert_eq!(
            parse_v1(output, false).unwrap().models[0].effort_support,
            EffortSupport::Unsupported
        );
        assert_eq!(major_version("opencode v2.2.6\n"), Some(2));
        assert_eq!(major_version("1.2.3\n"), Some(1));
        assert_eq!(major_version("some unrelated output"), None);
    }

    #[test]
    fn opencode_model_selection_honors_version_session_and_literal_prompt() {
        let argv = [
            "opencode",
            "--session",
            "session-id",
            "--model",
            "old/old",
            "--variant",
            "low",
            "--prompt",
            "--model",
        ]
        .map(String::from);
        let v2 = selection_argv_for_mode(&argv, Some("custom/new"), Some("deep"), V2).unwrap();
        assert_eq!(
            v2,
            [
                "opencode",
                "mini",
                "--model",
                "custom/new#deep",
                "--session",
                "session-id",
                "--prompt",
                "--model"
            ]
        );
        assert_eq!(
            selection_argv_for_mode(&v2, Some("custom/new"), Some("deep"), V2).unwrap(),
            v2
        );
        let v1 = selection_argv_for_mode(&argv, Some("custom/new"), Some("high"), V1).unwrap();
        assert_eq!(v1[1..5], ["--model", "custom/new", "--variant", "high"]);
        assert!(
            selection_argv_for_mode(&argv, Some("custom/new"), Some("high"), V1_NO_VARIANT)
                .is_err()
        );
        assert!(selection_argv_for_mode(&argv, None, Some("high"), V2).is_err());
        let initial = ["opencode", "--prompt=literal --model text"].map(String::from);
        assert_eq!(
            selection_argv_for_mode(&initial, Some("custom/new#deep"), None, V2).unwrap(),
            [
                "opencode",
                "mini",
                "--model",
                "custom/new#deep",
                "--prompt=literal --model text"
            ]
        );
        let fallback = unavailable(V2, "Discovery unavailable".into());
        assert_eq!(fallback.launch_mode, V2);
        assert!(fallback.models.is_empty());
    }
}
