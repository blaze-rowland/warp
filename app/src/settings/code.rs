use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use settings::macros::define_settings_group;
use settings::{RespectUserSyncSetting, Setting as _, SupportedPlatforms, SyncToCloud};
use warp_errors::report_if_error;
use warpui::ModelContext;

use crate::workspace::tab_settings::canonical_directory_key;

/// Code review dropdown pin/hide preferences for the repos under one parent folder.
/// Repo paths are stored canonicalized (see [`canonical_repo_list_key`]).
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    settings_value::SettingsValue,
)]
#[schemars(description = "Pinned and hidden repos in the code review repo dropdown.")]
pub struct RepoListPrefs {
    #[serde(default)]
    #[schemars(description = "Pinned repo paths, in pin order.")]
    pub pinned: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Hidden repo paths.")]
    pub hidden: Vec<String>,
}

impl RepoListPrefs {
    pub fn is_pinned(&self, repo: &str) -> bool {
        self.pinned.iter().any(|p| p == repo)
    }

    pub fn is_hidden(&self, repo: &str) -> bool {
        self.hidden.iter().any(|p| p == repo)
    }

    /// Like [`Self::is_pinned`], but compares as paths rather than strings.
    pub fn is_pinned_path(&self, repo: &Path) -> bool {
        self.pinned.iter().any(|p| Path::new(p) == repo)
    }

    /// Like [`Self::is_hidden`], but compares as paths rather than strings.
    pub fn is_hidden_path(&self, repo: &Path) -> bool {
        self.hidden.iter().any(|p| Path::new(p) == repo)
    }

    /// Appends `repo` to the pin order. A hidden repo is unhidden, since pinning is an
    /// explicit request to see it.
    pub fn pin(&mut self, repo: &str) {
        self.unhide(repo);
        if !self.is_pinned(repo) {
            self.pinned.push(repo.to_owned());
        }
    }

    pub fn unpin(&mut self, repo: &str) {
        self.pinned.retain(|p| p != repo);
    }

    /// Hides `repo`. A pinned repo is unpinned so it can't sit both pinned and hidden.
    pub fn hide(&mut self, repo: &str) {
        self.unpin(repo);
        if !self.is_hidden(repo) {
            self.hidden.push(repo.to_owned());
        }
    }

    pub fn unhide(&mut self, repo: &str) {
        self.hidden.retain(|p| p != repo);
    }

    fn is_empty(&self) -> bool {
        self.pinned.is_empty() && self.hidden.is_empty()
    }
}

/// Canonical string form of a path for use as a repo-list settings key or entry.
/// Canonicalizing makes differently-cased spellings on case-insensitive filesystems and
/// symlinked spellings (`/tmp` vs `/private/tmp`) agree. Falls back to the given path when
/// it can't be resolved (e.g. it no longer exists).
pub fn canonical_repo_list_key(path: &Path) -> String {
    canonical_directory_key(path)
}

define_settings_group!(CodeSettings, settings: [
    code_as_default_editor: CodeAsDefaultEditor {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Never,
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "code.editor.use_warp_as_default_editor",
        description: "Whether Warp is used as the default code editor.",
    }
    codebase_context_enabled: CodebaseContextEnabled {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::DESKTOP,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        storage_key: "AgentModeCodebaseContext",
        toml_path: "code.indexing.agent_mode_codebase_context",
        description: "Whether codebase context is provided to the AI agent.",
    },
    auto_indexing_enabled: AutoIndexingEnabled {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::DESKTOP,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        storage_key: "AgentModeCodebaseContextAutoIndexing",
        toml_path: "code.indexing.agent_mode_codebase_context_auto_indexing",
        description: "Whether automatic codebase indexing is enabled.",
    },
    // Whether or not the user has manually dismissed the code toolbelt new feature popup.
    dismissed_code_toolbelt_new_feature_popup: DismissedCodeToolbeltNewFeaturePopup {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: true,
    },
    // Controls whether the project explorer / file tree appears in the tools panel.
    show_project_explorer: ShowProjectExplorer {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "code.editor.show_project_explorer",
        description: "Whether the project explorer is shown in the tools panel.",
    },
    // Controls whether global file search appears in the tools panel.
    show_global_search: ShowGlobalSearch {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "code.editor.show_global_search",
        description: "Whether global file search is shown in the tools panel.",
    },
    // Controls whether hidden files (dotfiles) are shown in the project explorer.
    show_hidden_files: ShowHiddenFiles {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "code.editor.show_hidden_files",
        description: "Whether hidden files (dotfiles) are shown in the project explorer.",
    },
    // Controls whether the language server reformats the file on save.
    format_on_save: FormatOnSave {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "code.editor.format_on_save",
        description: "Whether the language server automatically formats the file on save. Other LSP features (hover, go-to-definition, references, diagnostics) are unaffected.",
    },
    // Controls whether the Warp text editor automatically saves file changes as the
    // user types (debounced) and when the editor loses focus. Only applies to the
    // Warp text editor, not the command line or AI input.
    auto_save: AutoSave {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        surface: settings::SettingSurfaces::GUI,
        private: false,
        toml_path: "code.editor.auto_save",
        description: "Whether the Warp text editor automatically saves changes as you type and when the editor loses focus.",
    },
    // Code review repo dropdown pins and hides, keyed by canonical parent folder path.
    // Local-only because the paths are machine-specific.
    code_review_repo_list_prefs: CodeReviewRepoListPrefs {
        type: HashMap<String, RepoListPrefs>,
        default: HashMap::default(),
        supported_platforms: SupportedPlatforms::DESKTOP,
        sync_to_cloud: SyncToCloud::Never,
        surface: settings::SettingSurfaces::GUI,
        private: true,
    },
]);

impl CodeSettings {
    /// Pin/hide preferences for the repos under `parent`.
    pub fn repo_list_prefs(&self, parent: &Path) -> RepoListPrefs {
        self.code_review_repo_list_prefs
            .get(&canonical_repo_list_key(parent))
            .cloned()
            .unwrap_or_default()
    }

    pub fn pin_repo(&mut self, parent: &Path, repo: &Path, ctx: &mut ModelContext<Self>) {
        self.update_repo_list_prefs(parent, repo, RepoListPrefs::pin, ctx);
    }

    pub fn unpin_repo(&mut self, parent: &Path, repo: &Path, ctx: &mut ModelContext<Self>) {
        self.update_repo_list_prefs(parent, repo, RepoListPrefs::unpin, ctx);
    }

    pub fn hide_repo(&mut self, parent: &Path, repo: &Path, ctx: &mut ModelContext<Self>) {
        self.update_repo_list_prefs(parent, repo, RepoListPrefs::hide, ctx);
    }

    pub fn unhide_repo(&mut self, parent: &Path, repo: &Path, ctx: &mut ModelContext<Self>) {
        self.update_repo_list_prefs(parent, repo, RepoListPrefs::unhide, ctx);
    }

    fn update_repo_list_prefs(
        &mut self,
        parent: &Path,
        repo: &Path,
        mutate: fn(&mut RepoListPrefs, &str),
        ctx: &mut ModelContext<Self>,
    ) {
        let key = canonical_repo_list_key(parent);
        let mut map = self.code_review_repo_list_prefs.clone();
        let prefs = map.entry(key.clone()).or_default();
        mutate(prefs, &canonical_repo_list_key(repo));
        if prefs.is_empty() {
            map.remove(&key);
        }
        if map == *self.code_review_repo_list_prefs {
            return;
        }
        report_if_error!(self.code_review_repo_list_prefs.set_value(map, ctx));
    }
}
