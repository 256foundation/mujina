//! Dynamic board registration tracking.

use tokio::sync::{mpsc, watch};

use crate::api::commands::BoardCommand;
use crate::api_client::types::BoardTelemetry;

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

    /// Remove boards whose sender has been dropped (board disconnected).
    fn prune_disconnected(&mut self) {
        self.boards
            .retain(|reg| reg.telemetry_rx.has_changed().is_ok());
    }

    /// Snapshot all connected boards.
    ///
    /// Removes boards whose sender has been dropped (board disconnected)
    /// and returns the current state of each.
    pub fn boards(&mut self) -> Vec<BoardTelemetry> {
        self.prune_disconnected();
        self.boards
            .iter()
            .map(|reg| reg.telemetry_rx.borrow().clone())
            .collect()
    }

    /// Snapshot a single connected board by name.
    pub fn board(&mut self, name: &str) -> Option<BoardTelemetry> {
        self.prune_disconnected();
        self.boards
            .iter()
            .find(|reg| reg.telemetry_rx.borrow().name == name)
            .map(|reg| reg.telemetry_rx.borrow().clone())
    }

    /// Look up the command sender for a board by name. `None` if the
    /// board is unknown or accepts no commands.
    pub fn command_tx(&mut self, name: &str) -> Option<mpsc::Sender<BoardCommand>> {
        self.prune_disconnected();
        self.boards
            .iter()
            .find(|reg| reg.telemetry_rx.borrow().name == name)
            .and_then(|reg| reg.command_tx.clone())
    }
}

/// A board's registration with the API server.
pub struct BoardRegistration {
    pub telemetry_rx: watch::Receiver<BoardTelemetry>,
    /// Sender for board commands. `None` if the board accepts no commands.
    pub command_tx: Option<mpsc::Sender<BoardCommand>>,
}

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    use super::*;

    /// Create a board registration with the given name, returning the
    /// state sender so the test can update or drop it.
    fn make_board(name: &str) -> (watch::Sender<BoardTelemetry>, BoardRegistration) {
        let telemetry = BoardTelemetry {
            name: name.into(),
            model: "Test".into(),
            ..Default::default()
        };
        let (tx, rx) = watch::channel(telemetry);
        (
            tx,
            BoardRegistration {
                telemetry_rx: rx,
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

        // Both present initially
        assert_eq!(registry.boards().len(), 2);

        // Drop the sender for board B -- simulates board disconnect
        drop(drop_me);
        let boards = registry.boards();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].name, "stays");

        // Sender A still alive
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
    fn returns_single_board_by_name() {
        let mut registry = BoardRegistry::new();

        let (_keep, reg) = make_board("board-a");
        registry.push(reg);

        assert_eq!(registry.board("board-a").unwrap().name, "board-a");
        assert!(registry.board("missing").is_none());
    }

    #[test]
    fn returns_command_sender_for_named_board() {
        use tokio::sync::mpsc;

        let mut registry = BoardRegistry::new();

        let (_keep, mut reg) = make_board("board-a");
        let (cmd_tx, _cmd_rx) = mpsc::channel::<BoardCommand>(1);
        reg.command_tx = Some(cmd_tx);
        registry.push(reg);

        assert!(registry.command_tx("board-a").is_some());
        assert!(registry.command_tx("missing").is_none());

        let (_keep_b, reg_b) = make_board("no-commands");
        registry.push(reg_b);
        assert!(registry.command_tx("no-commands").is_none());
    }
}
