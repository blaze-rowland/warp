# tmux control mode — deltas from the PR #14864 spec

**Issue:** [warpdotdev/warp#8492](https://github.com/warpdotdev/warp/issues/8492)
**Baseline:** `specs/GH8492/product.md` and `tech.md` from PR #14864.
**Full plan:** `docs/plans/2026-10-09-1516-feat-warp-tmux-native-plan.md` (requirements R1–R30).

This fork implements #14864 and goes beyond it where tmux users told us the baseline still leaves tmux "just like any other terminal." Every behavior below that departs from #14864 is its own setting, so the baseline can merge without any of them.

## Deltas behind their own settings

| Setting | Default | Baseline (#14864) | This fork | Why |
|---|---|---|---|---|
| `terminal.tmux.auto_convert` | on | Only hand-typed `tmux -CC` converts, after a click on a banner | Typed `tmux`, `tmux a`, `tmux new …` gain `-CC` and open natively without a click. Non-attaching subcommands (`ls`, `kill-*`, `new -d`), pipelines, chains and AI-issued commands are never rewritten | Users type `tmux a` out of habit; a banner per attach is friction |
| `terminal.tmux.integrate_panes` | on | Invariant #12: panes are plain unless the user sources Warp's integration | New panes start through a self-contained launcher (marker-only bootstrap that leaves PS1 and the line editor alone). Warp types the init only when it is the sole client; otherwise "Warpify this pane" | Blocks, AI and completions are the reason to use Warp inside tmux |
| `terminal.tmux.bindings_take_precedence` | on | Warp shortcuts win; tmux bindings are not honored | The attached server's key tables are mirrored; prefix, root-bound and in-table keys go through `send-keys -K` (tmux 3.4+) with a post-ack fence, so `if-shell` bindings (vim-tmux-navigator) and `bind -r` behave as in plain tmux. Reserved Warp actions and an escape-hatch chord stay with Warp | Muscle memory |

## Additions not in the baseline

- **Native tmux interactive UI.** Bound `confirm-before`, `command-prompt`, `display-menu`, `choose-tree`, `display-panes` and `display-popup` render natively; `%message` becomes a toast. Warp never sends blocking UI commands from its own control client (they stall its command queue).
- **Script-opened popups.** On tmux 3.7c, `display-popup` from a script targets the most recently active client; on a control client it runs hidden, blocks the caller and captures `send-keys -K` input, with no notification. A `tmux` wrapper first on PATH turns `popup`/`display-popup` into a blocking `new-window` + `wait-for`, shown as a floating pane; a probe detects remaining hidden popups and offers to close them.
- **Remote parity.** Tmux panes inside a Warpified SSH session map to the origin session's host in the app layer (never registered as remote-server sessions) and never get an in-band command executor.
- **Plugin.** `run-shell '~/.cache/warp/tmux/current/warp.tmux 2>/dev/null || true'` makes panes tmux starts on its own (resurrect restores, other clients' splits) Warp-ready. While Warp is the only client, it runs tmux-resurrect saves on continuum's interval, because continuum's status-line trigger never fires for control clients.
- **Hook trust.** Pane session ids derive from a per-server token, so pre-attach and restored panes register without relaxing hook session-id validation.
- **warpui pre-keymap claim hook** (`View::claims_keystroke`), generic and tmux-free, so it can be reviewed and merged on its own.

## Session lifecycle choices

- Converted sessions open as tabs in the current window at the origin tab's position; the origin tab hides and returns on detach with a status line.
- Quitting Warp or closing a window detaches; closing a tmux tab or split kills that window or pane (with confirmation when something is running).
- tmux tabs are never persisted to app state.

## Not yet verified

- The `crates/integration` GUI suite was not run: the full app test harness (`initialize_app_for_terminal_view`, `workspace::view` tests) hangs on the development machine, including on unmodified master. View glue is covered by hand testing in the dev build.
- fish was not installed on the development machine; fish paths are untested.
