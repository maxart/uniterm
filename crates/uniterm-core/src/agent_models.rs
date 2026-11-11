//! Provider-neutral, bounded picker metadata. Discovery and launch syntax live in the server.
use serde::{Deserialize, Serialize};

/// Missing capability metadata must not be mistaken for supported effort controls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffortSupport {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

/// An advertised native effort or variant, with its exact launch identifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortChoice {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// Display names are never substituted for the provider's canonical launch ID.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub efforts: Vec<EffortChoice>,
    pub default_effort: Option<String>,
    pub effort_support: EffortSupport,
}

/// An on-demand snapshot, including cached/unavailable provenance for the user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub provider: String,
    pub models: Vec<ModelChoice>,
    pub notice: String,
    /// Opaque provider adapter capability needed to reproduce launch and resume.
    pub launch_mode: String,
}

impl ModelCatalog {
    /// Resolve a known native ID, label or alias without rewriting custom IDs.
    pub fn find(&self, value: &str) -> Option<&ModelChoice> {
        self.models
            .iter()
            .find(|model| model.id == value)
            .or_else(|| {
                self.models.iter().find(|model| {
                    model.id.eq_ignore_ascii_case(value)
                        || model.label.eq_ignore_ascii_case(value)
                        || model
                            .aliases
                            .iter()
                            .any(|alias| alias.eq_ignore_ascii_case(value))
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn friendly_names_resolve_but_custom_ids_are_not_rewritten() {
        let catalog = ModelCatalog {
            models: vec![ModelChoice {
                id: "gpt-6-luna".into(),
                label: "GPT-6-Luna".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(catalog.find("GPT-6-Luna").unwrap().id, "gpt-6-luna");
        assert!(catalog.find("Custom/CASE-sensitive").is_none());
    }
}
