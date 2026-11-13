//! The dropdown-menu model: pure definitions shared by the server chrome and
//! the client that draws each requested menu. The server resolves the target
//! and anchor so a click always acts on what was drawn.
//!
//! The definitions cover Pane, Tab, agent, and Workspace verbs. Every item
//! shows its prefix shortcut so contextual and keyboard-opened menus teach the
//! same bindings.

/// An action a menu item triggers. The client maps these onto protocol
/// commands or client-side surfaces; the core stays UI-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    /// Workspace assistant setup and lifecycle controls.
    Overwatch,
    /// Enable or disable only the Workspace-owned assistant.
    OverwatchSet(bool),
    /// Focus its native interactive chat.
    OverwatchChat,
    /// Select a provider by registry ordinal while disabled.
    OverwatchChoose(usize),
    /// Grant or revoke management while disabled.
    OverwatchManage(bool),
    /// Reset the bounded recovery budget explicitly.
    OverwatchRetry,
    /// Browse the provider's models before choosing supported reasoning effort.
    OverwatchModel,
    /// Change reasoning effort for the selected model.
    OverwatchEffort,
    /// Select one model from the current catalog projection.
    OverwatchModelChoice(usize),
    /// Restore the harness's model and effort defaults together.
    OverwatchModelDefault,
    /// Enter an advanced custom model id.
    OverwatchModelCustom,
    /// Refresh the model catalog explicitly.
    OverwatchModelsRefresh,
    /// Select one supported effort from the pending model's catalog entry.
    OverwatchEffortChoice(usize),
    /// Use the harness's effort default for the pending model.
    OverwatchEffortDefault,
    /// Enter an advanced custom effort id when capability is unknown.
    OverwatchEffortCustom,
    /// Return to model selection without applying pending choices.
    OverwatchModelsBack,
    /// Focus pane left.
    FocusLeft,
    /// Focus pane down.
    FocusDown,
    /// Focus pane up.
    FocusUp,
    /// Focus pane right.
    FocusRight,
    /// Resize pane left.
    ResizeLeft,
    /// Resize pane down.
    ResizeDown,
    /// Resize pane up.
    ResizeUp,
    /// Resize pane right.
    ResizeRight,
    /// Focus previous pane.
    LastPane,
    /// Toggle file sidebar.
    FileSidebar,
    /// Select a Tab by its zero-based ordinal in the active Project.
    SelectTab(u8),
    /// Choose an automatic or manual arrangement for this Tab.
    PaneLayout(crate::layout::PaneLayout),
    SplitRight,
    SplitDown,
    Zoom,
    ZoomOut,
    CopyMode,
    ClosePane,
    NewTab,
    /// Create a child Tab in the current stack.
    NewStackTab,
    /// Promote a child Tab to the top level.
    UnstackTab,
    RenameTab,
    CloseTab,
    NextTab,
    PrevTab,
    /// Reorder the selected Tab without changing its Pane selection.
    MoveTabLeft,
    /// Reorder the selected Tab without changing its Pane selection.
    MoveTabRight,
    /// Open the day timeline without changing the remembered Project.
    Today,
    /// Browse client-local saved SSH Servers.
    Servers,
    /// Open the opt-in Workspace navigation surface.
    Switcher,
    /// Focus an exact Pane selected from a live navigation projection.
    FocusPane(crate::PaneId),
    /// Inspect the opt-in native orchestration graph.
    RunGraph,
    NewTask,
    Observatory,
    Tasks,
    /// Open the Manage Agents modal (connectors, launch, stop all).
    ManageAgents,
    /// Open the session-switcher modal (list, switch, kill).
    Sessions,
    /// Rename the current session (opens the rename input).
    RenameSession,
    /// Terminate the current session (server and all its panes).
    KillSession,
    /// Open the Project manager for the current Workspace.
    Projects,
    /// Add a Project by selecting its root directory.
    NewProject,
    /// Switch to the Project targeted by a sidebar context menu.
    SwitchProject,
    /// Create a Tab in the Project targeted by a sidebar context menu.
    NewProjectTab,
    /// Rename the Project targeted by a sidebar context menu.
    RenameProject,
    /// Edit the directory used by future launches in the targeted Project.
    ChangeProjectDirectory,
    /// Move the targeted Project one position earlier in the sidebar.
    MoveProjectUp,
    /// Move the targeted Project one position later in the sidebar.
    MoveProjectDown,
    /// Move the selected Project to the beginning of the sidebar.
    MoveProjectTop,
    /// Move the selected Project to the end of the sidebar.
    MoveProjectBottom,
    /// Close the targeted Project and every Pane it owns.
    CloseProject,
    /// Open the schema-backed application Settings surface.
    Settings,
    /// Open the application identity, version, and documentation surface.
    About,
    /// Toggle the left-hand Project sidebar.
    Sidebar,
    Detach,
}

