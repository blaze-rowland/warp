use settings::Setting;
use settings::schema::SettingSchemaEntry;
use strum::IntoEnumIterator;
use warp_core::features::FeatureFlag;
use warpui::keymap::DescriptionContext;
use warpui::keymap::macros::id;
use warpui::{App, SingletonEntity};
use warpui_extras::user_preferences::toml_backed::TomlBackedUserPreferences;

use super::{
    TmuxAutoConvert, TmuxBindingsTakePrecedence, TmuxControlModeSetting, TmuxIntegratePanes,
    TmuxSettings, TmuxToggle,
};
use crate::settings::schema_generation::settings_schema_json;
use crate::settings_view::SettingsAction;
use crate::settings_view::warpify_page;
use crate::test_util::settings::initialize_settings_for_tests;

/// Schema entries for the tmux settings, found by their TOML location.
fn tmux_schema_entries() -> Vec<&'static SettingSchemaEntry> {
    inventory::iter::<SettingSchemaEntry>
        .into_iter()
        .filter(|entry| match entry.hierarchy {
            Some("terminal") => entry.storage_key == "tmux_control_mode",
            Some("terminal.tmux") => true,
            _ => false,
        })
        .collect()
}

fn set_all(app: &mut App, value: bool) {
    TmuxSettings::handle(app).update(app, |settings, ctx| {
        settings.tmux_control_mode.set_value(value, ctx).unwrap();
        settings.auto_convert.set_value(value, ctx).unwrap();
        settings.integrate_panes.set_value(value, ctx).unwrap();
        settings
            .bindings_take_precedence
            .set_value(value, ctx)
            .unwrap();
    });
}

fn assert_all_accessors(app: &App, expected: bool) {
    TmuxSettings::handle(app).read(app, |settings, _| {
        assert_eq!(settings.control_mode_enabled(), expected);
        assert_eq!(settings.auto_convert_enabled(), expected);
        assert_eq!(settings.integrate_panes_enabled(), expected);
        assert_eq!(settings.bindings_take_precedence_enabled(), expected);
    });
}

/// Every palette description registered by the Warpify page, lowercased.
fn palette_descriptions(flag_enabled: bool) -> Vec<String> {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(flag_enabled);
    let descriptions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let out = descriptions.clone();
    App::test((), |mut app| async move {
        initialize_settings_for_tests(&mut app);
        app.update(|ctx| {
            warpify_page::init_actions_from_parent_view(ctx, &id!("Workspace"), |action| {
                action as SettingsAction
            });
            *out.lock().unwrap() = ctx
                .get_key_bindings()
                .filter_map(|binding| binding.description)
                .map(|description| {
                    description
                        .resolve(ctx, DescriptionContext::Default)
                        .to_lowercase()
                })
                .collect();
        });
    });
    std::sync::Arc::try_unwrap(descriptions)
        .expect("test app has finished")
        .into_inner()
        .unwrap()
}

#[test]
fn tmux_settings_are_hidden_from_schema_when_flag_is_off() {
    let entries = tmux_schema_entries();
    assert_eq!(
        entries.len(),
        4,
        "expected the master and three delta settings"
    );
    for entry in &entries {
        assert_eq!(entry.feature_flag, Some(FeatureFlag::TmuxControlMode));
    }

    let schema: serde_json::Value =
        serde_json::from_str(&settings_schema_json(|_| false).unwrap()).unwrap();
    let terminal = &schema["properties"]["terminal"]["properties"];
    assert!(terminal.get("tmux_control_mode").is_none());
    assert!(terminal.get("tmux").is_none());

    let schema: serde_json::Value = serde_json::from_str(
        &settings_schema_json(|flag| flag == FeatureFlag::TmuxControlMode).unwrap(),
    )
    .unwrap();
    let terminal = &schema["properties"]["terminal"]["properties"];
    assert!(terminal.get("tmux_control_mode").is_some());
    let tmux = &terminal["tmux"]["properties"];
    for key in [
        "auto_convert",
        "integrate_panes",
        "bindings_take_precedence",
    ] {
        assert!(tmux.get(key).is_some(), "missing terminal.tmux.{key}");
    }
}

