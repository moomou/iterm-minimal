use crate::session::SessionId;

/// Axis along which a pane split divides space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    /// Side-by-side panes (iTerm2 Cmd+D: "Split Vertically with Current Profile").
    Vertical,
    /// Stacked top/bottom panes (iTerm2 Cmd+Shift+D: "Split Horizontally with Current Profile").
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PaneRect {
    #[inline]
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }

    #[inline]
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.width * 0.5, self.y + self.height * 0.5)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DividerRect {
    pub axis: SplitAxis,
    pub rect: PaneRect,
    /// Path of child choices (0 = first, 1 = second) identifying the split node in the tree.
    pub split_id: u64,
}

#[derive(Debug, Clone)]
pub enum PaneNode {
    Leaf(SessionId),
    Split {
        split_id: u64,
        axis: SplitAxis,
        /// Fraction of space given to `first`, clamped to `[0.1, 0.9]`.
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

/// Binary split-pane layout tree for a single terminal tab.
#[derive(Debug, Clone)]
pub struct PaneTree {
    pub root: PaneNode,
    pub active: SessionId,
    pub zoomed: Option<SessionId>,
    next_split_id: u64,
}

impl PaneTree {
    pub fn new(initial: SessionId) -> Self {
        Self {
            root: PaneNode::Leaf(initial),
            active: initial,
            zoomed: None,
            next_split_id: 1,
        }
    }

    pub fn pane_count(&self) -> usize {
        fn count(n: &PaneNode) -> usize {
            match n {
                PaneNode::Leaf(_) => 1,
                PaneNode::Split { first, second, .. } => count(first) + count(second),
            }
        }
        count(&self.root)
    }

    pub fn sessions(&self) -> Vec<SessionId> {
        let mut out = Vec::new();
        fn collect(n: &PaneNode, out: &mut Vec<SessionId>) {
            match n {
                PaneNode::Leaf(id) => out.push(*id),
                PaneNode::Split { first, second, .. } => {
                    collect(first, out);
                    collect(second, out);
                }
            }
        }
        collect(&self.root, &mut out);
        out
    }

    /// Split `target` pane along `axis`, placing `new_session` in the second half and focusing it.
    pub fn split(&mut self, target: SessionId, axis: SplitAxis, new_session: SessionId) -> bool {
        let split_id = self.next_split_id;
        self.next_split_id += 1;
        fn replace(node: &mut PaneNode, target: SessionId, axis: SplitAxis, new_session: SessionId, split_id: u64) -> bool {
            match node {
                PaneNode::Leaf(id) if *id == target => {
                    *node = PaneNode::Split {
                        split_id,
                        axis,
                        ratio: 0.5,
                        first: Box::new(PaneNode::Leaf(target)),
                        second: Box::new(PaneNode::Leaf(new_session)),
                    };
                    true
                }
                PaneNode::Leaf(_) => false,
                PaneNode::Split { first, second, .. } => {
                    replace(first, target, axis, new_session, split_id)
                        || replace(second, target, axis, new_session, split_id)
                }
            }
        }
        if replace(&mut self.root, target, axis, new_session, split_id) {
            self.active = new_session;
            self.zoomed = None;
            true
        } else {
            false
        }
    }

    /// Close `target` pane. Returns `None` if it was the last pane in the tab, or `Some(new_active)` otherwise.
    pub fn close(&mut self, target: SessionId) -> Option<SessionId> {
        if let PaneNode::Leaf(id) = self.root {
            if id == target {
                return None;
            }
        }
        fn remove(node: &mut PaneNode, target: SessionId) -> Option<PaneNode> {
            match node {
                PaneNode::Leaf(_) => None,
                PaneNode::Split { first, second, .. } => {
                    if matches!(**first, PaneNode::Leaf(id) if id == target) {
                        return Some((**second).clone());
                    }
                    if matches!(**second, PaneNode::Leaf(id) if id == target) {
                        return Some((**first).clone());
                    }
                    if let Some(new_first) = remove(first, target) {
                        **first = new_first;
                        return None;
                    }
                    if let Some(new_second) = remove(second, target) {
                        **second = new_second;
                        return None;
                    }
                    None
                }
            }
        }
        if let Some(new_root) = remove(&mut self.root, target) {
            self.root = new_root;
        }
        if self.zoomed == Some(target) {
            self.zoomed = None;
        }
        let all = self.sessions();
        if !all.contains(&self.active) {
            self.active = *all.first()?;
        }
        Some(self.active)
    }

    pub fn toggle_zoom(&mut self) {
        if self.pane_count() <= 1 {
            self.zoomed = None;
            return;
        }
        if self.zoomed == Some(self.active) {
            self.zoomed = None;
        } else {
            self.zoomed = Some(self.active);
        }
    }

    /// Compute pixel rectangles for all visible panes and split dividers inside `bounds`.
    pub fn layout(&self, bounds: PaneRect, divider_px: f32) -> (Vec<(SessionId, PaneRect)>, Vec<DividerRect>) {
        if let Some(z) = self.zoomed {
            return (vec![(z, bounds)], Vec::new());
        }
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        fn walk(
            node: &PaneNode,
            rect: PaneRect,
            div_px: f32,
            panes: &mut Vec<(SessionId, PaneRect)>,
            dividers: &mut Vec<DividerRect>,
        ) {
            match node {
                PaneNode::Leaf(id) => panes.push((*id, rect)),
                PaneNode::Split {
                    split_id,
                    axis,
                    ratio,
                    first,
                    second,
                } => match axis {
                    SplitAxis::Vertical => {
                        let avail = (rect.width - div_px).max(2.0);
                        let w1 = (avail * *ratio).round().max(1.0);
                        let w2 = (avail - w1).max(1.0);
                        let r1 = PaneRect {
                            x: rect.x,
                            y: rect.y,
                            width: w1,
                            height: rect.height,
                        };
                        let div = PaneRect {
                            x: rect.x + w1,
                            y: rect.y,
                            width: div_px,
                            height: rect.height,
                        };
                        let r2 = PaneRect {
                            x: rect.x + w1 + div_px,
                            y: rect.y,
                            width: w2,
                            height: rect.height,
                        };
                        dividers.push(DividerRect {
                            axis: *axis,
                            rect: div,
                            split_id: *split_id,
                        });
                        walk(first, r1, div_px, panes, dividers);
                        walk(second, r2, div_px, panes, dividers);
                    }
                    SplitAxis::Horizontal => {
                        let avail = (rect.height - div_px).max(2.0);
                        let h1 = (avail * *ratio).round().max(1.0);
                        let h2 = (avail - h1).max(1.0);
                        let r1 = PaneRect {
                            x: rect.x,
                            y: rect.y,
                            width: rect.width,
                            height: h1,
                        };
                        let div = PaneRect {
                            x: rect.x,
                            y: rect.y + h1,
                            width: rect.width,
                            height: div_px,
                        };
                        let r2 = PaneRect {
                            x: rect.x,
                            y: rect.y + h1 + div_px,
                            width: rect.width,
                            height: h2,
                        };
                        dividers.push(DividerRect {
                            axis: *axis,
                            rect: div,
                            split_id: *split_id,
                        });
                        walk(first, r1, div_px, panes, dividers);
                        walk(second, r2, div_px, panes, dividers);
                    }
                },
            }
        }
        walk(&self.root, bounds, divider_px, &mut panes, &mut dividers);
        (panes, dividers)
    }

    /// Resize a split divider by setting its ratio directly (clamped to `[0.1, 0.9]`).
    pub fn set_split_ratio(&mut self, target_split_id: u64, new_ratio: f32) -> bool {
        fn update(node: &mut PaneNode, target_id: u64, ratio: f32) -> bool {
            match node {
                PaneNode::Leaf(_) => false,
                PaneNode::Split {
                    split_id,
                    ratio: r,
                    first,
                    second,
                    ..
                } => {
                    if *split_id == target_id {
                        *r = ratio.clamp(0.1, 0.9);
                        return true;
                    }
                    update(first, target_id, ratio) || update(second, target_id, ratio)
                }
            }
        }
        update(&mut self.root, target_split_id, new_ratio)
    }

    /// Move focus geometrically in `dir` (Cmd+Opt+Arrow in iTerm2).
    pub fn focus_direction(&mut self, dir: FocusDirection, bounds: PaneRect) -> SessionId {
        let (panes, _) = self.layout(bounds, 1.0);
        let Some((_, active_rect)) = panes.iter().find(|(id, _)| *id == self.active) else {
            return self.active;
        };
        let (ax, ay) = active_rect.center();
        let mut best: Option<(SessionId, f32)> = None;

        for &(id, rect) in &panes {
            if id == self.active {
                continue;
            }
            let (cx, cy) = rect.center();
            let valid = match dir {
                FocusDirection::Left => cx < ax - 1.0,
                FocusDirection::Right => cx > ax + 1.0,
                FocusDirection::Up => cy < ay - 1.0,
                FocusDirection::Down => cy > ay + 1.0,
            };
            if !valid {
                continue;
            }
            let dx = cx - ax;
            let dy = cy - ay;
            let dist = dx * dx + dy * dy;
            if best.is_none_or(|(_, bd)| dist < bd) {
                best = Some((id, dist));
            }
        }
        if let Some((next_id, _)) = best {
            self.active = next_id;
        }
        self.active
    }

    /// Cycle focus to next/previous pane in tree order (`Cmd+]` / `Cmd+[`).
    pub fn cycle_pane(&mut self, forward: bool) -> SessionId {
        let list = self.sessions();
        if list.len() <= 1 {
            return self.active;
        }
        let pos = list.iter().position(|&id| id == self.active).unwrap_or(0);
        let next = if forward {
            (pos + 1) % list.len()
        } else {
            (pos + list.len() - 1) % list.len()
        };
        self.active = list[next];
        self.active
    }

    /// Hit-test which pane contains pixel coordinate `(px, py)`.
    pub fn pane_at(&self, px: f32, py: f32, bounds: PaneRect, divider_px: f32) -> Option<SessionId> {
        let (panes, _) = self.layout(bounds, divider_px);
        panes.into_iter().find(|(_, r)| r.contains(px, py)).map(|(id, _)| id)
    }
}
