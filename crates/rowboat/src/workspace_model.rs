//! UI-independent ownership, routing and split-pane invariants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub workspace: u64,
    pub generation: u64,
}

pub fn workspace_commands_allowed(initialized: bool, closing: bool) -> bool {
    initialized && !closing
}
/// A sequence ACK alone cannot authorize closing buffers that changed after capture.
pub fn checkpoint_acknowledged<T: PartialEq>(
    saved: u64,
    requested: u64,
    captured: Option<&T>,
    current: &T,
) -> bool {
    saved >= requested && captured == Some(current)
}

/// One retained session per profile across all application windows. IDs are never reused.
#[derive(Default)]
pub struct WindowOwnership(std::collections::BTreeMap<Option<String>, u64>);
impl WindowOwnership {
    pub fn reassign(
        &mut self,
        workspace: u64,
        previous: &Option<String>,
        next: Option<String>,
    ) -> Result<(), String> {
        if self.find(previous) != Some(workspace) {
            return Err("Workspace ownership changed before profile assignment".into());
        }
        if self.find(&next).is_some_and(|id| id != workspace) {
            return Err(
                "This profile already belongs to another workspace; open that workspace instead"
                    .into(),
            );
        }
        self.0.remove(previous);
        self.0.insert(next, workspace);
        Ok(())
    }
    pub fn find(&self, owner: &Option<String>) -> Option<u64> {
        self.0.get(owner).copied()
    }
    pub fn claim(&mut self, owner: Option<String>, proposed: u64) -> u64 {
        *self.0.entry(owner).or_insert(proposed)
    }
    pub fn release(&mut self, workspace: u64) {
        self.0.retain(|_, id| *id != workspace);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsyncOwner {
    pub route: Route,
    pub connection: Option<String>,
}
impl AsyncOwner {
    pub fn accepts(&self, route: Route, connection: Option<&str>, closing: bool) -> bool {
        !closing && self.route == route && self.connection.as_deref() == connection
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitPanes {
    pub tabs: [Option<usize>; 2],
    pub focused: usize,
}
impl SplitPanes {
    pub fn select(&mut self, tab: usize) {
        if let Some(pane) = self.tabs.iter().position(|id| *id == Some(tab)) {
            self.focused = pane;
        } else {
            self.tabs[self.focused] = Some(tab);
        }
    }
    pub fn focus(&mut self, pane: usize) -> Option<usize> {
        let tab = *self.tabs.get(pane)?;
        if tab.is_some() {
            self.focused = pane;
        }
        tab
    }
    pub fn split(&mut self, other: usize) {
        if self.tabs[0] != Some(other) {
            self.tabs[1] = Some(other);
        }
    }
    pub fn collapse(&mut self) {
        self.tabs = [self.tabs[self.focused], None];
        self.focused = 0;
    }
    pub fn reconcile(&mut self, ids: &[usize], fallback: usize) {
        for tab in &mut self.tabs {
            if tab.is_some_and(|id| !ids.contains(&id)) {
                *tab = None;
            }
        }
        if self.tabs[0].is_none() {
            self.tabs[0] = self.tabs[1].take().or(Some(fallback));
            self.focused = 0;
        }
        if self.tabs[self.focused].is_none() {
            self.focused = 0;
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseState {
    pub busy: bool,
    pub transaction: bool,
    pub unknown_transaction: bool,
    pub staged: usize,
    pub recovered: usize,
    pub drafts: bool,
    pub error: bool,
}
impl CloseState {
    pub fn is_clean(self) -> bool {
        self == Self::default()
    }

    pub fn aggregate(states: impl IntoIterator<Item = Self>) -> Self {
        states.into_iter().fold(Self::default(), |a, b| Self {
            busy: a.busy || b.busy,
            transaction: a.transaction || b.transaction,
            unknown_transaction: a.unknown_transaction || b.unknown_transaction,
            staged: a.staged + b.staged,
            recovered: a.recovered + b.recovered,
            drafts: a.drafts || b.drafts,
            error: a.error || b.error,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn assignment_rekeys_atomically_and_rejects_conflicts_without_losing_draft() {
        let mut registry = super::WindowOwnership::default();
        let pg = Some("pg".into());
        let mysql = Some("mysql".into());
        assert_eq!(registry.claim(None, 1), 1);
        registry.reassign(1, &None, pg.clone()).unwrap();
        assert_eq!(registry.find(&None), None);
        assert_eq!(registry.claim(pg.clone(), 2), 1); // OpenWindow focuses the original host.
        assert_eq!(registry.claim(None, 3), 3); // +Workspace creates a new draft.
        assert!(registry.reassign(3, &None, pg.clone()).is_err());
        assert_eq!(registry.find(&None), Some(3));
        assert_eq!(registry.find(&pg), Some(1));
        assert!(registry.reassign(99, &None, mysql.clone()).is_err());
        registry.reassign(3, &None, mysql.clone()).unwrap();
        registry.release(1);
        assert_eq!(registry.find(&mysql), Some(3));
    }
    #[test]
    fn closing_rejects_old_ack_and_buffers_changed_after_final_capture() {
        use super::checkpoint_acknowledged;
        let captured = vec!["main SQL", "satellite SQL"];
        let edited = vec!["main SQL", "satellite SQL plus late input"];
        assert!(!checkpoint_acknowledged(7, 8, Some(&captured), &captured));
        assert!(!checkpoint_acknowledged(8, 8, Some(&captured), &edited));
        assert!(!checkpoint_acknowledged(8, 9, Some(&edited), &edited));
        assert!(checkpoint_acknowledged(9, 9, Some(&edited), &edited));
    }
    #[test]
    fn window_ownership_prevents_duplicate_sessions_and_late_close_cannot_release_reopened_owner() {
        let mut registry = super::WindowOwnership::default();
        let pg = Some("pg".into());
        let mysql = Some("mysql".into());
        assert_eq!(registry.claim(pg.clone(), 1), 1); // Main window.
        assert_eq!(registry.claim(pg.clone(), 2), 1); // Satellite request must activate main.
        assert_eq!(registry.claim(mysql.clone(), 3), 3); // Independent satellite session.
        registry.release(1);
        assert_eq!(registry.find(&mysql), Some(3));
        assert_eq!(registry.claim(pg.clone(), 4), 4); // Explicit reopen, fresh ownership.
        registry.release(1); // Late callback from closed main-hosted workspace.
        assert_eq!(registry.find(&pg), Some(4));
        let job = super::AsyncOwner {
            route: super::Route {
                workspace: 1,
                generation: 2,
            },
            connection: pg,
        };
        assert!(!job.accepts(
            super::Route {
                workspace: 4,
                generation: 2
            },
            Some("pg"),
            false
        ));
        assert!(!job.accepts(
            super::Route {
                workspace: 3,
                generation: 2
            },
            Some("mysql"),
            false
        ));
    }
    use super::{CloseState, Route, SplitPanes};
    #[test]
    fn sql_file_completion_stays_with_its_owner_and_is_rejected_after_reconnect_or_close() {
        let route = Route {
            workspace: 4,
            generation: 2,
        };
        let owner = super::AsyncOwner {
            route,
            connection: Some("pg".into()),
        };
        // Parking changes only shell focus, so a late file may still open in its owning workspace.
        assert!(owner.accepts(route, Some("pg"), false));
        assert!(!owner.accepts(
            Route {
                generation: 3,
                ..route
            },
            Some("pg"),
            false
        ));
        assert!(!owner.accepts(
            Route {
                workspace: 5,
                ..route
            },
            Some("pg"),
            false
        ));
        assert!(!owner.accepts(route, Some("mysql"), false));
        assert!(!owner.accepts(route, Some("pg"), true));
        assert!(!owner.accepts(route, None, false));
    }
    #[test]
    fn events_cannot_cross_workspace_or_reconnected_generation() {
        let owner = Route {
            workspace: 3,
            generation: 8,
        };
        assert_ne!(
            owner,
            Route {
                workspace: 4,
                generation: 8
            }
        );
        assert_ne!(
            owner,
            Route {
                workspace: 3,
                generation: 9
            }
        );
    }
    #[test]
    fn split_selection_focuses_existing_pane_without_duplicate_tab() {
        let mut p = SplitPanes {
            tabs: [Some(10), Some(20)],
            focused: 0,
        };
        p.select(20);
        assert_eq!(p.focused, 1);
        assert_eq!(p.tabs, [Some(10), Some(20)]);
        p.select(30);
        assert_eq!(p.tabs, [Some(10), Some(30)]);
        assert_eq!(p.focus(0), Some(10));
        p.collapse();
        assert_eq!(p.tabs, [Some(10), None]);
    }
    #[test]
    fn closing_a_split_tab_keeps_a_valid_surviving_focus() {
        let mut p = SplitPanes {
            tabs: [Some(10), Some(20)],
            focused: 1,
        };
        p.reconcile(&[10, 30], 30);
        assert_eq!(p.focused, 0);
        assert_eq!(p.tabs, [Some(10), None]);
        p.reconcile(&[30], 30);
        assert_eq!(p.tabs, [Some(30), None]);
    }
    #[test]
    fn parked_workspaces_participate_in_close_guards() {
        let active = CloseState::default();
        let parked = CloseState {
            busy: true,
            transaction: true,
            staged: 5,
            drafts: true,
            ..Default::default()
        };
        let all = CloseState::aggregate([active, parked]);
        assert!(all.busy && all.transaction && all.drafts);
        assert_eq!(all.staged, 5);
    }
}
