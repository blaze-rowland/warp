//! tmux window layouts as Warp pane trees (U7, KTD4).
//!
//! tmux's layout is the source of truth for geometry. A layout becomes a [`PaneNodeSnapshot`]
//! whose leaves are [`LeafContents::TmuxPane`], opened through the snapshot-restore path, and
//! later `%layout-change`s apply as a diff keyed by pane id ([`plan_pane_update`]) so existing
//! pane models are never recreated. Flexes come from tmux's cell sizes; a divider drag in Warp
//! goes back to tmux as `resize-pane` ([`resize_commands`]).

use std::collections::BTreeSet;

use warp_terminal::tmux::{LayoutNode, PaneId, WindowLayout};

use super::TmuxOriginId;
use crate::app_state::{
    BranchSnapshot, LeafContents, LeafSnapshot, PaneFlex, PaneNodeSnapshot, SplitDirection,
    TmuxPaneSnapshot,
};

/// Below this difference (in flex units) Warp and tmux agree on a split.
const FLEX_EPSILON: f32 = 1e-3;

/// The layout a Warp tab shows for a window: tmux's visible layout, so a zoomed window shows only
/// its zoomed pane.
pub fn tab_layout(layout: &WindowLayout) -> &LayoutNode {
    &layout.visible_layout
}

/// The pane tree for `node`. `focused` marks the window's active pane; `current_command` gives
/// each pane's `#{pane_current_command}` when known.
pub fn layout_to_snapshot(
    origin: TmuxOriginId,
    node: &LayoutNode,
    focused: Option<PaneId>,
    current_command: &dyn Fn(PaneId) -> Option<String>,
) -> PaneNodeSnapshot {
    match node {
        LayoutNode::Leaf(leaf) => PaneNodeSnapshot::Leaf(LeafSnapshot {
            is_focused: focused == Some(leaf.pane_id),
            custom_vertical_tabs_title: None,
            contents: LeafContents::TmuxPane(TmuxPaneSnapshot {
                origin,
                pane_id: leaf.pane_id,
                current_command: current_command(leaf.pane_id),
            }),
        }),
        LayoutNode::Split {
            horizontal,
            children,
            ..
        } => {
            let flexes = child_flexes(children, *horizontal);
            PaneNodeSnapshot::Branch(BranchSnapshot {
                direction: split_direction(*horizontal),
                children: flexes
                    .into_iter()
                    .zip(children)
                    .map(|(flex, child)| {
                        (
                            PaneFlex(flex),
                            layout_to_snapshot(origin, child, focused, current_command),
                        )
                    })
                    .collect(),
            })
        }
    }
}

/// Warp's split direction for a tmux split. tmux's `{}` (horizontal) puts panes side by side,
/// which Warp calls a horizontal split too.
pub fn split_direction(horizontal: bool) -> SplitDirection {
    if horizontal {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    }
}

/// Flexes for a split's children, proportional to their cell sizes along the split and averaging
/// 1.0 like Warp's own splits.
pub fn child_flexes(children: &[LayoutNode], horizontal: bool) -> Vec<f32> {
    let sizes: Vec<f32> = children
        .iter()
        .map(|child| size_along(child, horizontal) as f32)
        .collect();
    let total: f32 = sizes.iter().sum();
    if total <= 0.0 {
        return vec![1.0; children.len()];
    }
    let count = children.len() as f32;
    sizes.iter().map(|size| size * count / total).collect()
}

/// A layout's structure: which pane is where, and each split's flexes.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutShape {
    Leaf(PaneId),
    Split {
        horizontal: bool,
        children: Vec<(f32, LayoutShape)>,
    },
}

impl LayoutShape {
    /// The panes left to right, top to bottom.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        match self {
            Self::Leaf(pane_id) => vec![*pane_id],
            Self::Split { children, .. } => children
                .iter()
                .flat_map(|(_, child)| child.pane_ids())
                .collect(),
        }
    }

    /// Whether both shapes put the same panes in the same splits, ignoring sizes.
    pub fn same_structure(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Leaf(a), Self::Leaf(b)) => a == b,
            (
                Self::Split {
                    horizontal: a_horizontal,
                    children: a_children,
                },
                Self::Split {
                    horizontal: b_horizontal,
                    children: b_children,
                },
            ) => {
                a_horizontal == b_horizontal
                    && a_children.len() == b_children.len()
                    && a_children
                        .iter()
                        .zip(b_children)
                        .all(|((_, a), (_, b))| a.same_structure(b))
            }
            _ => false,
        }
    }

    /// Whether both shapes have the same structure and the same flexes.
    fn same_sizes(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Leaf(_), Self::Leaf(_)) => true,
            (
                Self::Split {
                    children: a_children,
                    ..
                },
                Self::Split {
                    children: b_children,
                    ..
                },
            ) => a_children
                .iter()
                .zip(b_children)
                .all(|((a_flex, a), (b_flex, b))| {
                    (a_flex - b_flex).abs() < FLEX_EPSILON && a.same_sizes(b)
                }),
            _ => false,
        }
    }
}

/// The shape of `node`.
pub fn layout_shape(node: &LayoutNode) -> LayoutShape {
    match node {
        LayoutNode::Leaf(leaf) => LayoutShape::Leaf(leaf.pane_id),
        LayoutNode::Split {
            horizontal,
            children,
            ..
        } => LayoutShape::Split {
            horizontal: *horizontal,
            children: child_flexes(children, *horizontal)
                .into_iter()
                .zip(children)
                .map(|(flex, child)| (flex, layout_shape(child)))
                .collect(),
        },
    }
}

