//! Resolve one keyboard recipient for overlapping Desktop surfaces.

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InputOverlays {
    pub resource_dialog: bool,
    pub remote_picker: bool,
    pub project_search: bool,
    pub git_search: bool,
    pub conversation_search: bool,
    pub remotes: bool,
    pub settings_restart: bool,
    pub settings_input: bool,
    pub help: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputTarget {
    ResourceDialog,
    RemotePicker,
    ProjectSearch,
    GitSearch,
    ConversationSearch,
    Remotes,
    SettingsRestart,
    SettingsInput,
    Help,
    Workspace,
}

impl InputOverlays {
    pub fn target(self) -> InputTarget {
        use InputTarget::*;
        [
            (self.resource_dialog, ResourceDialog),
            (self.remote_picker, RemotePicker),
            (self.project_search, ProjectSearch),
            (self.conversation_search, ConversationSearch),
            (self.git_search, GitSearch),
            (self.remotes, Remotes),
            (self.settings_restart, SettingsRestart),
            (self.settings_input, SettingsInput),
            (self.help, Help),
        ]
        .into_iter()
        .find_map(|(active, target)| active.then_some(target))
        .unwrap_or(Workspace)
    }
}

impl InputTarget {
    pub fn key_context(self, workspace_context: &'static str) -> &'static str {
        match self {
            Self::ResourceDialog => "ResourceDialog",
            Self::Help => "Help",
            Self::Workspace => workspace_context,
            _ => "BoomuxSettingsInput",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_dialog_owns_typing_with_any_underlying_panel() {
        for panel in [
            InputOverlays {
                remotes: true,
                ..Default::default()
            },
            InputOverlays {
                remote_picker: true,
                project_search: true,
                ..Default::default()
            },
            InputOverlays {
                project_search: true,
                ..Default::default()
            },
            InputOverlays {
                conversation_search: true,
                ..Default::default()
            },
            InputOverlays {
                git_search: true,
                ..Default::default()
            },
            InputOverlays {
                settings_restart: true,
                ..Default::default()
            },
            InputOverlays {
                settings_input: true,
                ..Default::default()
            },
            InputOverlays {
                help: true,
                ..Default::default()
            },
            InputOverlays::default(),
        ] {
            let modal = InputOverlays {
                resource_dialog: true,
                ..panel
            };
            assert_eq!(modal.target(), InputTarget::ResourceDialog);
            for base in ["Terminal", "Layout", "Sidebar", "SidebarLayout"] {
                assert_eq!(modal.target().key_context(base), "ResourceDialog");
            }
            assert_ne!(panel.target(), InputTarget::ResourceDialog);
        }
    }

    #[test]
    fn conversation_search_owns_keys_until_dismissed() {
        let mut overlays = InputOverlays {
            conversation_search: true,
            remotes: true,
            ..Default::default()
        };
        assert_eq!(overlays.target(), InputTarget::ConversationSearch);
        assert_eq!(
            overlays.target().key_context("Terminal"),
            "BoomuxSettingsInput"
        );
        overlays.conversation_search = false;
        assert_eq!(overlays.target(), InputTarget::Remotes);
    }

    #[test]
    fn dismissing_dialog_returns_keyboard_to_the_open_remotes_panel() {
        let mut state = InputOverlays {
            resource_dialog: true,
            remotes: true,
            ..Default::default()
        };
        assert_eq!(state.target(), InputTarget::ResourceDialog);
        state.resource_dialog = false;
        assert_eq!(state.target(), InputTarget::Remotes);
        assert_eq!(state.target().key_context("Sidebar"), "BoomuxSettingsInput");
        state.remotes = false;
        assert_eq!(state.target().key_context("SidebarLayout"), "SidebarLayout");
    }
}
