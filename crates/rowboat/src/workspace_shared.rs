//! One application-state writer shared by all retained connection entities.
use rowboat_services::AppState;
use std::{cell::RefCell, rc::Rc};
pub type Shared = Rc<RefCell<SharedState>>;
#[derive(Default)]
pub struct StorageReadiness {
    pub state: Option<bool>,
    pub locked_recovery: Option<bool>,
}
impl StorageReadiness {
    pub fn complete(&self) -> bool {
        self.state.is_some() && self.locked_recovery.is_some()
    }
    pub fn writable(&self) -> bool {
        self.state == Some(true) && self.locked_recovery == Some(true)
    }
}
pub struct SharedState {
    pub state: AppState,
    pub writable: bool,
    pub revision: u64,
    pub saved_revision: u64,
    pub next_tab: usize,
    pub active_workspace: u64,
    pub shell_modal_open: bool,
    pub save: tokio::sync::mpsc::UnboundedSender<crate::workspace_storage::Write<AppState>>,
}
impl SharedState {
    pub fn edit(&mut self, f: impl FnOnce(&mut AppState)) {
        if !self.writable {
            return;
        }
        f(&mut self.state);
        self.revision += 1;
        let _ = self.save.send(crate::workspace_storage::Write::Save(
            self.revision,
            self.state.clone(),
        ));
    }
    pub fn tab_id(&mut self) -> usize {
        let id = self.next_tab;
        self.next_tab += 1;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::{Shared, SharedState};
    use std::{cell::RefCell, rc::Rc};
    #[test]
    fn parked_workspace_updates_merge_into_one_writer_and_tab_ids_remain_global() {
        let (save, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let shared: Shared = Rc::new(RefCell::new(SharedState {
            state: Default::default(),
            writable: true,
            revision: 0,
            saved_revision: 0,
            next_tab: 30,
            active_workspace: 1,
            shell_modal_open: false,
            save,
        }));
        let a = shared.clone();
        let b = shared.clone();
        let stale_read = a.borrow().state.clone();
        a.borrow_mut().edit(|s| {
            s.connections.push(rowboat_core::ConnectionProfile {
                name: "postgres".into(),
                ..Default::default()
            })
        });
        b.borrow_mut().edit(|s| {
            s.snippets.push(rowboat_services::Snippet {
                id: "s".into(),
                name: "draft".into(),
                sql: "SELECT 1".into(),
            })
        });
        assert!(stale_read.connections.is_empty());
        let _ = receiver.try_recv().unwrap();
        let crate::workspace_storage::Write::Save(revision, combined) =
            receiver.try_recv().unwrap()
        else {
            panic!("Expected state save");
        };
        assert_eq!(revision, 2);
        assert_eq!(combined.connections.len(), 1);
        assert_eq!(combined.snippets.len(), 1);
        assert_eq!(a.borrow_mut().tab_id(), 30);
        assert_eq!(b.borrow_mut().tab_id(), 31);
    }
}
