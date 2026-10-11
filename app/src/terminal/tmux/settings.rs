//! Settings for native tmux integration via tmux control mode (`tmux -CC`).
//!
//! Everything here is gated by [`FeatureFlag::TmuxControlMode`] and the master
//! `terminal.tmux_control_mode` setting (R22). Each behavior that departs from
//! upstream spec #14864 has its own setting so it can be turned off on its own
//! (R21). Code outside this module should read the effective state through the
//! `*_enabled` accessors on [`TmuxSettings`], which fold in the feature flag and
//! the master setting, rather than reading the raw setting values.

use settings::macros::define_settings_group;
use settings::{RespectUserSyncSetting, Setting, SupportedPlatforms, SyncToCloud};
use strum_macros::EnumIter;
use warp_core::features::FeatureFlag;

use crate::settings_view::flags;

/// tmux control mode is only offered where tmux runs locally.
fn tmux_platforms() -> SupportedPlatforms {
    SupportedPlatforms::OR(
        SupportedPlatforms::MAC.into(),
        SupportedPlatforms::LINUX.into(),
    )
}

define_settings_group!(TmuxSettings, settings: [
    tmux_control_mode: TmuxControlModeSetting {
        type: bool,
        default: false,
        supported_platforms: tmux_platforms(),
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        storage_key: "TmuxControlMode",
        toml_path: "terminal.tmux_control_mode",
        description: "Whether Warp opens tmux sessions natively via tmux control mode, mapping tmux windows and panes to Warp tabs and panes.",
        feature_flag: FeatureFlag::TmuxControlMode,
    },
    // R3. Only takes effect when `tmux_control_mode` is on.
    auto_convert: TmuxAutoConvert {
        type: bool,
        default: true,
        supported_platforms: tmux_platforms(),
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        storage_key: "TmuxAutoConvert",
        toml_path: "terminal.tmux.auto_convert",
        description: "Whether typing tmux, tmux attach, or tmux new opens the session natively without a click. When off, tmux shows its raw screen.",
        feature_flag: FeatureFlag::TmuxControlMode,
    },
    // R7 / R9. Only takes effect when `tmux_control_mode` is on.
    integrate_panes: TmuxIntegratePanes {
        type: bool,
        default: true,
        supported_platforms: tmux_platforms(),
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        storage_key: "TmuxIntegratePanes",
        toml_path: "terminal.tmux.integrate_panes",
        description: "Whether every new shell pane in an attached tmux session gets Warp's shell integration automatically.",
        feature_flag: FeatureFlag::TmuxControlMode,
    },
    // R18. Only takes effect when `tmux_control_mode` is on. Applies immediately.
    bindings_take_precedence: TmuxBindingsTakePrecedence {
        type: bool,
        default: true,
        supported_platforms: tmux_platforms(),
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        storage_key: "TmuxBindingsTakePrecedence",
        toml_path: "terminal.tmux.bindings_take_precedence",
        description: "Whether keys bound in the attached tmux server's key tables go to tmux before Warp inside a tmux-backed tab.",
        feature_flag: FeatureFlag::TmuxControlMode,
    },
]);

impl TmuxSettings {
    /// Whether native tmux integration is on: the feature flag is enabled and
    /// the master `terminal.tmux_control_mode` setting is on. Every tmux code
    /// path must be inert when this is `false`.
    pub fn control_mode_enabled(&self) -> bool {
        FeatureFlag::TmuxControlMode.is_enabled() && *self.tmux_control_mode.value()
    }

    /// Whether typed `tmux` commands convert to native sessions without a click (R3).
    pub fn auto_convert_enabled(&self) -> bool {
        self.control_mode_enabled() && *self.auto_convert.value()
    }

    /// Whether new panes in an attached session get Warp's shell integration (R7, R9).
    pub fn integrate_panes_enabled(&self) -> bool {
        self.control_mode_enabled() && *self.integrate_panes.value()
    }

    /// Whether tmux-bound keys win over Warp's bindings in tmux-backed tabs (R18).
    pub fn bindings_take_precedence_enabled(&self) -> bool {
        self.control_mode_enabled() && *self.bindings_take_precedence.value()
    }
}

