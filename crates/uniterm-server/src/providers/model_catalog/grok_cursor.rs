//! Grok cache capabilities and native Grok/Cursor lists keep model IDs authoritative.
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
};
use uniterm_core::{
    agent_detail::bounded_text,
    agent_models::{EffortChoice, EffortSupport, ModelCatalog, ModelChoice},
};

const CACHE_LIMIT: u64 = 2 * 1024 * 1024;

pub(super) async fn discover(
    provider: &str,
    command: &str,
    cwd: &Path,
) -> Result<ModelCatalog, String> {
    if provider == "grok" {
        let path = std::env::var_os("GROK_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok")))
            .map(|home| home.join("models_cache.json"));
        if let Some(path) = path {
            if let Ok(Some(catalog)) = tokio::task::spawn_blocking(move || read_cache(&path)).await
            {
                return Ok(catalog);
            }
        }
    }
    let first = if provider == "cursor" {
        "--list-models"
    } else {
        "models"
    };
    if let Ok(output) = super::run(command, &[first], cwd, None).await {
        if let Ok(catalog) = parse_native(provider, &output) {
            return Ok(catalog);
        }
    }
    if provider == "cursor" {
        if let Ok(output) = super::run(command, &["models"], cwd, None).await {
            if let Ok(catalog) = parse_native(provider, &output) {
                return Ok(catalog);
            }
        }
    }
    Err("The harness did not return a recognizable model list; open its native model picker and check sign-in".into())
}

// Deliberately ignore credential-bearing siblings of `info` in Grok's cache.
#[derive(Deserialize)]
struct Cache {
    #[serde(default)]
    fetched_at: String,
    models: BTreeMap<String, CachedModel>,
}
#[derive(Deserialize)]
struct CachedModel {
    info: Info,
}
#[derive(Deserialize)]
struct Info {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    hidden: Option<bool>,
    supports_reasoning_effort: Option<bool>,
    reasoning_efforts: Option<Vec<CachedEffort>>,
}
#[derive(Deserialize)]
struct CachedEffort {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default: bool,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ':' | '[' | ']' | '=' | ',' | '&')
        })
        && value.as_bytes()[0].is_ascii_alphanumeric()
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
    parse_cache(&bytes).ok()
}

fn parse_cache(bytes: &[u8]) -> Result<ModelCatalog, String> {
    let cache: Cache =
        serde_json::from_slice(bytes).map_err(|_| "Grok's cached model metadata is unreadable")?;
    let mut models = Vec::new();
    for (id, entry) in cache.models.into_iter().take(256) {
        let info = entry.info;
        if !valid_id(&id) || info.hidden == Some(true) {
            continue;
        }
        let mut efforts: Vec<EffortChoice> = Vec::new();
        let mut default_effort = None;
        let empty_levels = info.reasoning_efforts.as_ref().is_some_and(Vec::is_empty);
        if info.supports_reasoning_effort != Some(false) {
            for effort in info.reasoning_efforts.into_iter().flatten().take(32) {
                if !valid_id(&effort.id) || efforts.iter().any(|e| e.id == effort.id) {
                    continue;
                }
                if effort.default && default_effort.is_none() {
                    default_effort = Some(effort.id.clone());
                }
                efforts.push(EffortChoice {
                    label: bounded_text(
                        if effort.label.trim().is_empty() {
                            &effort.id
                        } else {
                            &effort.label
                        },
                        80,
                    ),
                    description: bounded_text(&effort.description, 240),
                    id: effort.id,
                });
            }
        }
        let effort_support = if !efforts.is_empty() {
            EffortSupport::Supported
        } else if info.supports_reasoning_effort == Some(false) || empty_levels {
            EffortSupport::Unsupported
        } else {
            EffortSupport::Unknown
        };
        models.push(ModelChoice {
            label: bounded_text(
                info.name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(&id),
                160,
            ),
            description: bounded_text(info.description.as_deref().unwrap_or_default(), 320),
            id,
            efforts,
            default_effort,
            effort_support,
            ..Default::default()
        });
    }
    if models.is_empty() {
        return Err("Grok's cached catalog contains no visible models".into());
    }
    let notice = if cache.fetched_at.is_empty() {
        "Cached Grok model catalog; availability may have changed".into()
    } else {
        format!(
            "Cached Grok model catalog from {}; availability may have changed",
            bounded_text(&cache.fetched_at, 80)
        )
    };
    Ok(ModelCatalog {
        provider: "grok".into(),
        models,
        notice,
        ..Default::default()
    })
}