/// One menu entry: a label, the post-prefix key it mirrors (empty if none),
/// and the action it triggers.
#[derive(Clone, Copy, Debug)]
pub struct MenuItem {
    pub label: &'static str,
    /// The key pressed after the prefix, as displayed (e.g. `"c"`, `"%"`).
    pub key: &'static str,
    pub action: MenuAction,
}

/// One titled command-menu group.
#[derive(Clone, Copy, Debug)]
pub struct Menu {
    pub title: &'static str,
    pub items: &'static [MenuItem],
    /// Actions that start a section, independent of item positions.
    pub section_starts: &'static [MenuAction],
}

impl Menu {
    /// Derive separators from semantic section starts after insertions or removals.
    /// The first item never needs a leading separator.
    pub fn separators_before(&self) -> impl Iterator<Item = usize> + '_ {
        self.items.iter().enumerate().filter_map(|(index, item)| {
            (index > 0 && self.section_starts.contains(&item.action)).then_some(index)
        })
    }

    /// Shared visible rows for rendering and hit testing; `None` is a separator.
    pub fn rows(&self) -> impl Iterator<Item = Option<usize>> + '_ {
        self.items.iter().enumerate().flat_map(|(index, item)| {
            let separated = index > 0 && self.section_starts.contains(&item.action);
            [separated.then_some(None), Some(Some(index))]
                .into_iter()
                .flatten()
        })
    }
}