/// What a layout change does to a tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpdateKind {
    /// Same panes, same places, same sizes.
    #[default]
    Unchanged,
    /// Same panes in the same places; only split sizes changed.
    Resize,
    /// Panes were added, removed or moved: rebuild the tree from the existing pane models.
    Restructure,
}

/// The diff between a tab and a new layout, keyed by pane id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneUpdatePlan {
    /// Panes to create, in layout order.
    pub added: Vec<PaneId>,
    /// Panes whose Warp panes go away (tmux already removed them; nothing is sent).
    pub removed: Vec<PaneId>,
    pub kind: UpdateKind,
}

/// Plans applying `next` to a tab that shows `current` (the last applied shape, if any) and
/// holds Warp panes for `present`. Applying the same layout twice plans nothing, so a layout
/// echoed back after a Warp-initiated split never creates a second pane.
pub fn plan_pane_update(
    current: Option<&LayoutShape>,
    present: &[PaneId],
    next: &LayoutShape,
) -> PaneUpdatePlan {
    let next_panes = next.pane_ids();
    let present_set: BTreeSet<PaneId> = present.iter().copied().collect();
    let next_set: BTreeSet<PaneId> = next_panes.iter().copied().collect();
    let added: Vec<PaneId> = next_panes
        .iter()
        .copied()
        .filter(|pane_id| !present_set.contains(pane_id))
        .collect();
    let removed: Vec<PaneId> = present
        .iter()
        .copied()
        .filter(|pane_id| !next_set.contains(pane_id))
        .collect();
    let kind = match current {
        _ if !added.is_empty() || !removed.is_empty() => UpdateKind::Restructure,
        Some(current) if current.same_structure(next) => {
            if current.same_sizes(next) {
                UpdateKind::Unchanged
            } else {
                UpdateKind::Resize
            }
        }
        _ => UpdateKind::Restructure,
    };
    PaneUpdatePlan {
        added,
        removed,
        kind,
    }
}

/// Each split's child flexes, in depth-first order. Warp's pane tree lists its branches in the
/// same order, so the two can be compared branch by branch.
pub fn branch_flexes(node: &LayoutNode) -> Vec<Vec<f32>> {
    let mut out = Vec::new();
    collect_branch_flexes(node, &mut out);
    out
}

fn collect_branch_flexes(node: &LayoutNode, out: &mut Vec<Vec<f32>>) {
    if let LayoutNode::Split {
        horizontal,
        children,
        ..
    } = node
    {
        out.push(child_flexes(children, *horizontal));
        for child in children {
            collect_branch_flexes(child, out);
        }
    }
}

/// The `resize-pane` commands that make tmux's layout match `warp_flexes` (the Warp tab's
/// branch flexes in [`branch_flexes`] order, after a divider drag). Each child before a moved
/// divider is sized absolutely through its first pane; tmux applies the size to the cell at
/// that split. A tree that no longer matches the layout sends nothing.
pub fn resize_commands(layout: &LayoutNode, warp_flexes: &[Vec<f32>]) -> Vec<String> {
    let mut commands = Vec::new();
    let mut branch = 0;
    if !collect_resize_commands(layout, warp_flexes, &mut branch, &mut commands)
        || branch != warp_flexes.len()
    {
        return Vec::new();
    }
    commands
}

fn collect_resize_commands(
    node: &LayoutNode,
    warp_flexes: &[Vec<f32>],
    branch: &mut usize,
    commands: &mut Vec<String>,
) -> bool {
    let LayoutNode::Split {
        horizontal,
        children,
        ..
    } = node
    else {
        return true;
    };
    let Some(flexes) = warp_flexes.get(*branch) else {
        return false;
    };
    *branch += 1;
    if flexes.len() != children.len() {
        return false;
    }
    let tmux_flexes = child_flexes(children, *horizontal);
    let moved = flexes
        .iter()
        .zip(&tmux_flexes)
        .any(|(warp, tmux)| (warp - tmux).abs() >= FLEX_EPSILON);
    if moved {
        let sizes: Vec<u32> = children
            .iter()
            .map(|child| size_along(child, *horizontal))
            .collect();
        let total_cells: u32 = sizes.iter().sum();
        let total_flex: f32 = flexes.iter().sum();
        if total_flex > 0.0 {
            for (index, child) in children.iter().enumerate().take(children.len() - 1) {
                let cells = (flexes[index] / total_flex * total_cells as f32).round() as u32;
                let cells = cells.max(1);
                if cells == sizes[index] {
                    continue;
                }
                let Some(first_pane) = child.pane_ids().first().copied() else {
                    continue;
                };
                let axis = if *horizontal { "-x" } else { "-y" };
                commands.push(format!("resize-pane -t {first_pane} {axis} {cells}"));
            }
        }
    }
    children
        .iter()
        .all(|child| collect_resize_commands(child, warp_flexes, branch, commands))
}

fn size_along(node: &LayoutNode, horizontal: bool) -> u32 {
    let (width, height) = match node {
        LayoutNode::Leaf(leaf) => (leaf.width, leaf.height),
        LayoutNode::Split { width, height, .. } => (*width, *height),
    };
    if horizontal { width } else { height }
}

#[cfg(test)]
#[path = "layout_snapshot_tests.rs"]
mod tests;
