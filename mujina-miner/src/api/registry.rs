//! Dynamic board registration tracking.

use tokio::sync::mpsc;

use crate::api_client::types::BoardState;
use crate::board::{BoardCommand, BoardRegistration};

/// Dynamic collection of board registrations.
///
/// Boards are added via `push()` from a background drain task that
/// receives registrations as boards connect. The registry cleans up
/// disconnected boards lazily when `boards()` is called.
pub struct BoardRegistry {
    boards: Vec<BoardRegistration>,
}

impl BoardRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { boards: Vec::new() }
    }

    /// Add a board registration.
    pub fn push(&mut self, reg: BoardRegistration) {
        self.boards.push(reg);
    }

    fn prune_disconnected(&mut self) {
        self.boards.retain(|reg| reg.state_rx.has_changed().is_ok());
    }

    /// Snapshot all connected boards.
    ///
    /// Removes boards whose sender has been dropped (board disconnected)
    /// and returns the current state of each.
    pub fn boards(&mut self) -> Vec<BoardState> {
        self.prune_disconnected();
        self.boards
            .iter()
            .map(|reg| reg.state_rx.borrow().clone())
            .collect()
    }

    /// Snapshot one connected board by name.
    pub fn board(&mut self, name: &str) -> Option<BoardState> {
        self.prune_disconnected();
        self.boards
            .iter()
            .find(|reg| reg.state_rx.borrow().name == name)
            .map(|reg| reg.state_rx.borrow().clone())
    }

    /// Clone a board command sender by board name if available.
    pub fn command_tx(&mut self, name: &str) -> Option<mpsc::Sender<BoardCommand>> {
        self.prune_disconnected();
        self.boards
            .iter()
            .find(|reg| reg.state_rx.borrow().name == name)
            .and_then(|reg| reg.command_tx.clone())
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::board::BoardRegistration;

    /// Create a board registration with the given name, returning the
    /// state sender so the test can update or drop it.
    fn make_board(name: &str) -> (watch::Sender<BoardState>, BoardRegistration) {
        let state = BoardState {
            name: name.into(),
            model: "Test".into(),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(state);
        (
            tx,
            BoardRegistration {
                state_rx: rx,
                command_tx: None,
            },
        )
    }

    #[test]
    fn tracks_pushed_registrations() {
        let mut registry = BoardRegistry::new();

        let (_keep_a, reg_a) = make_board("board-a");
        let (_keep_b, reg_b) = make_board("board-b");
        registry.push(reg_a);
        registry.push(reg_b);

        let boards = registry.boards();
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].name, "board-a");
        assert_eq!(boards[1].name, "board-b");
    }

    #[test]
    fn removes_disconnected_boards() {
        let mut registry = BoardRegistry::new();

        let (keep, reg_a) = make_board("stays");
        let (drop_me, reg_b) = make_board("goes-away");
        registry.push(reg_a);
        registry.push(reg_b);

        assert_eq!(registry.boards().len(), 2);

        drop(drop_me);
        let boards = registry.boards();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].name, "stays");

        drop(keep);
    }

    #[test]
    fn reflects_updated_state() {
        let mut registry = BoardRegistry::new();

        let (tx, reg) = make_board("board-a");
        registry.push(reg);

        assert_eq!(registry.boards()[0].model, "Test");

        tx.send_modify(|s| s.model = "Updated".into());
        assert_eq!(registry.boards()[0].model, "Updated");
    }

    #[test]
    fn returns_command_sender_for_named_board() {
        let mut registry = BoardRegistry::new();
        let (_state_tx, state_rx) = watch::channel(BoardState {
            name: "board-a".into(),
            model: "Test".into(),
            ..Default::default()
        });
        let (command_tx, _command_rx) = mpsc::channel(1);
        registry.push(BoardRegistration {
            state_rx,
            command_tx: Some(command_tx.clone()),
        });

        let cloned = registry.command_tx("board-a");
        assert!(cloned.is_some());
    }
}
