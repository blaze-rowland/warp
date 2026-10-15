use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ai::skills::SkillProvider;
use chrono::Local;
use pathfinder_color::ColorU;
use smol_str::SmolStr;
use warp_editor::render::model::LineCount;
use warp_util::path::EscapeChar;
use warpui::App;

use super::{
    CLIAgent, ReviewGitTarget, UBER_TEAM_UID, build_diff_hunk_prompt, build_review_prompt,
    build_selection_line_range_prompt, build_selection_substring_prompt,
};
use crate::ai::agent::{AgentReviewCommentBatch, DiffSetHunk};
use crate::code::buffer_location::LocalOrRemotePath;
use crate::code::editor::line::EditorLineLocation;
use crate::code_review::comments::{
    AttachedReviewComment, AttachedReviewCommentTarget, CommentOrigin, LineDiffContent,
};
use crate::code_review::diff_state::DiffMode;
use crate::server::ids::ServerId;
use crate::server::server_api::team::MockTeamClient;
use crate::server::server_api::workspace::MockWorkspaceClient;
use crate::ui_components::icons::Icon;
use crate::workspaces::team::Team;
use crate::workspaces::user_workspaces::UserWorkspaces;
use crate::workspaces::workspace::Workspace;

/// Helper to build an alias map from pairs.
fn aliases(pairs: &[(&str, &str)]) -> HashMap<SmolStr, String> {
    pairs
        .iter()
        .map(|(k, v)| (SmolStr::new(k), v.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers for prompt-building tests
// ---------------------------------------------------------------------------

fn make_comment(
    content: &str,
    target: AttachedReviewCommentTarget,
    outdated: bool,
) -> AttachedReviewComment {
    AttachedReviewComment {
        id: Default::default(),
        content: content.to_string(),
        target,
        last_update_time: Local::now(),
        base: None,
        head: None,
        outdated,
        origin: CommentOrigin::Native,
    }
}

fn batch(comments: Vec<AttachedReviewComment>) -> AgentReviewCommentBatch {
    AgentReviewCommentBatch {
        comments,
        diff_set: HashMap::new(),
    }
}

fn local_path(path: &str) -> LocalOrRemotePath {
    LocalOrRemotePath::Local(path.into())
}

// ---------------------------------------------------------------------------
// build_review_prompt tests
// ---------------------------------------------------------------------------

#[test]
fn test_build_review_prompt_current_line_is_1_indexed() {
    // LineCount 0 (0-indexed) should appear as L1 in the prompt.
    let comment = make_comment(
        "fix this",
        AttachedReviewCommentTarget::Line {
            absolute_file_path: local_path("/repo/src/main.rs"),
            line: EditorLineLocation::Current {
                line_number: LineCount::from(0),
                line_range: LineCount::from(0)..LineCount::from(1),
            },
            content: LineDiffContent::default(),
        },
        false,
    );
    let prompt = build_review_prompt(&batch(vec![comment]), &ReviewGitTarget::WorkingDirectory);
    assert!(
        prompt.contains("/repo/src/main.rs L1"),
        "expected 1-indexed L1, got: {prompt}",
    );
    assert!(prompt.contains("fix this"));
}

#[test]
fn test_build_review_prompt_removed_line_is_1_indexed() {
    let comment = make_comment(
        "why was this deleted?",
        AttachedReviewCommentTarget::Line {
            absolute_file_path: local_path("/repo/old.rs"),
            line: EditorLineLocation::Removed {
                line_number: LineCount::from(9),
                line_range: LineCount::from(9)..LineCount::from(10),
                index: 0,
            },
            content: LineDiffContent::default(),
        },
        false,
    );
    let prompt = build_review_prompt(&batch(vec![comment]), &ReviewGitTarget::WorkingDirectory);
    assert!(
        prompt.contains("(deleted, was L10"),
        "expected 1-indexed L10, got: {prompt}",
    );
}

#[test]
fn test_build_review_prompt_collapsed_range_is_1_indexed_start() {
    let comment = make_comment(
        "check this hunk",
        AttachedReviewCommentTarget::Line {
            absolute_file_path: local_path("/repo/lib.rs"),
            line: EditorLineLocation::Collapsed {
                line_range: LineCount::from(4)..LineCount::from(10),
            },
            content: LineDiffContent::default(),
        },
        false,
    );
    let prompt = build_review_prompt(&batch(vec![comment]), &ReviewGitTarget::WorkingDirectory);
    // line_range is [4, 10) 0-indexed -> L5-L10 (1-indexed, both ends inclusive)
    assert!(prompt.contains("L5-L10"), "expected L5-L10, got: {prompt}",);
}

#[test]
fn test_build_review_prompt_file_level_comment() {
    let comment = make_comment(
        "needs refactoring",
        AttachedReviewCommentTarget::File {
            absolute_file_path: local_path("/repo/src/utils.rs"),
        },
        false,
    );
    let prompt = build_review_prompt(&batch(vec![comment]), &ReviewGitTarget::WorkingDirectory);
    assert!(prompt.contains("/repo/src/utils.rs: needs refactoring"));
    // Not a deleted file (empty diff_set), so no "deleted file" text.
    assert!(!prompt.contains("deleted file"));
}

#[test]
fn test_build_review_prompt_deleted_file_comment() {
    let comment = make_comment(
        "why remove this?",
        AttachedReviewCommentTarget::File {
            absolute_file_path: local_path("/repo/src/old.rs"),
        },
        false,
    );
    let mut review = batch(vec![comment]);
    review.diff_set.insert(
        "src/old.rs".to_string(),
        vec![DiffSetHunk {
            line_range: LineCount::from(0)..LineCount::from(5),
            diff_content: String::new(),
            lines_added: 0,
            lines_removed: 5,
        }],
    );
    let prompt = build_review_prompt(&review, &ReviewGitTarget::WorkingDirectory);
    assert!(
        prompt.contains("(deleted file"),
        "expected deleted file annotation, got: {prompt}",
    );
}

#[test]
fn test_build_review_prompt_general_comment() {
    let comment = make_comment(
        "overall looks good",
        AttachedReviewCommentTarget::General,
        false,
    );
    let prompt = build_review_prompt(&batch(vec![comment]), &ReviewGitTarget::WorkingDirectory);
    assert!(prompt.contains("General: overall looks good"));
}

#[test]
fn test_build_review_prompt_skips_outdated_comments() {
    let active = make_comment("keep me", AttachedReviewCommentTarget::General, false);
    let outdated = make_comment("skip me", AttachedReviewCommentTarget::General, true);
    let prompt = build_review_prompt(
        &batch(vec![active, outdated]),
        &ReviewGitTarget::WorkingDirectory,
    );
    assert!(prompt.contains("keep me"));
    assert!(!prompt.contains("skip me"));
}

#[test]
fn test_build_review_prompt_multiple_comments() {
    let c1 = make_comment(
        "first",
        AttachedReviewCommentTarget::Line {
            absolute_file_path: local_path("/repo/a.rs"),
            line: EditorLineLocation::Current {
                line_number: LineCount::from(4),
                line_range: LineCount::from(4)..LineCount::from(5),
            },
            content: LineDiffContent::default(),
        },
        false,
    );
    let c2 = make_comment("second", AttachedReviewCommentTarget::General, false);
    let prompt = build_review_prompt(&batch(vec![c1, c2]), &ReviewGitTarget::WorkingDirectory);
    assert!(prompt.contains("/repo/a.rs L5: first"));
    assert!(prompt.contains("General: second"));
}

#[test]
fn test_build_review_prompt_exports_internal_markdown_without_punctuation_escapes() {
    let comment = make_comment("Fix this\\.", AttachedReviewCommentTarget::General, false);
    let prompt = build_review_prompt(&batch(vec![comment]), &ReviewGitTarget::WorkingDirectory);
    assert!(prompt.contains("General: Fix this."));
    assert!(!prompt.contains("Fix this\\."));
}

// ---------------------------------------------------------------------------
// build_diff_hunk_prompt tests
// ---------------------------------------------------------------------------

#[test]
fn test_build_diff_hunk_prompt_format() {
    let prompt = build_diff_hunk_prompt(
        "/repo/src/main.rs",
        10,
        20,
        3,
        2,
        &ReviewGitTarget::WorkingDirectory,
    );
    assert_eq!(
        prompt,
        "/repo/src/main.rs L10-L20 (+3 -2) -- run `git diff` to see the full context.",
    );
}

// ---------------------------------------------------------------------------
// build_selection_line_range_prompt tests
// ---------------------------------------------------------------------------

#[test]
fn test_build_selection_line_range_prompt_format() {
    let result = build_selection_line_range_prompt("src/foo.rs", 5, 10);
    assert_eq!(result, "src/foo.rs L5-L10");
}

#[test]
fn test_build_selection_substring_prompt_format() {
    let result = build_selection_substring_prompt("src/foo.rs", 5, "let x = 42;");
    assert_eq!(result, "src/foo.rs L5: let x = 42;");
}

#[test]
fn test_detect_known_agents() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            for (command, expected) in [
                ("claude", CLIAgent::Claude),
                ("gemini", CLIAgent::Gemini),
                ("codex", CLIAgent::Codex),
                ("amp", CLIAgent::Amp),
                ("droid", CLIAgent::Droid),
                ("opencode", CLIAgent::OpenCode),
                ("copilot", CLIAgent::Copilot),
                ("agent", CLIAgent::CursorCli),
                ("goose", CLIAgent::Goose),
                ("vibe", CLIAgent::Vibe),
                ("agy", CLIAgent::Antigravity),
                ("omp", CLIAgent::OhMyPi),
                ("grok", CLIAgent::Grok),
                ("muse", CLIAgent::Muse),
                ("warp", CLIAgent::WarpTui),
                ("warp-dev", CLIAgent::WarpTui),
                ("./script/run-tui", CLIAgent::WarpTui),
            ] {
                assert_eq!(
                    CLIAgent::detect(command, None, None, ctx),
                    Some(expected),
                    "failed to detect {command}",
                );
            }
        });
    });
}

