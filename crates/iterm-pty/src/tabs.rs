use crate::pane_tree::{FocusDirection, PaneRect, PaneTree, SplitAxis};
use crate::session::SessionId;
use iterm_profile::Rgba;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

/// One tab inside a terminal window.
#[derive(Debug, Clone)]
pub struct TabState {
    pub id: TabId,
    pub title: String,
    /// Dynamic per-tab tint set by `OSC 6;1;bg;...` (used by user's `color-ssh` & `color-ranger`).
    pub tab_color: Option<Rgba>,
    pub panes: PaneTree,
}

impl TabState {
    pub fn new(id: TabId, initial_session: SessionId, title: String) -> Self {
        Self {
            id,
            title,
            tab_color: None,
            panes: PaneTree::new(initial_session),
        }
    }
}

/// Manages all tabs in a single OS window.
#[derive(Debug, Clone)]
pub struct TabManager {
    pub tabs: Vec<TabState>,
    pub active_index: usize,
    next_tab_id: u64,
}

impl TabManager {
    pub fn new(first_session: SessionId, title: String) -> Self {
        let tab = TabState::new(TabId(1), first_session, title);
        Self {
            tabs: vec![tab],
            active_index: 0,
            next_tab_id: 2,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    #[inline]
    pub fn try_active_tab(&self) -> Option<&TabState> {
        self.tabs.get(self.active_index).or_else(|| self.tabs.first())
    }

    #[inline]
    pub fn try_active_tab_mut(&mut self) -> Option<&mut TabState> {
        if self.tabs.is_empty() {
            return None;
        }
        if self.active_index >= self.tabs.len() {
            self.active_index = self.tabs.len() - 1;
        }
        self.tabs.get_mut(self.active_index)
    }

    #[inline]
    pub fn try_active_session(&self) -> Option<SessionId> {
        self.try_active_tab().map(|t| t.panes.active)
    }

    #[inline]
    pub fn active_tab(&self) -> &TabState {
        &self.tabs[self.active_index]
    }

    #[inline]
    pub fn active_tab_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_index]
    }

    #[inline]
    pub fn active_session(&self) -> SessionId {
        self.active_tab().panes.active
    }

    pub fn add_tab(&mut self, session: SessionId, title: String) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        self.tabs.push(TabState::new(id, session, title));
        self.active_index = self.tabs.len() - 1;
        id
    }

    pub fn select_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_index = idx;
        }
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_index = (self.active_index + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active_index = (self.active_index + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    pub fn move_active_tab(&mut self, delta: isize) {
        let len = self.tabs.len();
        if len <= 1 {
            return;
        }
        let target = (self.active_index as isize + delta).rem_euclid(len as isize) as usize;
        self.tabs.swap(self.active_index, target);
        self.active_index = target;
    }

    pub fn split_active_pane(&mut self, axis: SplitAxis, new_session: SessionId) {
        if let Some(active) = self.try_active_session() {
            if let Some(tab) = self.try_active_tab_mut() {
                tab.panes.split(active, axis, new_session);
            }
        }
    }

    /// Close a session anywhere in this window. Returns `true` if the window now has 0 tabs left.
    pub fn close_session(&mut self, session: SessionId) -> bool {
        for idx in 0..self.tabs.len() {
            if self.tabs[idx].panes.sessions().contains(&session) {
                if self.tabs[idx].panes.close(session).is_none() {
                    self.tabs.remove(idx);
                    if self.tabs.is_empty() {
                        return true;
                    }
                    if self.active_index >= self.tabs.len() {
                        self.active_index = self.tabs.len() - 1;
                    }
                }
                return false;
            }
        }
        self.tabs.is_empty()
    }

    pub fn focus_direction(&mut self, dir: FocusDirection, bounds: PaneRect) -> Option<SessionId> {
        self.try_active_tab_mut()
            .map(|t| t.panes.focus_direction(dir, bounds))
    }

    pub fn update_session_tab_color(&mut self, session: SessionId, color: Option<Rgba>) {
        for tab in &mut self.tabs {
            if tab.panes.sessions().contains(&session) {
                tab.tab_color = color;
            }
        }
    }

    pub fn update_session_title(&mut self, session: SessionId, title: String) {
        for tab in &mut self.tabs {
            if tab.panes.active == session {
                tab.title = title;
                return;
            }
        }
    }
}