/// Command-menu groups in keyboard navigation order.
pub const MENUS: &[Menu] = &[
    Menu {
        title: "Pane",
        section_starts: &[],
        items: &[
            MenuItem {
                label: "Split right",
                key: "%",
                action: MenuAction::SplitRight,
            },
            MenuItem {
                label: "Split down",
                key: "\"",
                action: MenuAction::SplitDown,
            },
            MenuItem {
                label: "Split layout",
                key: "",
                action: MenuAction::PaneLayout(crate::layout::PaneLayout::Split),
            },
            MenuItem {
                label: "Dwindle layout",
                key: "",
                action: MenuAction::PaneLayout(crate::layout::PaneLayout::Dwindle),
            },
            MenuItem {
                label: "Scrolling layout",
                key: "",
                action: MenuAction::PaneLayout(crate::layout::PaneLayout::Scrolling),
            },
            MenuItem {
                label: "Zoom in (toggle)",
                key: "z",
                action: MenuAction::Zoom,
            },
            MenuItem {
                label: "Zoom out: all tabs",
                key: "w",
                action: MenuAction::ZoomOut,
            },
            MenuItem {
                label: "Scrollback / copy",
                key: "[",
                action: MenuAction::CopyMode,
            },
            MenuItem {
                label: "Close pane",
                key: "x",
                action: MenuAction::ClosePane,
            },
        ],
    },
    Menu {
        title: "Tabs",
        section_starts: &[],
        items: &[
            MenuItem {
                label: "New tab",
                key: "c",
                action: MenuAction::NewTab,
            },
            MenuItem {
                label: "New child tab",
                key: "",
                action: MenuAction::NewStackTab,
            },
            MenuItem {
                label: "Move out of stack",
                key: "",
                action: MenuAction::UnstackTab,
            },
            MenuItem {
                label: "Rename tab",
                key: ",",
                action: MenuAction::RenameTab,
            },
            MenuItem {
                label: "Close tab",
                key: "&",
                action: MenuAction::CloseTab,
            },
            MenuItem {
                label: "Next tab",
                key: "n",
                action: MenuAction::NextTab,
            },
            MenuItem {
                label: "Previous tab",
                key: "p",
                action: MenuAction::PrevTab,
            },
            MenuItem {
                label: "Move tab left",
                key: "<",
                action: MenuAction::MoveTabLeft,
            },
            MenuItem {
                label: "Move tab right",
                key: ">",
                action: MenuAction::MoveTabRight,
            },
        ],
    },
    Menu {
        title: "Agents",
        section_starts: &[],
        items: &[
            MenuItem {
                label: "New task...",
                key: "N",
                action: MenuAction::NewTask,
            },
            MenuItem {
                label: "Tasks",
                key: "t",
                action: MenuAction::Tasks,
            },
            MenuItem {
                label: "Config...",
                key: "a",
                action: MenuAction::ManageAgents,
            },
        ],
    },
    Menu {
        title: "Workspace",
        section_starts: &[
            MenuAction::NewProject,
            MenuAction::RenameSession,
            MenuAction::Sidebar,
            MenuAction::About,
            MenuAction::Detach,
        ],
        items: &[
            MenuItem {
                label: "Overwatch...",
                key: "O",
                action: MenuAction::Overwatch,
            },
            MenuItem {
                label: "Today / Timeline",
                key: "D",
                action: MenuAction::Today,
            },
            MenuItem {
                label: "Find Tabs and Agents",
                key: "S",
                action: MenuAction::Switcher,
            },
            MenuItem {
                label: "Servers...",
                key: "",
                action: MenuAction::Servers,
            },
            MenuItem {
                label: "New Project...",
                key: "A",
                action: MenuAction::NewProject,
            },
            MenuItem {
                label: "Manage Projects...",
                key: "P",
                action: MenuAction::Projects,
            },
            MenuItem {
                label: "Rename Workspace",
                key: "$",
                action: MenuAction::RenameSession,
            },
            MenuItem {
                label: "Manage Workspaces...",
                key: "s",
                action: MenuAction::Sessions,
            },
            MenuItem {
                label: "Projects",
                key: "b",
                action: MenuAction::Sidebar,
            },
            MenuItem {
                label: "Observatory",
                key: "o",
                action: MenuAction::Observatory,
            },
            MenuItem {
                label: "Settings",
                key: "g",
                action: MenuAction::Settings,
            },
            MenuItem {
                label: "About Uniterm",
                key: "",
                action: MenuAction::About,
            },
            MenuItem {
                label: "Detach",
                key: "d",
                action: MenuAction::Detach,
            },
            MenuItem {
                label: "Close this Workspace",
                key: "Q",
                action: MenuAction::KillSession,
            },
        ],
    },
    Menu {
        title: "Projects",
        section_starts: &[],
        items: &[
            MenuItem {
                label: "New Project...",
                key: "",
                action: MenuAction::NewProject,
            },
            MenuItem {
                label: "Manage Projects...",
                key: "",
                action: MenuAction::Projects,
            },
        ],
    },
    Menu {
        title: "Project",
        section_starts: &[
            MenuAction::RenameProject,
            MenuAction::MoveProjectTop,
            MenuAction::NewProject,
        ],
        items: &[
            MenuItem {
                label: "Open Project",
                key: "",
                action: MenuAction::SwitchProject,
            },
            MenuItem {
                label: "New Tab",
                key: "",
                action: MenuAction::NewProjectTab,
            },
            MenuItem {
                label: "Rename Project...",
                key: "",
                action: MenuAction::RenameProject,
            },
            MenuItem {
                label: "Change Directory...",
                key: "",
                action: MenuAction::ChangeProjectDirectory,
            },
            MenuItem {
                label: "Move Up",
                key: "",
                action: MenuAction::MoveProjectUp,
            },
            MenuItem {
                label: "Move Down",
                key: "",
                action: MenuAction::MoveProjectDown,
            },
            MenuItem {
                label: "Move to Top",
                key: "",
                action: MenuAction::MoveProjectTop,
            },
            MenuItem {
                label: "Move to Bottom",
                key: "",
                action: MenuAction::MoveProjectBottom,
            },
            MenuItem {
                label: "New Project...",
                key: "",
                action: MenuAction::NewProject,
            },
            MenuItem {
                label: "Manage Projects...",
                key: "",
                action: MenuAction::Projects,
            },
            MenuItem {
                label: "Close Project and Panes",
                key: "",
                action: MenuAction::CloseProject,
            },
        ],
    },
];