#[test]
fn test_detect_with_arguments() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect("claude --model opus", None, None, ctx),
                Some(CLIAgent::Claude),
            );
            assert_eq!(
                CLIAgent::detect("gemini chat", None, None, ctx),
                Some(CLIAgent::Gemini),
            );
            assert_eq!(
                CLIAgent::detect(r#"muse exec "ping""#, None, None, ctx),
                Some(CLIAgent::Muse),
            );
            assert_eq!(CLIAgent::detect("npx muse", None, None, ctx), None);
        });
    });
}

#[test]
fn test_grok_public_configuration() {
    assert_eq!(CLIAgent::Grok.command_prefix(), "grok");
    assert_eq!(CLIAgent::Grok.display_name(), "Grok Build");
    assert!(CLIAgent::Grok.supports_bash_mode());
    assert_eq!(CLIAgent::Grok.skill_command_prefix(), "/");
    assert_eq!(CLIAgent::Grok.icon(), Some(Icon::GrokLogo));
}

#[test]
fn test_muse_public_configuration() {
    assert_eq!(CLIAgent::Muse.command_prefix(), "muse");
    assert_eq!(CLIAgent::Muse.display_name(), "Muse Code");
    assert!(CLIAgent::Muse.supports_bash_mode());
    assert_eq!(CLIAgent::Muse.skill_command_prefix(), "/");
    assert_eq!(CLIAgent::Muse.icon(), Some(Icon::MuseLogo));
    assert_eq!(
        CLIAgent::Muse.supported_skill_providers(),
        &[
            SkillProvider::Agents,
            SkillProvider::Claude,
            SkillProvider::Codex,
        ]
    );
}

