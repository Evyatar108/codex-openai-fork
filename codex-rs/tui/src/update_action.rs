#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallContext;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallMethod;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::StandalonePlatform;

/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    // SANDBOX PATCH: variants still identify the detected install method, but `command_args`
    // no longer emits the upstream channel command (the upstream npm package, brew, or the
    // chatgpt.com installer scripts). Every variant now resolves to the fork releases page
    // (https://github.com/gim-home/codex/releases).
    /// Detected an npm-managed install.
    NpmGlobalLatest,
    /// Detected a bun-managed install.
    BunGlobalLatest,
    /// Detected a Homebrew-managed install.
    BrewUpgrade,
    /// Detected a standalone Unix install.
    StandaloneUnix,
    /// Detected a standalone Windows install.
    StandaloneWindows,
}

impl UpdateAction {
    #[cfg(any(not(debug_assertions), test))]
    pub(crate) fn from_install_context(context: &InstallContext) -> Option<Self> {
        match &context.method {
            InstallMethod::Npm => Some(UpdateAction::NpmGlobalLatest),
            InstallMethod::Bun => Some(UpdateAction::BunGlobalLatest),
            InstallMethod::Brew => Some(UpdateAction::BrewUpgrade),
            InstallMethod::Standalone { platform, .. } => Some(match platform {
                StandalonePlatform::Unix => UpdateAction::StandaloneUnix,
                StandalonePlatform::Windows => UpdateAction::StandaloneWindows,
            }),
            InstallMethod::Other => None,
        }
    }

    /// Returns the list of command-line arguments for invoking the update.
    pub fn command_args(self) -> (&'static str, &'static [&'static str]) {
        // SANDBOX PATCH: every upstream install/update channel (the upstream npm package,
        // brew, and the chatgpt.com installer scripts) is redirected to the fork releases
        // page. The fork ships via GitHub Releases / GitHub Packages and has no install.sh; a
        // bare `npm install -g @gim-home/codex` also won't work for clean users (GitHub
        // Packages needs a registry + read:packages token). This path is inert (tui::updates::
        // get_upgrade_version returns None), but the hint is surfaced if a cell is built.
        match self {
            UpdateAction::NpmGlobalLatest
            | UpdateAction::BunGlobalLatest
            | UpdateAction::BrewUpgrade
            | UpdateAction::StandaloneUnix
            | UpdateAction::StandaloneWindows => {
                ("https://github.com/gim-home/codex/releases", &[])
            }
        }
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command).chain(args.iter().copied()))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

#[cfg(not(debug_assertions))]
pub fn get_update_action() -> Option<UpdateAction> {
    UpdateAction::from_install_context(InstallContext::current())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn maps_install_context_to_update_action() {
        let native_release_dir =
            AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join("native-release"))
                .expect("temp dir path should be absolute");

        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Other,
                package_layout: None,
            }),
            None
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Npm,
                package_layout: None,
            }),
            Some(UpdateAction::NpmGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Bun,
                package_layout: None,
            }),
            Some(UpdateAction::BunGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Brew,
                package_layout: None,
            }),
            Some(UpdateAction::BrewUpgrade)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Unix,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            Some(UpdateAction::StandaloneUnix)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Windows,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            Some(UpdateAction::StandaloneWindows)
        );
    }

    #[test]
    fn all_update_actions_point_at_fork_releases() {
        // SANDBOX PATCH: upstream returned channel-specific installer commands; the fork
        // redirects every variant to the releases page.
        for action in [
            UpdateAction::NpmGlobalLatest,
            UpdateAction::BunGlobalLatest,
            UpdateAction::BrewUpgrade,
            UpdateAction::StandaloneUnix,
            UpdateAction::StandaloneWindows,
        ] {
            let (command, args) = action.command_args();
            assert_eq!(command, "https://github.com/gim-home/codex/releases");
            assert!(args.is_empty());
            assert_eq!(
                action.command_str(),
                "https://github.com/gim-home/codex/releases"
            );
        }
    }
}
