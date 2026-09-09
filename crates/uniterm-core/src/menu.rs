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
    SplitRight,
    SplitDown,
    Zoom,
    ZoomOut,
    CopyMode,
    ClosePane,
    NewTab,
    RenameTab,
    CloseTab,
    NextTab,
    PrevTab,
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
    /// Move the targeted Project one position earlier in the sidebar.
    MoveProjectUp,
    /// Move the targeted Project one position later in the sidebar.
    MoveProjectDown,
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
    /// Item indices that receive a separator row immediately before them.
    pub separators_before: &'static [usize],
}

/// Command-menu groups in keyboard navigation order.
pub const MENUS: &[Menu] = &[
    Menu {
        title: "Pane",
        separators_before: &[],
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
        separators_before: &[],
        items: &[
            MenuItem {
                label: "New tab",
                key: "c",
                action: MenuAction::NewTab,
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
        ],
    },
    Menu {
        title: "Agents",
        separators_before: &[],
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
                label: "Setup...",
                key: "a",
                action: MenuAction::ManageAgents,
            },
        ],
    },
    Menu {
        title: "Workspace",
        separators_before: &[2, 4, 7, 8],
        items: &[
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
        separators_before: &[],
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
        separators_before: &[2, 5, 7],
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
        assert_eq!(workspace.separators_before, &[2, 4, 7, 8]);
        let labels: Vec<_> = workspace.items.iter().map(|item| item.label).collect();
        assert_eq!(
            labels,
            [
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
        assert_eq!(workspace.items[4].action, MenuAction::Sidebar);
        assert_eq!(workspace.items[0].key, "A");
        assert_eq!(workspace.items[4].key, "b");
        assert_eq!(workspace.items[5].action, MenuAction::Observatory);
        assert_eq!(workspace.items[6].action, MenuAction::Settings);
        assert_eq!(workspace.items[7].action, MenuAction::About);
        assert_eq!(workspace.items[7].key, "");
        assert_eq!(workspace.items[9].key, "Q");
        assert_eq!(
            workspace.items.last().unwrap().action,
            MenuAction::KillSession
        );
    }

    #[test]
    fn agents_menu_exposes_setup_action() {
        let agents = MENUS.iter().find(|menu| menu.title == "Agents").unwrap();
        let setup = agents.items.last().unwrap();
        assert_eq!(setup.label, "Setup...");
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