/// Menus reachable from the keyboard command bar. The remaining definitions
/// are anchored context menus and require a server-resolved target.
pub const MENU_BAR_LEN: usize = 4;

/// Keyboard actions also discoverable in the prefix action picker.
pub const QUICK_ACTIONS: &[MenuItem] = &[
    MenuItem {
        label: "Graph",
        key: "G",
        action: MenuAction::RunGraph,
    },
    MenuItem {
        label: "Focus pane left",
        key: "h",
        action: MenuAction::FocusLeft,
    },
    MenuItem {
        label: "Focus pane down",
        key: "j",
        action: MenuAction::FocusDown,
    },
    MenuItem {
        label: "Focus pane up",
        key: "k",
        action: MenuAction::FocusUp,
    },
    MenuItem {
        label: "Focus pane right",
        key: "l",
        action: MenuAction::FocusRight,
    },
    MenuItem {
        label: "Resize pane left",
        key: "H",
        action: MenuAction::ResizeLeft,
    },
    MenuItem {
        label: "Resize pane down",
        key: "J",
        action: MenuAction::ResizeDown,
    },
    MenuItem {
        label: "Resize pane up",
        key: "K",
        action: MenuAction::ResizeUp,
    },
    MenuItem {
        label: "Resize pane right",
        key: "L",
        action: MenuAction::ResizeRight,
    },
    MenuItem {
        label: "Focus previous pane",
        key: ";",
        action: MenuAction::LastPane,
    },
    MenuItem {
        label: "Toggle file sidebar",
        key: "f",
        action: MenuAction::FileSidebar,
    },
    MenuItem {
        label: "Select tab 1",
        key: "1",
        action: MenuAction::SelectTab(0),
    },
    MenuItem {
        label: "Select tab 2",
        key: "2",
        action: MenuAction::SelectTab(1),
    },
    MenuItem {
        label: "Select tab 3",
        key: "3",
        action: MenuAction::SelectTab(2),
    },
    MenuItem {
        label: "Select tab 4",
        key: "4",
        action: MenuAction::SelectTab(3),
    },
    MenuItem {
        label: "Select tab 5",
        key: "5",
        action: MenuAction::SelectTab(4),
    },
    MenuItem {
        label: "Select tab 6",
        key: "6",
        action: MenuAction::SelectTab(5),
    },
    MenuItem {
        label: "Select tab 7",
        key: "7",
        action: MenuAction::SelectTab(6),
    },
    MenuItem {
        label: "Select tab 8",
        key: "8",
        action: MenuAction::SelectTab(7),
    },
    MenuItem {
        label: "Select tab 9",
        key: "9",
        action: MenuAction::SelectTab(8),
    },
    MenuItem {
        label: "Select tab 10",
        key: "0",
        action: MenuAction::SelectTab(9),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_key_is_a_real_binding_shape() {
        // Keys are single displayable characters (the menu teaches
        // prefix+key), or empty for menu-only items.
        for m in MENUS {
            assert!(!m.items.is_empty());
            for it in m.items {
                assert!(it.key.chars().count() <= 1, "{}", it.label);
            }
        }
    }

    #[test]
    fn every_displayed_shortcut_is_unique() {
        let mut seen = std::collections::HashMap::new();
        for menu in MENUS {
            for item in menu.items {
                if item.key.is_empty() {
                    continue;
                }
                assert_eq!(
                    seen.insert(item.key, item.label),
                    None,
                    "shortcut {} is shared by {}",
                    item.key,
                    item.label
                );
            }
        }
    }

    #[test]
    fn workspace_menu_has_requested_groups_and_actions() {
        let workspace = MENUS.iter().find(|menu| menu.title == "Workspace").unwrap();
        let starts: Vec<_> = workspace
            .separators_before()
            .map(|index| workspace.items[index].action)
            .collect();
        assert_eq!(
            starts,
            [
                MenuAction::NewProject,
                MenuAction::RenameSession,
                MenuAction::Sidebar,
                MenuAction::About,
                MenuAction::Detach
            ]
        );
        assert!(!workspace
            .items
            .iter()
            .any(|item| item.action == MenuAction::RunGraph));
        let labels: Vec<_> = workspace.items.iter().map(|item| item.label).collect();
        assert_eq!(
            labels,
            [
                "Overwatch...",
                "Today / Timeline",
                "Find Tabs and Agents",
                "Servers...",
                "New Project...",
                "Manage Projects...",
                "Rename Workspace",
                "Manage Workspaces...",
                "Projects",
                "Observatory",
                "Settings",
                "About Uniterm",
                "Detach",
                "Close this Workspace",
            ]
        );
        for (action, key) in [
            (MenuAction::NewProject, "A"),
            (MenuAction::Sidebar, "b"),
            (MenuAction::About, ""),
            (MenuAction::KillSession, "Q"),
        ] {
            assert_eq!(
                workspace
                    .items
                    .iter()
                    .find(|item| item.action == action)
                    .unwrap()
                    .key,
                key
            );
        }
        assert!(QUICK_ACTIONS
            .iter()
            .any(|item| item.action == MenuAction::RunGraph && item.key == "G"));
        assert_eq!(
            workspace.items.last().unwrap().action,
            MenuAction::KillSession
        );
    }

    #[test]
    fn semantic_sections_survive_an_inserted_item_without_splitting_the_tail() {
        let workspace = MENUS.iter().find(|menu| menu.title == "Workspace").unwrap();
        // Simulate extending any section without changing its semantic definition.
        for insertion in 0..workspace.items.len() - 2 {
            let mut items = workspace.items.to_vec();
            items.insert(
                insertion,
                MenuItem {
                    label: "New feature",
                    key: "",
                    action: MenuAction::CopyMode,
                },
            );
            let extended = Menu {
                items: Box::leak(items.into_boxed_slice()),
                ..*workspace
            };
            let actions: Vec<_> = extended
                .rows()
                .map(|row| row.map(|index| extended.items[index].action))
                .collect();
            assert!(actions.ends_with(&[
                None,
                Some(MenuAction::About),
                None,
                Some(MenuAction::Detach),
                Some(MenuAction::KillSession)
            ]));
            assert_eq!(
                actions.iter().filter(|row| row.is_none()).count(),
                workspace.separators_before().count()
            );
            assert_eq!(
                extended.rows().flatten().collect::<Vec<_>>(),
                (0..extended.items.len()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn agents_menu_exposes_config_action() {
        let agents = MENUS.iter().find(|menu| menu.title == "Agents").unwrap();
        let setup = agents.items.last().unwrap();
        assert_eq!(setup.label, "Config...");
        assert_eq!(setup.key, "a");
        assert_eq!(setup.action, MenuAction::ManageAgents);
    }

    #[test]
    fn project_context_menus_cover_empty_space_and_specific_projects() {
        let empty = MENUS.iter().find(|menu| menu.title == "Projects").unwrap();
        assert_eq!(empty.items.len(), 2);
        assert_eq!(empty.items[0].action, MenuAction::NewProject);

        let project = MENUS.iter().find(|menu| menu.title == "Project").unwrap();
        let actions: Vec<_> = project.items.iter().map(|item| item.action).collect();
        assert!(actions.contains(&MenuAction::SwitchProject));
        assert!(actions.contains(&MenuAction::RenameProject));
        assert!(actions.contains(&MenuAction::MoveProjectUp));
        assert!(actions.contains(&MenuAction::MoveProjectDown));
        assert!(actions.contains(&MenuAction::CloseProject));
    }
}
