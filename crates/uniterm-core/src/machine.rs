//! Machine profiles are metadata only; SSH owns authentication and host trust.
use serde::{Deserialize, Serialize};

/// A saved target has an identity independent of its editable display label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineProfile {
    /// Monotonic catalog identity; never inferred from the display label.
    pub id: u64,
    /// Human-selected printable label shown in the Servers surface.
    pub label: String,
    /// OpenSSH host, user@host, URI or configured alias.
    pub target: String,
    /// Default remote Workspace; discovery can select a different one.
    pub workspace: String,
    /// Disabled profiles remain saved but cannot start a connection.
    pub enabled: bool,
}

/// Catalog mutations are appended before a client observes their projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MachineEvent {
    Saved(MachineProfile),
    Removed(u64),
}

/// Reject option injection, control bytes and ambiguous whitespace before SSH.
pub fn validate_target(target: &str) -> Result<(), &'static str> {
    if target.is_empty()
        || target.len() > 1024
        || target.starts_with('-')
        || target.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(
            "SSH target must be a nonempty host or SSH alias without whitespace or leading '-'",
        );
    }
    Ok(())
}

impl MachineProfile {
    /// Validate bounded metadata before it enters the durable catalog or UI.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_target(&self.target)?;
        if self.id == 0
            || self.label.trim().is_empty()
            || self.label.len() > 128
            || self.label.chars().any(char::is_control)
        {
            return Err("Server label must contain 1 to 128 bytes of printable text");
        }
        if self.workspace.is_empty()
            || self.workspace.len() > 64
            || self.workspace == "."
            || self.workspace == ".."
            || !self
                .workspace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err("invalid remote Workspace name");
        }
        Ok(())
    }
}

/// Replay one validated event without consulting processes, disks or clocks.
pub fn apply(profiles: &mut Vec<MachineProfile>, event: MachineEvent) -> Result<(), &'static str> {
    match event {
        MachineEvent::Saved(profile) => {
            profile.validate()?;
            if profiles
                .iter()
                .any(|p| p.id != profile.id && p.label.eq_ignore_ascii_case(&profile.label))
            {
                return Err("Server label already exists");
            }
            if let Some(old) = profiles.iter_mut().find(|p| p.id == profile.id) {
                *old = profile;
            } else if profiles.len() < 128 {
                profiles.push(profile);
            } else {
                return Err("saved Server limit reached (128)");
            }
        }
        MachineEvent::Removed(id) => profiles.retain(|p| p.id != id),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_replay_rename_disable_and_remove_without_reusing_identity() {
        let mut state = Vec::new();
        let mut p = MachineProfile {
            id: 1,
            label: "Build".into(),
            target: "user@host".into(),
            workspace: "work".into(),
            enabled: true,
        };
        apply(&mut state, MachineEvent::Saved(p.clone())).unwrap();
        p.label = "Build two".into();
        p.enabled = false;
        apply(&mut state, MachineEvent::Saved(p.clone())).unwrap();
        assert_eq!(state, [p]);
        apply(&mut state, MachineEvent::Removed(1)).unwrap();
        assert!(state.is_empty());
    }
    #[test]
    fn ssh_targets_reject_option_and_terminal_injection() {
        for invalid in [
            "",
            "-oProxyCommand=bad",
            "host\n",
            "two hosts",
            "host\x1b]0;bad",
        ] {
            assert!(validate_target(invalid).is_err());
        }
        for valid in ["build", "user@host", "ssh://user@host:2222"] {
            assert!(validate_target(valid).is_ok());
        }
    }
}