#[test]
fn test_detect_vibe_acp_binary() {
    // The mistral-vibe package ships a `vibe-acp` ACP-mode binary alongside
    // the user-facing `vibe` TUI. Both must be detected as the same agent.
    App::test((), |mut app| async move {
        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect("vibe-acp", None, None, ctx),
                Some(CLIAgent::Vibe),
            );
            assert_eq!(
                CLIAgent::detect("vibe-acp --some-flag", None, None, ctx),
                Some(CLIAgent::Vibe),
            );
            // Distinct binary names should not bleed into Vibe.
            assert_eq!(CLIAgent::detect("vibe-other", None, None, ctx), None);
        });
    });
}

#[test]
fn test_detect_with_leading_whitespace() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect("  claude", None, None, ctx),
                Some(CLIAgent::Claude),
            );
            assert_eq!(
                CLIAgent::detect("\tclaude --help", None, None, ctx),
                Some(CLIAgent::Claude),
            );
        });
    });
}

#[test]
fn test_detect_no_match() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            assert_eq!(CLIAgent::detect("ls -la", None, None, ctx), None);
            assert_eq!(CLIAgent::detect("vim", None, None, ctx), None);
            assert_eq!(CLIAgent::detect("claude_wrapper", None, None, ctx), None);
        });
    });
}

