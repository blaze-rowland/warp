//! Parse tmux `window-layout` strings into pane geometry.
//!
//! Format: a 4-hex-digit checksum of the body, a comma, then a cell:
//! `WxH,X,Y,paneid` (a pane), `WxH,X,Y{cell,cell}` (side by side) or `WxH,X,Y[cell,cell]`
//! (stacked). `%layout-change` carries both the full layout and the visible layout, which
//! differ while a pane is zoomed.

use super::parser::PaneId;

/// Deepest split nesting accepted. tmux cannot usefully nest this deep in a real window; the cap
/// keeps a hostile or corrupt layout from exhausting the stack.
pub const MAX_LAYOUT_DEPTH: usize = 32;

/// `#{window_flags}` as reported in `%layout-change`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowFlags(String);

impl WindowFlags {
    pub fn new(raw: &str) -> Self {
        Self(raw.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `*`: the session's current window.
    pub fn is_active(&self) -> bool {
        self.0.contains('*')
    }

    /// `Z`: a pane is zoomed; the visible layout shows only that pane.
    pub fn is_zoomed(&self) -> bool {
        self.0.contains('Z')
    }

    /// `-`: the session's previously current window.
    pub fn is_last(&self) -> bool {
        self.0.contains('-')
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("layout has no 4-hex-digit checksum prefix")]
    MissingChecksum,
    #[error("layout checksum {expected:04x} does not match body checksum {actual:04x}")]
    BadChecksum { expected: u16, actual: u16 },
    #[error("layout nests deeper than {MAX_LAYOUT_DEPTH} levels")]
    TooDeep,
    #[error("malformed layout")]
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutLeaf {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub pane_id: PaneId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    Leaf(LayoutLeaf),
    Split {
        /// True for `{…}` (children side by side), false for `[…]` (stacked).
        horizontal: bool,
        width: u32,
        height: u32,
        x: u32,
        y: u32,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn leaves(&self) -> Vec<&LayoutLeaf> {
        match self {
            Self::Leaf(leaf) => vec![leaf],
            Self::Split { children, .. } => children.iter().flat_map(Self::leaves).collect(),
        }
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.leaves().into_iter().map(|leaf| leaf.pane_id).collect()
    }

    fn size_along(&self, horizontal: bool) -> u32 {
        let (width, height) = match self {
            Self::Leaf(leaf) => (leaf.width, leaf.height),
            Self::Split { width, height, .. } => (*width, *height),
        };
        if horizontal { width } else { height }
    }
}

/// A Warp split that inserts a pane beside the previous sibling subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitStep {
    pub parent: Vec<PaneId>,
    pub new_pane: PaneId,
    pub side_by_side: bool,
    pub parent_size: u32,
    pub new_size: u32,
}

/// Parsed `%layout-change` layout fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLayout {
    pub layout: LayoutNode,
    /// What is on screen: only the zoomed pane while zoomed, otherwise the full layout.
    pub visible_layout: LayoutNode,
    pub flags: WindowFlags,
}

/// tmux's `layout_checksum`: a 16-bit rotate-and-add over the layout body.
pub fn layout_checksum(body: &str) -> u16 {
    body.bytes().fold(0u16, |checksum, byte| {
        checksum.rotate_right(1).wrapping_add(u16::from(byte))
    })
}

/// Parses a checksummed `#{window_layout}` string.
pub fn parse_window_layout(input: &str) -> Result<LayoutNode, LayoutError> {
    let input = input.trim();
    let (checksum, body) = input
        .split_once(',')
        .filter(|(checksum, _)| {
            checksum.len() == 4 && checksum.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .and_then(|(checksum, body)| Some((u16::from_str_radix(checksum, 16).ok()?, body)))
        .ok_or(LayoutError::MissingChecksum)?;
    let actual = layout_checksum(body);
    if actual != checksum {
        return Err(LayoutError::BadChecksum {
            expected: checksum,
            actual,
        });
    }
    let (node, rest) = parse_cell(body, 0)?;
    if rest.is_empty() {
        Ok(node)
    } else {
        Err(LayoutError::Malformed)
    }
}

/// Parses the layout fields of a `%layout-change`. Without a visible layout (older servers), the
/// full layout is what is visible.
pub fn parse_layout_change(
    layout: &str,
    visible_layout: Option<&str>,
    flags: &WindowFlags,
) -> Result<WindowLayout, LayoutError> {
    let layout = parse_window_layout(layout)?;
    let visible_layout = match visible_layout {
        Some(visible) => parse_window_layout(visible)?,
        None => layout.clone(),
    };
    Ok(WindowLayout {
        layout,
        visible_layout,
        flags: flags.clone(),
    })
}

pub fn split_steps(layout: &LayoutNode) -> Vec<SplitStep> {
    let mut steps = Vec::new();
    collect_split_steps(layout, &mut steps);
    steps
}

pub fn missing_from_layout(bound: &[PaneId], layout: &LayoutNode) -> Vec<PaneId> {
    let live = layout.pane_ids();
    bound
        .iter()
        .filter(|id| !live.contains(id))
        .copied()
        .collect()
}

fn collect_split_steps(node: &LayoutNode, steps: &mut Vec<SplitStep>) {
    let LayoutNode::Split {
        horizontal,
        children,
        ..
    } = node
    else {
        return;
    };
    let Some((first, rest)) = children.split_first() else {
        return;
    };
    collect_split_steps(first, steps);
    let mut previous_leaves = first.pane_ids();
    let mut previous_size = first.size_along(*horizontal);
    for child in rest {
        let child_ids = child.pane_ids();
        if let Some(&new_pane) = child_ids.first() {
            let new_size = child.size_along(*horizontal);
            steps.push(SplitStep {
                parent: previous_leaves,
                new_pane,
                side_by_side: *horizontal,
                parent_size: previous_size,
                new_size,
            });
            previous_leaves = child_ids;
            previous_size = new_size;
        }
        collect_split_steps(child, steps);
    }
}

fn parse_cell(input: &str, depth: usize) -> Result<(LayoutNode, &str), LayoutError> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err(LayoutError::TooDeep);
    }
    let (width, rest) = parse_u32(input)?;
    let (height, rest) = parse_u32(expect(rest, 'x')?)?;
    let (x, rest) = parse_u32(expect(rest, ',')?)?;
    let (y, rest) = parse_u32(expect(rest, ',')?)?;
    for (open, close, horizontal) in [('{', '}', true), ('[', ']', false)] {
        if let Some(rest) = rest.strip_prefix(open) {
            let (children, rest) = parse_children(rest, depth + 1)?;
            let node = LayoutNode::Split {
                horizontal,
                width,
                height,
                x,
                y,
                children,
            };
            return Ok((node, expect(rest, close)?));
        }
    }
    let (pane, rest) = parse_u32(expect(rest, ',')?)?;
    let leaf = LayoutLeaf {
        width,
        height,
        x,
        y,
        pane_id: PaneId(pane),
    };
    Ok((LayoutNode::Leaf(leaf), rest))
}

fn parse_children(mut input: &str, depth: usize) -> Result<(Vec<LayoutNode>, &str), LayoutError> {
    let mut children = Vec::new();
    loop {
        let (child, rest) = parse_cell(input, depth)?;
        children.push(child);
        match rest.strip_prefix(',') {
            Some(rest) => input = rest,
            None => return Ok((children, rest)),
        }
    }
}

fn expect(input: &str, prefix: char) -> Result<&str, LayoutError> {
    input.strip_prefix(prefix).ok_or(LayoutError::Malformed)
}

fn parse_u32(input: &str) -> Result<(u32, &str), LayoutError> {
    let end = input
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(input.len());
    let value = input[..end].parse().map_err(|_| LayoutError::Malformed)?;
    Ok((value, &input[end..]))
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