/// One entry per toggleable tmux setting. Drives both the Warpify settings page
/// switches and the matching Command Palette enable/disable actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub enum TmuxToggle {
    ControlMode,
    AutoConvert,
    IntegratePanes,
    BindingsTakePrecedence,
}

impl TmuxToggle {
    /// User-facing label. The Command Palette shows `Enable {label}` / `Disable {label}`.
    pub fn label(self) -> &'static str {
        match self {
            TmuxToggle::ControlMode => "tmux control mode",
            TmuxToggle::AutoConvert => "tmux automatic session conversion",
            TmuxToggle::IntegratePanes => "Warp integration in tmux panes",
            TmuxToggle::BindingsTakePrecedence => "tmux key bindings precedence",
        }
    }

    /// One-line explanation shown under the switch on the Warpify settings page.
    pub fn page_description(self) -> &'static str {
        match self {
            TmuxToggle::ControlMode => {
                "Open tmux sessions natively: tmux windows become Warp tabs and tmux panes become Warp panes."
            }
            TmuxToggle::AutoConvert => {
                "Typing tmux, tmux attach, or tmux new opens the session natively without a click. When off, tmux shows its raw screen."
            }
            TmuxToggle::IntegratePanes => {
                "Every new shell pane in an attached session gets blocks, the input editor, completions, and AI automatically."
            }
            TmuxToggle::BindingsTakePrecedence => {
                "Inside a tmux-backed tab, keys bound in tmux go to tmux even when Warp also uses them. Takes effect immediately."
            }
        }
    }

    /// Storage key of the underlying setting.
    pub fn storage_key(self) -> &'static str {
        match self {
            TmuxToggle::ControlMode => TmuxControlModeSetting::storage_key(),
            TmuxToggle::AutoConvert => TmuxAutoConvert::storage_key(),
            TmuxToggle::IntegratePanes => TmuxIntegratePanes::storage_key(),
            TmuxToggle::BindingsTakePrecedence => TmuxBindingsTakePrecedence::storage_key(),
        }
    }

    /// Keymap context flag present while the setting is on. Set in the
    /// workspace keymap context so the palette shows the right action.
    pub fn context_flag(self) -> &'static str {
        match self {
            TmuxToggle::ControlMode => flags::TMUX_CONTROL_MODE_CONTEXT_FLAG,
            TmuxToggle::AutoConvert => flags::TMUX_AUTO_CONVERT_CONTEXT_FLAG,
            TmuxToggle::IntegratePanes => flags::TMUX_INTEGRATE_PANES_CONTEXT_FLAG,
            TmuxToggle::BindingsTakePrecedence => flags::TMUX_BINDINGS_TAKE_PRECEDENCE_CONTEXT_FLAG,
        }
    }

    /// The raw stored value of the setting (not folded with the master setting).
    pub fn value(self, settings: &TmuxSettings) -> bool {
        match self {
            TmuxToggle::ControlMode => *settings.tmux_control_mode.value(),
            TmuxToggle::AutoConvert => *settings.auto_convert.value(),
            TmuxToggle::IntegratePanes => *settings.integrate_panes.value(),
            TmuxToggle::BindingsTakePrecedence => *settings.bindings_take_precedence.value(),
        }
    }

    /// Flips the setting and persists it.
    pub fn toggle(
        self,
        settings: &mut TmuxSettings,
        ctx: &mut warpui::ModelContext<TmuxSettings>,
    ) -> anyhow::Result<bool> {
        use settings::ToggleableSetting as _;
        match self {
            TmuxToggle::ControlMode => settings.tmux_control_mode.toggle_and_save_value(ctx),
            TmuxToggle::AutoConvert => settings.auto_convert.toggle_and_save_value(ctx),
            TmuxToggle::IntegratePanes => settings.integrate_panes.toggle_and_save_value(ctx),
            TmuxToggle::BindingsTakePrecedence => {
                settings.bindings_take_precedence.toggle_and_save_value(ctx)
            }
        }
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