#[test]
fn defaults_are_master_off_and_deltas_on() {
    assert!(!TmuxControlModeSetting::default_value());
    assert!(TmuxAutoConvert::default_value());
    assert!(TmuxIntegratePanes::default_value());
    assert!(TmuxBindingsTakePrecedence::default_value());
}

#[test]
fn accessors_are_inert_when_flag_is_off() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(false);
    App::test((), |mut app| async move {
        initialize_settings_for_tests(&mut app);
        assert_all_accessors(&app, false);
        set_all(&mut app, true);
        assert_all_accessors(&app, false);
    });
}

#[test]
fn accessors_follow_master_setting_when_flag_is_on() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        initialize_settings_for_tests(&mut app);
        // Master defaults off, so every delta is inert.
        assert_all_accessors(&app, false);

        // Turning the master on enables every delta at its default.
        TmuxSettings::handle(&app).update(&mut app, |settings, ctx| {
            settings.tmux_control_mode.set_value(true, ctx).unwrap();
        });
        assert_all_accessors(&app, true);

        // Each delta can be turned off on its own (R21).
        TmuxSettings::handle(&app).update(&mut app, |settings, ctx| {
            settings.auto_convert.set_value(false, ctx).unwrap();
        });
        TmuxSettings::handle(&app).read(&app, |settings, _| {
            assert!(settings.control_mode_enabled());
            assert!(!settings.auto_convert_enabled());
            assert!(settings.integrate_panes_enabled());
            assert!(settings.bindings_take_precedence_enabled());
        });
    });
}

#[test]
fn each_setting_round_trips_through_its_toml_path() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("settings.toml");

    {
        let (prefs, _) = TomlBackedUserPreferences::new(file_path.clone());
        // Write the non-default value of each setting.
        TmuxControlModeSetting::write_to_preferences(&true, &prefs).unwrap();
        TmuxAutoConvert::write_to_preferences(&false, &prefs).unwrap();
        TmuxIntegratePanes::write_to_preferences(&false, &prefs).unwrap();
        TmuxBindingsTakePrecedence::write_to_preferences(&false, &prefs).unwrap();
    }

    let contents = std::fs::read_to_string(&file_path).unwrap();
    let parsed: toml::Value = toml::from_str(&contents).unwrap();
    assert_eq!(
        parsed["terminal"]["tmux_control_mode"],
        toml::Value::Boolean(true),
        "{contents}"
    );
    for key in [
        "auto_convert",
        "integrate_panes",
        "bindings_take_precedence",
    ] {
        assert_eq!(
            parsed["terminal"]["tmux"][key],
            toml::Value::Boolean(false),
            "terminal.tmux.{key} in {contents}"
        );
    }

    let (prefs, _) = TomlBackedUserPreferences::new(file_path);
    assert_eq!(
        TmuxControlModeSetting::read_from_preferences(&prefs),
        Some(true)
    );
    assert_eq!(TmuxAutoConvert::read_from_preferences(&prefs), Some(false));
    assert_eq!(
        TmuxIntegratePanes::read_from_preferences(&prefs),
        Some(false)
    );
    assert_eq!(
        TmuxBindingsTakePrecedence::read_from_preferences(&prefs),
        Some(false)
    );
}

#[test]
fn every_toggleable_setting_has_a_command_palette_entry() {
    // One toggle per tmux setting.
    assert_eq!(TmuxToggle::iter().count(), tmux_schema_entries().len());

    let descriptions = palette_descriptions(true);
    for toggle in TmuxToggle::iter() {
        let label = toggle.label().to_lowercase();
        for verb in ["enable", "disable"] {
            let expected = format!("{verb} {label}");
            assert!(
                descriptions.contains(&expected),
                "missing palette entry {expected:?}"
            );
        }
    }
}

#[test]
fn no_command_palette_entries_when_flag_is_off() {
    let descriptions = palette_descriptions(false);
    for toggle in TmuxToggle::iter() {
        let label = toggle.label().to_lowercase();
        assert!(
            !descriptions.iter().any(|d| d.ends_with(&label)),
            "unexpected palette entry for {label:?}"
        );
    }
}
