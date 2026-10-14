use super::*;
use crate::settings_view::settings_page::search_terms_match;

fn matching(query: &str) -> Vec<&'static str> {
    tmux_widgets()
        .iter()
        .filter(|widget| search_terms_match(widget.search_terms(), query))
        .map(|widget| widget.widget_id())
        .collect()
}

#[test]
fn each_tmux_setting_row_is_its_own_search_result() {
    let widgets = tmux_widgets();
    assert_eq!(widgets.len(), TmuxToggle::iter().count() + 1);
    let mut ids: Vec<_> = widgets.iter().map(|widget| widget.widget_id()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), widgets.len(), "every row has its own widget id");

    // The section's name finds every row.
    assert_eq!(matching("tmux").len(), widgets.len());
    assert_eq!(matching("").len(), widgets.len());

    assert_eq!(
        matching("plugin"),
        [TmuxPluginWidget::static_widget_id()]
    );
    assert_eq!(matching("resurrect"), [TmuxPluginWidget::static_widget_id()]);
    assert_eq!(
        matching("auto convert"),
        [tmux_toggle_widget_id(TmuxToggle::AutoConvert)]
    );
    assert_eq!(
        matching("key bindings"),
        [tmux_toggle_widget_id(TmuxToggle::BindingsTakePrecedence)]
    );
    assert_eq!(
        matching("control mode"),
        [tmux_toggle_widget_id(TmuxToggle::ControlMode)]
    );
    assert_eq!(
        matching("integration panes"),
        [tmux_toggle_widget_id(TmuxToggle::IntegratePanes)]
    );
}