fn strip_controls(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let mut result = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if ch == '\n' || ch == '\t' || !ch.is_control() {
                result.push(ch);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(ch) = chars.next() {
                    if ch == '\u{7}' || (ch == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    result
}

fn parse_native(provider: &str, output: &str) -> Result<ModelCatalog, String> {
    let output = strip_controls(output);
    let mut in_models = false;
    let mut models: Vec<ModelChoice> = Vec::new();
    for line in output.lines().take(4096) {
        let line = line.trim();
        if line.to_ascii_lowercase().starts_with("available models") {
            in_models = true;
            continue;
        }
        if !in_models || line.is_empty() {
            continue;
        }
        if line.starts_with("Use ")
            || line.starts_with("Run ")
            || line.starts_with("Default model:")
            || line.starts_with("--")
        {
            continue;
        }
        let line = line.trim_start_matches(['*', '-', '>', '•']).trim();
        let line = line
            .strip_suffix("(default)")
            .or_else(|| line.strip_suffix("(current)"))
            .unwrap_or(line)
            .trim();
        let (id, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let id = id.trim_end_matches(':');
        if !valid_id(id)
            || matches!(
                id.to_ascii_lowercase().as_str(),
                "id" | "model" | "name" | "models"
            )
        {
            continue;
        }
        let rest = rest
            .trim()
            .trim_start_matches(['-', '\u{2013}', '\u{2014}', ':', '|'])
            .trim();
        if models.iter().any(|model| model.id == id) {
            continue;
        }
        models.push(ModelChoice {
            id: id.into(),
            label: bounded_text(if rest.is_empty() { id } else { rest }, 160),
            effort_support: if provider == "cursor" {
                EffortSupport::Unsupported
            } else {
                EffortSupport::Unknown
            },
            ..Default::default()
        });
        if models.len() == 256 {
            break;
        }
    }
    if models.is_empty() {
        return Err("Native output contained no recognizable model IDs".into());
    }
    Ok(ModelCatalog {
        provider: provider.into(),
        models,
        notice: if provider == "cursor" {
            "Native model IDs reported by Cursor. Choose a native model variant; no separate effort flag".into()
        } else {
            "Native model IDs reported by the harness; separate reasoning levels were not reported"
                .into()
        },
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grok_cache_projects_capabilities_without_credential_siblings() {
        let bytes = serde_json::to_vec(&serde_json::json!({"fetched_at":"fixture-time","models":{
            "grok-fixture":{"api_key":"SECRET-API-KEY","api_base_url":"https://secret.example", "info":{
                "name":"Grok fixture","description":null,"supports_reasoning_effort":true,"reasoning_efforts":[
                    {"id":"high","label":"High","description":"Thorough","default":true},
                    {"id":"low","label":"Low"}
                ]}},
            "hidden":{"info":{"hidden":true}},
            "no-effort":{"info":{"supports_reasoning_effort":false}},
            "unknown":{"info":{}}
        }})).unwrap();
        let catalog = parse_cache(&bytes).unwrap();
        assert_eq!(catalog.models.len(), 3);
        let model = &catalog.models[0];
        assert_eq!(model.id, "grok-fixture");
        assert_eq!(model.label, "Grok fixture");
        assert_eq!(model.default_effort.as_deref(), Some("high"));
        assert_eq!(model.efforts.len(), 2);
        assert_eq!(catalog.models[1].effort_support, EffortSupport::Unsupported);
        assert_eq!(catalog.models[2].effort_support, EffortSupport::Unknown);
        assert!(catalog.notice.contains("Cached"));
        let encoded = serde_json::to_string(&catalog).unwrap();
        assert!(!encoded.contains("SECRET") && !encoded.contains("secret.example"));
    }

    #[test]
    fn native_lists_preserve_exact_variant_ids_and_do_not_invent_efforts() {
        let catalog = parse_native("cursor", "\u{1b}[1mAvailable models\u{1b}[0m\n  auto - Auto\n  model-fixture-high - Model fixture (High)\nUse --model to select\n").unwrap();
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[1].id, "model-fixture-high");
        assert_eq!(catalog.models[1].label, "Model fixture (High)");
        assert!(catalog
            .models
            .iter()
            .all(|m| m.efforts.is_empty() && m.effort_support == EffortSupport::Unsupported));
        assert!(parse_native("grok", "You are not authenticated.").is_err());
        assert!(parse_native(
            "grok",
            "Available models:\n--effort/--reasoning-effort: high\n"
        )
        .is_err());
    }

    #[test]
    fn grok_native_bullets_and_default_markers_are_not_model_labels() {
        let catalog = parse_native("grok", "You are not authenticated.\n\nDefault model: grok-4.6\n\nAvailable models:\n  * grok-4.6 (default)\n  - grok-4.5\n").unwrap();
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["grok-4.6", "grok-4.5"]
        );
        assert_eq!(catalog.models[0].label, "grok-4.6");
    }
}
