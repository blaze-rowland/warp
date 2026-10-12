//! Tab index mapping while a tmux origin tab is hidden (KTD4). The origin tab stays in the
//! workspace's tab list; cmd+1–9, next/previous tab and the tab bar count visible tabs only.

/// The tab index of the `visible`-th visible tab.
pub fn visible_to_raw(hidden: &[bool], visible: usize) -> Option<usize> {
    hidden
        .iter()
        .enumerate()
        .filter(|(_, hidden)| !**hidden)
        .nth(visible)
        .map(|(index, _)| index)
}

/// The visible position of tab `raw`, or `None` when it is hidden.
pub fn raw_to_visible(hidden: &[bool], raw: usize) -> Option<usize> {
    if *hidden.get(raw)? {
        return None;
    }
    Some(hidden[..raw].iter().filter(|hidden| !**hidden).count())
}

/// The next (or previous) visible tab from `current`, wrapping around.
pub fn step_visible(hidden: &[bool], current: usize, forward: bool) -> Option<usize> {
    let len = hidden.len();
    if len == 0 {
        return None;
    }
    (1..=len)
        .map(|step| {
            if forward {
                (current + step) % len
            } else {
                (current + len - step % len) % len
            }
        })
        .find(|index| !hidden[*index])
}

/// The last visible tab.
pub fn last_visible(hidden: &[bool]) -> Option<usize> {
    hidden.iter().rposition(|hidden| !*hidden)
}

/// `index` if it is visible, else the closest visible tab after it, else before it.
pub fn nearest_visible(hidden: &[bool], index: usize) -> Option<usize> {
    let index = index.min(hidden.len().checked_sub(1)?);
    (index..hidden.len())
        .find(|i| !hidden[*i])
        .or_else(|| (0..index).rev().find(|i| !hidden[*i]))
}
