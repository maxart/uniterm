//! Overwatch uses the common searchable list and the provider's native chat Pane.
use crate::actions::RenameTarget;
use crate::menu::MenuState;
use crate::palette::{Palette, PalettePurpose};
use crate::task::LineInput;
use uniterm_core::agent_models::{EffortSupport, ModelCatalog, ModelChoice};
use uniterm_core::{menu::MenuAction, overwatch::State};
use uniterm_proto::{encode_frame, ClientMessage};

pub(crate) fn controls(state: &State, providers: &[String]) -> MenuState {
    let mut rows = vec![(
        format!(
            "{} Overwatch",
            if state.enabled { "Turn OFF" } else { "Turn ON" }
        ),
        MenuAction::OverwatchSet(!state.enabled),
    )];
    if state.enabled {
        rows.push(("Open chat".into(), MenuAction::OverwatchChat));
        rows.push((
            if state.message.is_empty() {
                "Retry recovery".into()
            } else {
                format!("Retry recovery: {}", state.message)
            },
            MenuAction::OverwatchRetry,
        ));
    }
    if !state.provider.is_empty() {
        rows.push((
            format!(
                "Model: {} ({})",
                state.model.as_deref().unwrap_or("Provider default"),
                if state.enabled {
                    "restart chat"
                } else {
                    "edit"
                }
            ),
            MenuAction::OverwatchModel,
        ));
        rows.push((
            format!(
                "Reasoning effort: {}",
                state.effort.as_deref().unwrap_or("Harness default")
            ),
            MenuAction::OverwatchEffort,
        ));
    }
    if !state.enabled {
        rows.push((
            if state.manage {
                "Use read-only Workspace access".into()
            } else {
                "Grant Workspace management and private content access".into()
            },
            MenuAction::OverwatchManage(!state.manage),
        ));
    }
    for provider in providers {
        if let Some(index) = uniterm_core::agent::PROVIDERS
            .iter()
            .position(|p| p.id == provider)
        {
            rows.push((
                format!(
                    "{}Provider: {provider} (share Workspace metadata)",
                    if state.provider == *provider {
                        "* "
                    } else {
                        ""
                    }
                ),
                MenuAction::OverwatchChoose(index),
            ));
        }
    }
    let mut menu = MenuState::custom(
        if state.enabled {
            format!(
                "Overwatch ON - {:?} | Changes restart chat and interrupt reply",
                state.phase
            )
        } else {
            format!(
                "Overwatch {} - {:?} - {} / {} | {}",
                if state.enabled { "ON" } else { "OFF" },
                state.phase,
                state.provider,
                state.model.as_deref().unwrap_or("Provider default"),
                state.message
            )
        },
        rows,
    );
    menu.palette.as_mut().unwrap().purpose = PalettePurpose::OverwatchControls;
    menu
}