#[test]
fn test_detect_with_alias() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            let map = aliases(&[("c", "claude")]);
            assert_eq!(
                CLIAgent::detect("c", None, Some(&map), ctx),
                Some(CLIAgent::Claude),
            );
            assert_eq!(
                CLIAgent::detect("c --help", None, Some(&map), ctx),
                Some(CLIAgent::Claude),
            );

            let map = aliases(&[("o", "omp")]);
            assert_eq!(
                CLIAgent::detect("o", None, Some(&map), ctx),
                Some(CLIAgent::OhMyPi),
            );
        });
    });
}

#[test]
fn test_detect_alias_not_matching() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            let map = aliases(&[("c", "cat")]);
            assert_eq!(CLIAgent::detect("c", None, Some(&map), ctx), None);
        });
    });
}

#[test]
fn test_detect_alias_multi_word_value() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            // Alias whose value starts with "gemini" but has extra words
            let map = aliases(&[("g", "gemini chat --verbose")]);
            assert_eq!(
                CLIAgent::detect("g", None, Some(&map), ctx),
                Some(CLIAgent::Gemini),
            );
        });
    });
}

#[test]
fn test_detect_with_env_var_prefix() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect(
                    "EXAMPLE=true opencode",
                    Some(EscapeChar::Backslash),
                    None,
                    ctx,
                ),
                Some(CLIAgent::OpenCode),
            );
            assert_eq!(
                CLIAgent::detect("FOO=1 omp", Some(EscapeChar::Backslash), None, ctx,),
                Some(CLIAgent::OhMyPi),
            );
        });
    });
}

#[test]
fn test_detect_with_multiple_env_vars() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect(
                    "FOO=1 BAR=2 opencode --flag",
                    Some(EscapeChar::Backslash),
                    None,
                    ctx,
                ),
                Some(CLIAgent::OpenCode),
            );
        });
    });
}

#[test]
fn test_detect_with_alias_and_env_var() {
    App::test((), |mut app| async move {
        app.update(|ctx| {
            let map = aliases(&[("oc", "EXAMPLE=1 opencode")]);
            assert_eq!(
                CLIAgent::detect("oc --flag", Some(EscapeChar::Backslash), Some(&map), ctx,),
                Some(CLIAgent::OpenCode),
            );
        });
    });
}

/// Creates a workspace containing a team with the given UID.
fn workspace_with_team_uid(uid: &str) -> Workspace {
    Workspace::from_local_cache(
        ServerId::from_string_lossy("test-workspace-uid-001").into(),
        "Test Workspace".to_string(),
        Some(vec![Team::from_local_cache(
            ServerId::from_string_lossy(uid),
            "Test Team".to_string(),
            None,
            None,
            None,
            None,
        )]),
        None,
    )
}

#[test]
fn test_detect_aifx_agent_run_claude_on_uber_team() {
    App::test((), |mut app| async move {
        let uber_workspace = workspace_with_team_uid(UBER_TEAM_UID);
        app.add_singleton_model(|ctx| {
            UserWorkspaces::mock(
                Arc::new(MockTeamClient::new()),
                Arc::new(MockWorkspaceClient::new()),
                vec![uber_workspace],
                ctx,
            )
        });

        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect("aifx agent run claude", None, None, ctx),
                Some(CLIAgent::Claude),
            );
            // With extra args
            assert_eq!(
                CLIAgent::detect("aifx agent run claude --verbose", None, None, ctx),
                Some(CLIAgent::Claude),
            );
        });
    });
}

#[test]
fn test_detect_aifx_agent_run_claude_via_alias_on_uber_team() {
    App::test((), |mut app| async move {
        let uber_workspace = workspace_with_team_uid(UBER_TEAM_UID);
        app.add_singleton_model(|ctx| {
            UserWorkspaces::mock(
                Arc::new(MockTeamClient::new()),
                Arc::new(MockWorkspaceClient::new()),
                vec![uber_workspace],
                ctx,
            )
        });

        app.update(|ctx| {
            let map = aliases(&[("ai", "aifx agent run claude")]);
            assert_eq!(
                CLIAgent::detect("ai", None, Some(&map), ctx),
                Some(CLIAgent::Claude),
            );
            assert_eq!(
                CLIAgent::detect("ai --flag", None, Some(&map), ctx),
                Some(CLIAgent::Claude),
            );
        });
    });
}

#[test]
fn test_detect_aifx_agent_run_claude_not_on_uber_team() {
    App::test((), |mut app| async move {
        // Register UserWorkspaces with no Uber team membership
        app.add_singleton_model(UserWorkspaces::default_mock);

        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect("aifx agent run claude", None, None, ctx),
                None,
            );
        });
    });
}

#[test]
fn test_serialized_name_round_trips_known_agents() {
    for agent in enum_iterator::all::<CLIAgent>() {
        let name = agent.to_serialized_name();
        if agent == CLIAgent::Unknown {
            assert_eq!(name, "Unknown");
        } else {
            assert!(!name.is_empty(), "empty serialized name for {agent:?}");
        }
        assert_eq!(
            CLIAgent::from_serialized_name(&name),
            agent,
            "round-trip failed for {agent:?} with serialized name {name:?}",
        );
    }
}

#[test]
fn test_from_serialized_name_falls_back_to_unknown() {
    assert_eq!(CLIAgent::from_serialized_name(""), CLIAgent::Unknown);
    assert_eq!(
        CLIAgent::from_serialized_name("nonexistent"),
        CLIAgent::Unknown
    );
}

#[test]
fn test_detect_aifx_agent_run_claude_wrong_team() {
    App::test((), |mut app| async move {
        let other_workspace = workspace_with_team_uid("some-other-team-uid-01");
        app.add_singleton_model(|ctx| {
            UserWorkspaces::mock(
                Arc::new(MockTeamClient::new()),
                Arc::new(MockWorkspaceClient::new()),
                vec![other_workspace],
                ctx,
            )
        });

        app.update(|ctx| {
            assert_eq!(
                CLIAgent::detect("aifx agent run claude", None, None, ctx),
                None,
            );
        });
    });
}

#[test]
fn test_oh_my_pi_supports_bash_mode() {
    assert!(CLIAgent::OhMyPi.supports_bash_mode());
}

#[test]
fn test_warp_tui_matches_binaries_and_launchers() {
    // Direct binary names.
    assert!(CLIAgent::WarpTui.matches_command("warp", None));
    assert!(CLIAgent::WarpTui.matches_command("warp-preview", None));
    assert!(CLIAgent::WarpTui.matches_command("warp-dev", None));
    assert!(CLIAgent::WarpTui.matches_command("warp-tui", None));
    assert!(CLIAgent::WarpTui.matches_command("warp-tui-oss", None));
    // The dev launcher script.
    assert!(CLIAgent::WarpTui.matches_command("./script/run-tui", None));
    assert!(CLIAgent::WarpTui.matches_command("script/run-tui", None));
    // Absolute / relative paths to the binary.
    assert!(CLIAgent::WarpTui.matches_command("/workspace/warp/target/debug/warp-tui", None,));
    assert!(CLIAgent::WarpTui.matches_command("./target/debug/warp-tui", None));
    assert!(CLIAgent::WarpTui.matches_command(
        "/Applications/WarpPreview.app/Contents/MacOS/warp-preview --resume abc",
        None,
    ));
    // With arguments and leading whitespace.
    assert!(CLIAgent::WarpTui.matches_command("  warp --resume abc", None));
}