pub(crate) fn choose_provider(state: &State, index: usize) -> Option<uniterm_proto::ClientMessage> {
    let provider = uniterm_core::agent::PROVIDERS.get(index)?;
    Some(uniterm_proto::ClientMessage::Overwatch {
        operation: uniterm_core::overwatch::Operation::Configure {
            provider: provider.id.into(),
            model: (state.provider == provider.id)
                .then(|| state.model.clone())
                .flatten(),
            project: state.project,
            manage: state.manage,
        },
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Models,
    Efforts,
    CustomModel,
    CustomEffort,
}

struct Selection {
    request: u64,
    provider: String,
    enabled: bool,
    current_model: Option<String>,
    current_effort: Option<String>,
    pending_model: Option<String>,
    catalog: ModelCatalog,
    stage: Stage,
    loading: bool,
    choose_model_first: bool,
}

impl Selection {
    fn model(&self) -> Option<&ModelChoice> {
        let id = self.pending_model.as_deref()?;
        self.catalog.find(id)
    }

    fn menu(&self) -> MenuState {
        let mut rows = Vec::new();
        let mut selected = MenuAction::OverwatchModelDefault;
        let notice;
        let title;
        let mut add = |label: String, search: String, description: String, action| {
            rows.push((label, search, description, action))
        };
        if self.stage == Stage::Models {
            title = format!("Overwatch models - {}", self.provider);
            notice = format!(
                "{}{}{}",
                if self.choose_model_first {
                    "Choose a model before setting effort. "
                } else {
                    ""
                },
                if self.loading {
                    "Loading models... "
                } else {
                    ""
                },
                self.catalog.notice
            );
            add(
                "Harness default (model and effort)".into(),
                "Harness provider default".into(),
                "Use the harness's configured model and reasoning effort.".into(),
                MenuAction::OverwatchModelDefault,
            );
            for (index, model) in self.catalog.models.iter().enumerate() {
                let current = self
                    .current_model
                    .as_ref()
                    .is_some_and(|id| *id == model.id || model.aliases.contains(id));
                if current {
                    selected = MenuAction::OverwatchModelChoice(index);
                }
                let label = format!(
                    "{}{} [{}]",
                    if current { "* " } else { "" },
                    model.label,
                    model.id
                );
                let search = format!("{label} {} {}", model.aliases.join(" "), model.description);
                add(
                    label,
                    search,
                    model.description.clone(),
                    MenuAction::OverwatchModelChoice(index),
                );
            }
            add(
                "Custom model ID... (advanced)".into(),
                "Custom model advanced exact ID".into(),
                "Enter the exact model ID accepted by this harness. Select effort before applying."
                    .into(),
                MenuAction::OverwatchModelCustom,
            );
            if self.current_model.is_some()
                && !self.catalog.models.iter().any(|model| {
                    self.current_model
                        .as_ref()
                        .is_some_and(|id| *id == model.id || model.aliases.contains(id))
                })
            {
                selected = MenuAction::OverwatchModelCustom;
            }
        } else {
            let model = self.model();
            let label = model
                .map(|m| m.label.as_str())
                .unwrap_or_else(|| self.pending_model.as_deref().unwrap_or("Harness default"));
            title = format!(
                "Overwatch effort{} - {label}",
                if self.enabled { " (restarts chat)" } else { "" }
            );
            let support = model
                .map(|m| m.effort_support)
                .unwrap_or(EffortSupport::Unknown);
            let capability = match support {
                EffortSupport::Supported => {
                    "Choose reasoning effort; applies model and effort together."
                }
                EffortSupport::Unsupported => {
                    "Separate effort is not supported; use the model or its listed variant."
                }
                EffortSupport::Unknown => {
                    "Supported efforts are unknown. Default leaves control with the harness."
                }
            };
            notice = format!(
                "{capability} {}{}",
                if self.loading {
                    "Loading models... "
                } else {
                    ""
                },
                self.catalog.notice
            );
            selected = MenuAction::OverwatchEffortDefault;
            let default = model
                .and_then(|m| m.default_effort.as_ref())
                .and_then(|id| model?.efforts.iter().find(|e| &e.id == id))
                .map(|effort| format!(" (model default: {})", effort.label))
                .unwrap_or_default();
            add(
                format!("Harness default{default}"),
                "Harness provider default reasoning effort".into(),
                capability.into(),
                MenuAction::OverwatchEffortDefault,
            );
            if let Some(model) = model.filter(|_| support == EffortSupport::Supported) {
                for (index, effort) in model.efforts.iter().enumerate() {
                    let same_model = self
                        .current_model
                        .as_ref()
                        .is_some_and(|id| *id == model.id || model.aliases.contains(id));
                    let current = same_model && self.current_effort.as_ref() == Some(&effort.id);
                    if current {
                        selected = MenuAction::OverwatchEffortChoice(index);
                    }
                    let label = format!(
                        "{}{} [{}]",
                        if current { "* " } else { "" },
                        effort.label,
                        effort.id
                    );
                    add(
                        label.clone(),
                        format!("{label} {}", effort.description),
                        effort.description.clone(),
                        MenuAction::OverwatchEffortChoice(index),
                    );
                }
            }
            if support == EffortSupport::Unknown {
                add(
                    "Custom effort ID... (advanced)".into(),
                    "Custom reasoning effort advanced exact ID".into(),
                    "Enter a provider-specific effort only if this harness and model support it."
                        .into(),
                    MenuAction::OverwatchEffortCustom,
                );
            }
            add(
                "Back to models".into(),
                "Back models cancel pending choice".into(),
                "Nothing is applied until you choose an effort or harness default.".into(),
                MenuAction::OverwatchModelsBack,
            );
        }
        add(
            "Refresh available models".into(),
            "Refresh retry reload models catalog".into(),
            "Request the installed harness's current model catalog.".into(),
            MenuAction::OverwatchModelsRefresh,
        );
        let mut palette = Palette::custom_with_details(title, notice, rows);
        palette.purpose = PalettePurpose::OverwatchPicker;
        let mut menu = MenuState::open(0);
        menu.sel = palette
            .items
            .iter()
            .position(|item| item.action == selected)
            .unwrap_or(0);
        menu.palette = Some(palette);
        menu
    }
}

/// One attachment owns the pending model/effort choice and its reply generation.
/// Closing the menu invalidates replies through its explicit surface identity.
#[derive(Default)]
pub(crate) struct Picker {
    serial: u64,
    active: Option<Selection>,
}

impl Picker {
    fn request(&mut self, out: &mut Vec<u8>) {
        self.serial = self.serial.wrapping_add(1);
        if let Some(active) = &mut self.active {
            active.request = self.serial;
            active.loading = true;
            out.extend(encode_frame(&ClientMessage::Overwatch {
                operation: uniterm_core::overwatch::Operation::Models {
                    request: self.serial,
                },
            }));
        }
    }

    /// Mouse and keyboard both enter this semantic action handler.
    pub(crate) fn action(
        &mut self,
        action: MenuAction,
        state: &State,
        menu: &mut Option<MenuState>,
        rename: &mut Option<(LineInput, RenameTarget)>,
        out: &mut Vec<u8>,
    ) {
        *menu = None;
        if matches!(
            action,
            MenuAction::OverwatchModel | MenuAction::OverwatchEffort
        ) {
            let effort = action == MenuAction::OverwatchEffort && state.model.is_some();
            self.active = Some(Selection {
                request: 0,
                provider: state.provider.clone(),
                enabled: state.enabled,
                current_model: state.model.clone(),
                current_effort: state.effort.clone(),
                pending_model: state.model.clone(),
                catalog: ModelCatalog {
                    provider: state.provider.clone(),
                    ..Default::default()
                },
                stage: if effort {
                    Stage::Efforts
                } else {
                    Stage::Models
                },
                loading: true,
                choose_model_first: action == MenuAction::OverwatchEffort && !effort,
            });
            self.request(out);
        } else {
            let Some(active) = self
                .active
                .as_mut()
                .filter(|a| a.provider == state.provider)
            else {
                return;
            };
            match action {
                MenuAction::OverwatchModelChoice(index) => {
                    let Some(model) = active.catalog.models.get(index) else {
                        return;
                    };
                    active.pending_model = Some(model.id.clone());
                    active.stage = Stage::Efforts;
                }
                MenuAction::OverwatchModelDefault => {
                    active.pending_model = None;
                    self.apply(None, out);
                    return;
                }
                MenuAction::OverwatchModelCustom => {
                    active.stage = Stage::CustomModel;
                    *rename = Some((
                        LineInput::with_text(
                            "Custom Overwatch model ID (advanced; blank: default)",
                            active.pending_model.clone().unwrap_or_default(),
                        ),
                        RenameTarget::OverwatchModel {
                            request: active.request,
                        },
                    ));
                    return;
                }
                MenuAction::OverwatchEffortCustom => {
                    active.stage = Stage::CustomEffort;
                    *rename = Some((
                        LineInput::with_text(
                            "Custom Overwatch effort ID (advanced; blank: default)",
                            active.current_effort.clone().unwrap_or_default(),
                        ),
                        RenameTarget::OverwatchEffort {
                            request: active.request,
                        },
                    ));
                    return;
                }
                MenuAction::OverwatchEffortChoice(index) => {
                    let Some(effort) = active.model().and_then(|model| model.efforts.get(index))
                    else {
                        return;
                    };
                    let effort = effort.id.clone();
                    self.apply(Some(effort), out);
                    return;
                }
                MenuAction::OverwatchEffortDefault => {
                    self.apply(None, out);
                    return;
                }
                MenuAction::OverwatchModelsBack => {
                    active.stage = Stage::Models;
                }
                MenuAction::OverwatchModelsRefresh => self.request(out),
                _ => return,
            }
        }
        if let Some(active) = &self.active {
            *menu = Some(active.menu());
        }
    }

    fn apply(&mut self, effort: Option<String>, out: &mut Vec<u8>) {
        let Some(active) = self.active.take() else {
            return;
        };
        out.extend(encode_frame(&ClientMessage::Overwatch {
            operation: uniterm_core::overwatch::Operation::Selection {
                provider: active.provider,
                model: active.pending_model,
                effort,
            },
        }));
    }

    /// Custom values remain exact; the provider owns validation and aliases.
    pub(crate) fn submit_custom(
        &mut self,
        request: u64,
        effort: bool,
        text: String,
        state: &State,
        menu: &mut Option<MenuState>,
        out: &mut Vec<u8>,
    ) {
        let stage = if effort {
            Stage::CustomEffort
        } else {
            Stage::CustomModel
        };
        let Some(active) = self
            .active
            .as_mut()
            .filter(|a| a.request == request && a.provider == state.provider && a.stage == stage)
        else {
            return;
        };
        let value = (!text.is_empty()).then_some(text);
        if effort {
            self.apply(value, out);
        } else {
            active.pending_model = value;
            if active.pending_model.is_none() {
                self.apply(None, out);
            } else {
                active.stage = Stage::Efforts;
                *menu = Some(active.menu());
            }
        }
    }

    /// A closed picker, a replacement request or another provider cannot consume this reply.
    pub(crate) fn receive(
        &mut self,
        request: u64,
        catalog: ModelCatalog,
        state: &State,
        menu: &mut Option<MenuState>,
    ) -> bool {
        if !menu.as_ref().is_some_and(MenuState::is_overwatch_picker) {
            return false;
        }
        let Some(active) = self.active.as_mut().filter(|a| {
            a.request == request && a.provider == state.provider && a.provider == catalog.provider
        }) else {
            return false;
        };
        let initial = active.catalog.models.is_empty() && active.loading;
        let selected = menu
            .as_ref()
            .and_then(|menu| menu.palette.as_ref()?.items.get(menu.sel))
            .map(|item| item.action);
        let selected_id = match selected {
            Some(MenuAction::OverwatchModelChoice(index)) => active
                .catalog
                .models
                .get(index)
                .map(|model| model.id.clone()),
            Some(MenuAction::OverwatchEffortChoice(index)) => active
                .model()
                .and_then(|model| model.efforts.get(index))
                .map(|effort| effort.id.clone()),
            _ => None,
        };
        active.catalog = catalog;
        active.loading = false;
        let mut next = active.menu();
        let initial_action = next.action();
        if let (Some(existing), Some(incoming)) = (menu.as_mut(), next.palette.take()) {
            if let Some(current) = &mut existing.palette {
                current.refresh_custom(incoming, &mut existing.sel);
                let action = selected_id.and_then(|id| match selected {
                    Some(MenuAction::OverwatchModelChoice(_)) => active
                        .catalog
                        .models
                        .iter()
                        .position(|model| model.id == id)
                        .map(MenuAction::OverwatchModelChoice),
                    Some(MenuAction::OverwatchEffortChoice(_)) => active
                        .model()?
                        .efforts
                        .iter()
                        .position(|effort| effort.id == id)
                        .map(MenuAction::OverwatchEffortChoice),
                    _ => None,
                });
                if let Some(action) = action {
                    existing.sel = current
                        .items
                        .iter()
                        .position(|item| item.action == action)
                        .unwrap_or(0);
                } else if initial {
                    existing.sel = current
                        .items
                        .iter()
                        .position(|item| item.action == initial_action)
                        .unwrap_or(0);
                }
            }
        }
        true
    }

    /// Provider changes invalidate both the displayed catalog and custom inputs.
    pub(crate) fn state_changed(
        &mut self,
        state: &State,
        menu: &mut Option<MenuState>,
        rename: &mut Option<(LineInput, RenameTarget)>,
    ) -> bool {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.provider == state.provider)
        {
            return false;
        }
        self.active = None;
        let mut changed = false;
        if menu.as_ref().is_some_and(MenuState::is_overwatch_picker) {
            *menu = None;
            changed = true;
        }
        if rename.as_ref().is_some_and(|(_, target)| {
            matches!(
                target,
                RenameTarget::OverwatchModel { .. } | RenameTarget::OverwatchEffort { .. }
            )
        }) {
            *rename = None;
            changed = true;
        }
        changed
    }
}

/// A non-opening state update can refresh only an already open Overwatch menu.
pub(crate) fn update(
    menu: &mut Option<MenuState>,
    state: &State,
    providers: &[String],
    open: bool,
) -> bool {
    if menu.as_ref().is_some_and(MenuState::is_overwatch_picker) {
        return false;
    }
    if !open && !menu.as_ref().is_some_and(MenuState::is_overwatch) {
        return false;
    }
    let mut next = controls(state, providers);
    if let Some(existing) = menu.as_mut().filter(|menu| menu.is_overwatch()) {
        if let (Some(current), Some(incoming)) = (existing.palette.as_mut(), next.palette.take()) {
            current.refresh_custom(incoming, &mut existing.sel);
            return true;
        }
    }
    *menu = Some(next);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniterm_core::agent_models::EffortChoice;
    use uniterm_core::overwatch::Operation;
    use uniterm_proto::FrameDecoder;

    fn catalog(provider: &str, support: EffortSupport) -> ModelCatalog {
        ModelCatalog {
            provider: provider.into(),
            notice: "Cached catalog; refresh to check availability".into(),
            models: vec![ModelChoice {
                id: "exact/model-id".into(),
                label: "Friendly Model 界".into(),
                description: "Useful for careful engineering".into(),
                aliases: vec!["friendly-alias".into()],
                efforts: if support == EffortSupport::Supported {
                    vec![
                        EffortChoice {
                            id: "high".into(),
                            label: "High".into(),
                            description: "Thorough reasoning".into(),
                        },
                        EffortChoice {
                            id: "xhigh".into(),
                            label: "Extra High".into(),
                            description: "Hardest tasks".into(),
                        },
                    ]
                } else {
                    vec![]
                },
                default_effort: Some("high".into()),
                effort_support: support,
            }],
            ..Default::default()
        }
    }

    fn messages(out: &mut Vec<u8>) -> Vec<Operation> {
        let mut decoder = FrameDecoder::new();
        decoder.push(out);
        out.clear();
        let mut operations = vec![];
        while let Some(message) = decoder.decode::<ClientMessage>().unwrap() {
            if let ClientMessage::Overwatch { operation } = message {
                operations.push(operation);
            }
        }
        operations
    }

    fn request(out: &mut Vec<u8>) -> u64 {
        let messages = messages(out);
        assert_eq!(messages.len(), 1);
        let Operation::Models { request } = messages[0] else {
            panic!("model list request")
        };
        request
    }

    fn text(menu: &Option<MenuState>) -> String {
        let menu = menu.as_ref().unwrap();
        let overlay = menu.palette.as_ref().unwrap().overlay(menu.sel, 30);
        format!("{}\n{}", overlay.title, overlay.lines.join("\n"))
    }

    #[test]
    fn picker_searches_friendly_names_ids_aliases_then_applies_exact_model_and_effort_once() {
        let state = State {
            provider: "grok".into(),
            model: Some("friendly-alias".into()),
            effort: Some("xhigh".into()),
            ..Default::default()
        };
        let (mut picker, mut menu, mut rename, mut out) = (Picker::default(), None, None, vec![]);
        picker.action(
            MenuAction::OverwatchModel,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        let generation = request(&mut out);
        assert!(picker.receive(
            generation,
            catalog("grok", EffortSupport::Supported),
            &state,
            &mut menu
        ));
        assert!(text(&menu).contains("Friendly Model 界 [exact/model-id]"));
        assert!(text(&menu).contains("Useful for careful engineering"));
        assert_eq!(
            menu.as_ref().unwrap().action(),
            MenuAction::OverwatchModelChoice(0)
        );
        for query in ["friendly", "exact/model-id", "friendly-alias"] {
            let mut found = menu.clone().unwrap();
            found
                .palette
                .as_mut()
                .unwrap()
                .handle(query.as_bytes(), &mut found.sel);
            assert_eq!(found.palette.as_ref().unwrap().items.len(), 1);
            assert_eq!(found.action(), MenuAction::OverwatchModelChoice(0));
        }
        let found = menu.as_ref().unwrap();
        for (cols, rows) in [(44, 16), (120, 30)] {
            let hits: Vec<_> = (1..=rows)
                .flat_map(|y| (1..=cols).map(move |x| (x, y)))
                .filter_map(|(x, y)| crate::menu::item_at(found, cols, rows, true, 1, x, y))
                .filter(|i| found.action_at(*i) == found.action())
                .collect();
            assert!(
                !hits.is_empty(),
                "selected model's rendered row remains clickable"
            );
            let ansi = crate::menu::render_menu(found, cols, rows, true, 1);
            assert!(!ansi.is_empty());
            assert_eq!(crate::menu::render_menu(found, cols, rows, true, 1), ansi);
        }
        picker.action(
            MenuAction::OverwatchModelChoice(0),
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        assert!(
            messages(&mut out).is_empty(),
            "model choice must not restart chat before effort is chosen"
        );
        assert!(text(&menu).contains("Extra High [xhigh]"));
        assert_eq!(
            menu.as_ref().unwrap().action(),
            MenuAction::OverwatchEffortChoice(1)
        );
        assert!(!update(&mut menu, &state, &["grok".into()], false));
        assert!(!update(&mut menu, &state, &["grok".into()], true));
        assert!(menu.as_ref().unwrap().is_overwatch_picker());
        picker.action(
            MenuAction::OverwatchEffortChoice(1),
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        assert_eq!(
            messages(&mut out),
            vec![Operation::Selection {
                provider: "grok".into(),
                model: Some("exact/model-id".into()),
                effort: Some("xhigh".into())
            }]
        );
        assert!(menu.is_none());
        assert!(picker.active.is_none());
    }

    #[test]
    fn picker_cancel_refresh_and_provider_change_reject_stale_catalogs() {
        let state = State {
            provider: "grok".into(),
            ..Default::default()
        };
        let (mut picker, mut menu, mut rename, mut out) = (Picker::default(), None, None, vec![]);
        picker.action(
            MenuAction::OverwatchModel,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        let old = request(&mut out);
        menu = None;
        assert!(!picker.receive(
            old,
            catalog("grok", EffortSupport::Supported),
            &state,
            &mut menu
        ));
        picker.action(
            MenuAction::OverwatchModel,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        let fresh = request(&mut out);
        assert_ne!(old, fresh);
        assert!(!picker.receive(
            old,
            catalog("grok", EffortSupport::Supported),
            &state,
            &mut menu
        ));
        assert!(picker.receive(
            fresh,
            catalog("grok", EffortSupport::Supported),
            &state,
            &mut menu
        ));
        picker.action(
            MenuAction::OverwatchModelChoice(0),
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        menu = None;
        assert!(
            messages(&mut out).is_empty(),
            "cancelling effort never applies the pending model"
        );
        assert!(!picker.receive(
            fresh,
            catalog("grok", EffortSupport::Supported),
            &state,
            &mut menu
        ));
        picker.action(
            MenuAction::OverwatchModel,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        let before_refresh = request(&mut out);
        picker.action(
            MenuAction::OverwatchModelsRefresh,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        let refresh = request(&mut out);
        assert!(!picker.receive(
            before_refresh,
            catalog("grok", EffortSupport::Supported),
            &state,
            &mut menu
        ));
        let different = State {
            provider: "cursor".into(),
            ..Default::default()
        };
        assert!(!picker.receive(
            refresh,
            catalog("grok", EffortSupport::Supported),
            &different,
            &mut menu
        ));
        assert!(picker.state_changed(&different, &mut menu, &mut rename));
        assert!(menu.is_none());
        assert!(messages(&mut out).is_empty());
        let mut similar = Some(MenuState::custom("Overwatch lookalike".into(), vec![]));
        assert!(!update(&mut similar, &state, &[], false));
    }

    #[test]
    fn picker_explains_unknown_and_unsupported_effort_and_preserves_custom_ids() {
        let state = State {
            provider: "cursor".into(),
            ..Default::default()
        };
        let (mut picker, mut menu, mut rename, mut out) = (Picker::default(), None, None, vec![]);
        picker.action(
            MenuAction::OverwatchEffort,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        assert!(text(&menu).contains("Choose a model before setting effort"));
        let generation = request(&mut out);
        assert!(picker.receive(
            generation,
            catalog("cursor", EffortSupport::Unsupported),
            &state,
            &mut menu
        ));
        picker.action(
            MenuAction::OverwatchModelChoice(0),
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        assert!(text(&menu).contains("Separate effort is not supported"));
        assert!(!text(&menu).contains("Custom effort"));
        picker.action(
            MenuAction::OverwatchEffortDefault,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        assert_eq!(
            messages(&mut out),
            vec![Operation::Selection {
                provider: "cursor".into(),
                model: Some("exact/model-id".into()),
                effort: None
            }]
        );

        picker.action(
            MenuAction::OverwatchModel,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        let generation = request(&mut out);
        assert!(picker.receive(
            generation,
            ModelCatalog {
                provider: "cursor".into(),
                notice: "Catalog unavailable; retry or use a custom ID".into(),
                ..Default::default()
            },
            &state,
            &mut menu
        ));
        assert!(text(&menu).contains("Catalog unavailable"));
        picker.action(
            MenuAction::OverwatchModelCustom,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        assert!(rename.is_some());
        let custom = "MyProvider/CASE-sensitive[fast=true]";
        picker.submit_custom(
            generation,
            false,
            custom.into(),
            &state,
            &mut menu,
            &mut out,
        );
        rename = None;
        assert!(messages(&mut out).is_empty());
        assert!(text(&menu).contains("Supported efforts are unknown"));
        picker.action(
            MenuAction::OverwatchEffortCustom,
            &state,
            &mut menu,
            &mut rename,
            &mut out,
        );
        picker.submit_custom(
            generation,
            true,
            "Deep-CUSTOM".into(),
            &state,
            &mut menu,
            &mut out,
        );
        assert_eq!(
            messages(&mut out),
            vec![Operation::Selection {
                provider: "cursor".into(),
                model: Some(custom.into()),
                effort: Some("Deep-CUSTOM".into())
            }]
        );
    }

    #[test]
    fn live_configuration_keeps_model_and_provider_actions_but_not_management() {
        let mut state = State {
            provider: "codex".into(),
            model: Some("chosen-model".into()),
            ..Default::default()
        };
        let menu = controls(&state, &["codex".into()]);
        let items = &menu.palette.as_ref().unwrap().items;
        assert!(items
            .iter()
            .any(|item| item.action == MenuAction::OverwatchModel));
        state.enabled = true;
        let menu = controls(&state, &["codex".into()]);
        assert!(menu
            .palette
            .as_ref()
            .unwrap()
            .items
            .iter()
            .any(|item| item.action == MenuAction::OverwatchModel));
        let items = &menu.palette.as_ref().unwrap().items;
        assert!(items
            .iter()
            .any(|item| matches!(item.action, MenuAction::OverwatchChoose(_))));
        assert!(!items
            .iter()
            .any(|item| matches!(item.action, MenuAction::OverwatchManage(_))));
        assert!(items
            .iter()
            .any(|item| item.action == MenuAction::OverwatchEffort));
    }

    #[test]
    fn provider_choice_preserves_access_scope_and_same_provider_model() {
        let state = State {
            enabled: true,
            provider: "codex".into(),
            model: Some("chosen-model".into()),
            manage: true,
            project: Some(uniterm_core::ProjectId(42)),
            ..Default::default()
        };
        for provider in ["codex", "claude"] {
            let index = uniterm_core::agent::PROVIDERS
                .iter()
                .position(|p| p.id == provider)
                .unwrap();
            let Some(uniterm_proto::ClientMessage::Overwatch { operation }) =
                choose_provider(&state, index)
            else {
                panic!("expected Overwatch configuration")
            };
            assert_eq!(
                operation,
                uniterm_core::overwatch::Operation::Configure {
                    provider: provider.into(),
                    model: (provider == "codex").then(|| "chosen-model".into()),
                    project: state.project,
                    manage: true,
                }
            );
        }
        assert!(choose_provider(&state, usize::MAX).is_none());
    }

    #[test]
    fn live_supervisor_updates_preserve_closed_and_unrelated_menus() {
        let state = State::default();
        let mut menu = None;
        assert!(!update(&mut menu, &state, &[], false));
        assert!(menu.is_none());
        menu = Some(MenuState::open(1));
        assert!(!update(&mut menu, &state, &[], false));
        assert_eq!(menu.as_ref().unwrap().idx, 1);
        assert!(update(&mut menu, &state, &["codex".into()], true));
        assert!(menu.as_ref().unwrap().is_overwatch());
        menu.as_mut().unwrap().sel = 1;
        assert!(update(&mut menu, &state, &["codex".into()], false));
        assert_eq!(menu.as_ref().unwrap().sel, 1);
        menu = None;
        assert!(!update(&mut menu, &state, &[], false));
    }
}