#[test]
fn test_warp_tui_matches_with_env_var_prefix() {
    // Env-var assignments before the command are skipped when an escape char is
    // provided (mirrors `CLIAgent::detect`).
    assert!(
        CLIAgent::WarpTui.matches_command("WARP_API_KEY=secret warp", Some(EscapeChar::Backslash),)
    );
}

#[test]
fn test_warp_tui_does_not_match_other_commands() {
    assert!(!CLIAgent::WarpTui.matches_command("vim", None));
    assert!(!CLIAgent::WarpTui.matches_command("htop", None));
    assert!(!CLIAgent::WarpTui.matches_command("claude", None));
    // Lookalikes / substrings should not match.
    assert!(!CLIAgent::WarpTui.matches_command("warp-preview-wrapper", None));
    assert!(!CLIAgent::WarpTui.matches_command("mywarp-dev", None));
    assert!(!CLIAgent::WarpTui.matches_command("warp-tui-wrapper", None));
    assert!(!CLIAgent::WarpTui.matches_command("mywarp-tui", None));
    assert!(!CLIAgent::WarpTui.matches_command("", None));
    // `cargo run` is a known non-match (the first token is `cargo`).
    assert!(!CLIAgent::WarpTui.matches_command("cargo run -p warp_tui", None));
}

#[test]
fn test_warp_tui_variant_properties() {
    assert!(CLIAgent::Claude.supports_cli_agent_footer());
    assert_eq!(CLIAgent::WarpTui.command_prefix(), "warp");
    assert_eq!(
        CLIAgent::WarpTui.command_prefixes(),
        &[
            "warp",
            "warp-preview",
            "warp-dev",
            "warp-tui",
            "warp-tui-oss",
            "run-tui",
        ]
    );
    assert_eq!(CLIAgent::WarpTui.display_name(), "Warp TUI");
    assert_eq!(CLIAgent::WarpTui.brand_color(), Some(ColorU::black()));
    assert_eq!(CLIAgent::WarpTui.icon(), Some(Icon::Warp));
    assert_eq!(CLIAgent::WarpTui.brand_icon_color(), ColorU::white());
    assert!(CLIAgent::WarpTui.supported_skill_providers().is_empty());
    assert!(!CLIAgent::WarpTui.supports_bash_mode());
    assert!(!CLIAgent::WarpTui.supports_cli_agent_footer());
    // Serialized name round-trips (also covered by
    // `test_serialized_name_round_trips_known_agents`, asserted explicitly here).
    assert_eq!(
        CLIAgent::from_serialized_name(&CLIAgent::WarpTui.to_serialized_name()),
        CLIAgent::WarpTui
    );
}

/// A batch with every comment kind whose prompt text names a `git diff` hint.
fn review_with_diff_hints(root: &str) -> AgentReviewCommentBatch {
    let removed = make_comment(
        "why gone",
        AttachedReviewCommentTarget::Line {
            absolute_file_path: local_path(&format!("{root}/old.rs")),
            line: EditorLineLocation::Removed {
                line_number: LineCount::from(1),
                line_range: LineCount::from(1)..LineCount::from(2),
                index: 0,
            },
            content: LineDiffContent::default(),
        },
        false,
    );
    let collapsed = make_comment(
        "hunk",
        AttachedReviewCommentTarget::Line {
            absolute_file_path: local_path(&format!("{root}/lib.rs")),
            line: EditorLineLocation::Collapsed {
                line_range: LineCount::from(0)..LineCount::from(3),
            },
            content: LineDiffContent::default(),
        },
        false,
    );
    let deleted_file = make_comment(
        "file gone",
        AttachedReviewCommentTarget::File {
            absolute_file_path: local_path(&format!("{root}/src/old.rs")),
        },
        false,
    );
    let mut review = batch(vec![removed, collapsed, deleted_file]);
    review.diff_set.insert(
        "src/old.rs".to_string(),
        vec![DiffSetHunk {
            line_range: LineCount::from(0)..LineCount::from(5),
            diff_content: String::new(),
            lines_added: 0,
            lines_removed: 5,
        }],
    );
    review
}

#[test]
fn test_build_review_prompt_in_repo_text_is_characterized() {
    let prompt = build_review_prompt(
        &review_with_diff_hints("/repo"),
        &ReviewGitTarget::WorkingDirectory,
    );
    assert_eq!(
        prompt,
        "Please address the following code review comments. \
         Run `git diff` (or `git diff HEAD`) to see the full context of any changes, \
         especially for deleted lines.\n\
         \n- /repo/old.rs (deleted, was L2 — see `git diff`): why gone\
         \n- /repo/lib.rs (collapsed hunk, L1-L3 — see `git diff`): hunk\
         \n- /repo/src/old.rs (deleted file — see `git diff`): file gone",
    );
}

#[test]
fn test_build_review_prompt_outside_repo_names_repo_in_git_hints() {
    let target = ReviewGitTarget::Repo {
        root: PathBuf::from("/w/eventric/mobile"),
        base_branch: Some("main".to_string()),
    };
    let prompt = build_review_prompt(&review_with_diff_hints("/w/eventric/mobile"), &target);
    let diff = "git -C /w/eventric/mobile diff $(git -C /w/eventric/mobile merge-base HEAD main)";
    assert_eq!(
        prompt,
        format!(
            "Please address the following code review comments in the git repo at \
             `/w/eventric/mobile`. Run `{diff}` to see the full context of any changes, \
             especially for deleted lines.\n\
             \n- /w/eventric/mobile/old.rs (deleted, was L2 — see `{diff}`): why gone\
             \n- /w/eventric/mobile/lib.rs (collapsed hunk, L1-L3 — see `{diff}`): hunk\
             \n- /w/eventric/mobile/src/old.rs (deleted file — see `{diff}`): file gone"
        ),
    );
    assert!(!prompt.contains("`git diff`"));
}

#[test]
fn test_build_review_prompt_outside_repo_head_mode_and_quoting() {
    let target = ReviewGitTarget::Repo {
        root: PathBuf::from("/w/my repos/mobile"),
        base_branch: None,
    };
    let prompt = build_review_prompt(&review_with_diff_hints("/w/my repos/mobile"), &target);
    assert!(
        prompt.contains("Run `git -C '/w/my repos/mobile' diff HEAD`"),
        "got: {prompt}",
    );
}

#[test]
fn test_review_git_target_for_terminal_inside_repo_uses_working_directory() {
    let root = Path::new("/w/eventric/mobile");
    for cwd in [Some(root), Some(Path::new("/w/eventric/mobile/src")), None] {
        assert_eq!(
            ReviewGitTarget::for_terminal(root, cwd, &DiffMode::MainBranch, Some("main")),
            ReviewGitTarget::WorkingDirectory,
        );
    }
}

#[test]
fn test_review_git_target_for_terminal_outside_repo_names_base() {
    let root = Path::new("/w/eventric/mobile");
    let cwd = Some(Path::new("/w/eventric"));
    let repo = |base_branch: Option<&str>| ReviewGitTarget::Repo {
        root: root.to_path_buf(),
        base_branch: base_branch.map(str::to_owned),
    };
    assert_eq!(
        ReviewGitTarget::for_terminal(root, cwd, &DiffMode::MainBranch, Some("master")),
        repo(Some("master")),
    );
    assert_eq!(
        ReviewGitTarget::for_terminal(root, cwd, &DiffMode::Head, Some("master")),
        repo(None),
    );
    assert_eq!(
        ReviewGitTarget::for_terminal(
            root,
            cwd,
            &DiffMode::OtherBranch("dev".to_string()),
            Some("master"),
        ),
        repo(Some("dev")),
    );
}

#[test]
fn test_build_diff_hunk_prompt_outside_repo_is_absolute() {
    let target = ReviewGitTarget::Repo {
        root: PathBuf::from("/w/eventric/mobile"),
        base_branch: None,
    };
    assert_eq!(
        build_diff_hunk_prompt("src/main.rs", 10, 20, 3, 2, &target),
        "/w/eventric/mobile/src/main.rs L10-L20 (+3 -2) \
         -- run `git -C /w/eventric/mobile diff HEAD` to see the full context.",
    );
}
