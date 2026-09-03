//! Application state and the deterministic input reducer.

use crate::{
    chess::{ChessGame, ChessMove, Color, GameStatus, MoveError, MoveRecord, Piece, Square},
    render::Orientation,
};

/// A logical user intent. Terminal key and mouse events are mapped to these
/// commands before they reach the application state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    MoveCursor { file_delta: i8, rank_delta: i8 },
    Activate,
    ActivateSquare(Square),
    Cancel,
    Promote(Piece),
    Undo,
    NewGame,
    FlipBoard,
    ToggleHelp,
    ToggleCoordinates,
    InstallEngine,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandResult {
    Continue,
    InstallEngineRequested,
    Quit,
}

/// One-time setup shown before a managed external engine can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineSetupState {
    /// Ask the user to approve downloading Maia and its isolated dependencies.
    InstallPrompt,
    /// Installation is active; `step` is suitable for displaying as progress.
    Installing { step: String },
    /// Installation stopped and can be retried without leaving the game.
    Failed { message: String },
}

/// Why a move returned by an external opponent was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EngineMoveError {
    #[error("the external engine does not own the side to move")]
    NotEngineTurn,
    #[error(transparent)]
    InvalidMove(#[from] MoveError),
}

/// A move awaiting the user's promotion-piece choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingPromotion {
    pub from: Square,
    pub to: Square,
}

/// All mutable state needed by the UI. The chess rules remain owned by
/// [`ChessGame`], while this type owns interaction state such as selection.
#[derive(Debug, Clone)]
pub struct App {
    game: ChessGame,
    cursor: Square,
    selected: Option<Square>,
    legal_moves: Vec<ChessMove>,
    promotion: Option<PendingPromotion>,
    orientation: Orientation,
    help_visible: bool,
    coordinates_visible: bool,
    engine_color: Option<Color>,
    engine_name: Option<String>,
    engine_failure: Option<String>,
    engine_setup_state: Option<EngineSetupState>,
    position_revision: u64,
    message: String,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        Self {
            game: ChessGame::new(),
            cursor: Square::E2,
            selected: None,
            legal_moves: Vec::new(),
            promotion: None,
            orientation: Orientation::White,
            help_visible: false,
            coordinates_visible: true,
            engine_color: None,
            engine_name: None,
            engine_failure: None,
            engine_setup_state: None,
            position_revision: 0,
            message: "White to move".to_owned(),
        }
    }

    /// Create a game where one side is controlled by an external engine.
    #[must_use]
    pub fn vs_engine(human_color: Color) -> Self {
        let mut app = Self::new();
        app.engine_color = Some(!human_color);
        app.engine_name = Some("Engine".to_owned());
        app.orientation = match human_color {
            Color::White => Orientation::White,
            Color::Black => Orientation::Black,
        };
        app.message = if app.is_human_turn() {
            "Your move".to_owned()
        } else {
            "Engine is starting".to_owned()
        };
        app
    }

    #[must_use]
    pub const fn game(&self) -> &ChessGame {
        &self.game
    }

    #[must_use]
    pub const fn cursor(&self) -> Square {
        self.cursor
    }

    #[must_use]
    pub const fn selected(&self) -> Option<Square> {
        self.selected
    }

    #[must_use]
    pub fn legal_moves(&self) -> &[ChessMove] {
        &self.legal_moves
    }

    #[must_use]
    pub const fn pending_promotion(&self) -> Option<PendingPromotion> {
        self.promotion
    }

    #[must_use]
    pub const fn orientation(&self) -> Orientation {
        self.orientation
    }

    #[must_use]
    pub const fn help_visible(&self) -> bool {
        self.help_visible
    }

    #[must_use]
    pub const fn coordinates_visible(&self) -> bool {
        self.coordinates_visible
    }

    #[must_use]
    pub fn message(&self) -> &str {
        self.engine_failure.as_deref().unwrap_or(&self.message)
    }

    #[must_use]
    pub const fn engine_color(&self) -> Option<Color> {
        self.engine_color
    }

    #[must_use]
    pub const fn is_engine_game(&self) -> bool {
        self.engine_color.is_some()
    }

    #[must_use]
    pub fn is_engine_turn(&self) -> bool {
        self.engine_color == Some(self.game.side_to_move()) && !self.game.is_game_over()
    }

    #[must_use]
    pub fn is_human_turn(&self) -> bool {
        !self.is_engine_turn()
    }

    #[must_use]
    pub const fn position_revision(&self) -> u64 {
        self.position_revision
    }

    #[must_use]
    pub fn engine_name(&self) -> Option<&str> {
        self.engine_name.as_deref()
    }

    #[must_use]
    pub fn engine_failure(&self) -> Option<&str> {
        self.engine_failure.as_deref()
    }

    #[must_use]
    pub const fn engine_setup_state(&self) -> Option<&EngineSetupState> {
        self.engine_setup_state.as_ref()
    }

    pub fn set_engine_setup_state(&mut self, state: EngineSetupState) {
        self.engine_setup_state = Some(state);
    }

    pub fn clear_engine_setup_state(&mut self) {
        self.engine_setup_state = None;
    }

    pub fn set_engine_name(&mut self, name: impl Into<String>) {
        if self.is_engine_game() {
            self.engine_name = Some(name.into());
        }
    }

    pub fn set_engine_status(&mut self, message: impl Into<String>) {
        if self.is_engine_game() && self.engine_failure.is_none() {
            self.message = message.into();
        }
    }

    pub fn set_engine_failure(&mut self, message: impl Into<String>) {
        if self.is_engine_game() && self.engine_failure.is_none() {
            self.engine_failure = Some(message.into());
        }
    }

    /// Validate and apply one move from the configured external opponent.
    pub fn apply_engine_move(&mut self, mv: ChessMove) -> Result<MoveRecord, EngineMoveError> {
        if !self.is_engine_turn() {
            return Err(EngineMoveError::NotEngineTurn);
        }
        let record = self.game.play_move(mv)?;
        self.finish_move(&record);
        Ok(record)
    }

    pub fn dispatch(&mut self, command: Command) -> CommandResult {
        if let Some(setup) = &self.engine_setup_state {
            return match command {
                Command::InstallEngine if !matches!(setup, EngineSetupState::Installing { .. }) => {
                    CommandResult::InstallEngineRequested
                }
                Command::Quit => CommandResult::Quit,
                _ => CommandResult::Continue,
            };
        }
        if self.engine_failure.is_some()
            && matches!(
                command,
                Command::Activate | Command::ActivateSquare(_) | Command::Promote(_)
            )
        {
            return CommandResult::Continue;
        }
        if self.help_visible
            && !matches!(
                command,
                Command::Cancel | Command::ToggleHelp | Command::Quit
            )
        {
            return CommandResult::Continue;
        }
        if self.promotion.is_some()
            && !matches!(
                command,
                Command::Promote(_) | Command::Cancel | Command::Quit
            )
        {
            return CommandResult::Continue;
        }
        match command {
            Command::MoveCursor {
                file_delta,
                rank_delta,
            } => self.move_cursor(file_delta, rank_delta),
            Command::Activate => self.activate_square(self.cursor),
            Command::ActivateSquare(square) => self.activate_square(square),
            Command::Cancel => self.cancel(),
            Command::Promote(piece) => self.promote(piece),
            Command::Undo => self.undo(),
            Command::NewGame => self.new_game(),
            Command::FlipBoard => self.orientation = self.orientation.flipped(),
            Command::ToggleHelp => self.help_visible = !self.help_visible,
            Command::ToggleCoordinates => self.coordinates_visible = !self.coordinates_visible,
            Command::InstallEngine => return CommandResult::Continue,
            Command::Quit => return CommandResult::Quit,
        }
        CommandResult::Continue
    }

    fn move_cursor(&mut self, file_delta: i8, rank_delta: i8) {
        if self.promotion.is_some() || self.help_visible {
            return;
        }
        let (file_delta, rank_delta) = match self.orientation {
            Orientation::White => (file_delta, rank_delta),
            Orientation::Black => (-file_delta, -rank_delta),
        };
        if let Some(square) = self.cursor.try_offset(file_delta, rank_delta) {
            self.cursor = square;
        }
    }

    fn activate_square(&mut self, square: Square) {
        if self.promotion.is_some() || self.help_visible {
            return;
        }
        self.cursor = square;
        if self.game.is_game_over() {
            self.message = "Game over — press n for a new game".to_owned();
            return;
        }
        if self.is_engine_turn() {
            self.message = format!("{} is thinking", self.engine_name().unwrap_or("Engine"));
            return;
        }

        let friendly = self
            .game
            .piece_at(square)
            .is_some_and(|piece| piece.color == self.game.side_to_move());

        let Some(from) = self.selected else {
            if friendly {
                self.select(square);
            } else {
                self.message = format!("Select a {} piece", color_name(self.game.side_to_move()));
            }
            return;
        };

        if square == from {
            self.clear_selection();
            self.message = format!("{} to move", color_name(self.game.side_to_move()));
            return;
        }
        if friendly {
            self.select(square);
            return;
        }

        let candidates: Vec<_> = self
            .legal_moves
            .iter()
            .copied()
            .filter(|mv| mv.to == square)
            .collect();
        if candidates.is_empty() {
            self.message = format!("{from} cannot move to {square}");
            return;
        }
        if candidates.iter().any(|mv| mv.promotion.is_some()) {
            self.promotion = Some(PendingPromotion { from, to: square });
            self.message = "Promote to queen, rook, bishop, or knight".to_owned();
            return;
        }
        self.play(from, square, None);
    }

    fn select(&mut self, square: Square) {
        self.selected = Some(square);
        self.legal_moves = self.game.legal_moves_from(square);
        self.message = if self.legal_moves.is_empty() {
            format!("{square} has no legal moves")
        } else {
            format!("{} legal moves from {square}", self.legal_moves.len())
        };
    }

    fn promote(&mut self, piece: Piece) {
        let Some(pending) = self.promotion else {
            return;
        };
        if !matches!(
            piece,
            Piece::Queen | Piece::Rook | Piece::Bishop | Piece::Knight
        ) {
            self.message = "Choose queen, rook, bishop, or knight".to_owned();
            return;
        }
        self.play(pending.from, pending.to, Some(piece));
    }

    fn play(&mut self, from: Square, to: Square, promotion: Option<Piece>) {
        match self.game.play(from, to, promotion) {
            Ok(record) => {
                self.finish_move(&record);
            }
            Err(error) => self.message = move_error_message(error),
        }
    }

    fn finish_move(&mut self, record: &MoveRecord) {
        self.cursor = record.mv.to;
        self.clear_selection();
        self.promotion = None;
        self.position_revision = self.position_revision.wrapping_add(1);
        self.message = move_message(record, self.game.status());
    }

    fn cancel(&mut self) {
        if self.help_visible {
            self.help_visible = false;
        } else if self.promotion.take().is_some() {
            self.message = "Promotion cancelled".to_owned();
        } else {
            self.clear_selection();
            self.message = if self.is_engine_turn() {
                format!("{} is thinking", self.engine_name().unwrap_or("Engine"))
            } else {
                format!("{} to move", color_name(self.game.side_to_move()))
            };
        }
    }

    fn undo(&mut self) {
        let plies = match self.engine_color {
            None => 1,
            Some(engine_color) if self.game.side_to_move() == engine_color => 1,
            Some(_) if self.game.history().len() >= 2 => 2,
            Some(_) => 0,
        };
        let mut undone = None;
        for _ in 0..plies {
            undone = self.game.undo();
        }
        if let Some(undone) = undone {
            self.promotion = None;
            self.clear_selection();
            self.cursor = undone.mv.from;
            self.position_revision = self.position_revision.wrapping_add(1);
            self.message = format!(
                "{} undone — {} to move",
                if self.is_engine_game() {
                    "Turn"
                } else {
                    "Move"
                },
                color_name(self.game.side_to_move())
            );
        } else {
            self.message = if self.is_engine_game() {
                "Nothing to undo yet".to_owned()
            } else {
                "Nothing to undo".to_owned()
            };
        }
    }

    fn new_game(&mut self) {
        self.game.reset();
        self.cursor = Square::E2;
        self.promotion = None;
        self.clear_selection();
        self.position_revision = self.position_revision.wrapping_add(1);
        self.message = "New game — White to move".to_owned();
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.legal_moves.clear();
    }
}

fn color_name(color: Color) -> &'static str {
    match color {
        Color::White => "White",
        Color::Black => "Black",
    }
}

fn move_message(record: &MoveRecord, status: GameStatus) -> String {
    match status {
        GameStatus::Checkmate { winner } => {
            format!("{} — checkmate, {} wins", record.san, color_name(winner))
        }
        GameStatus::Stalemate => format!("{} — stalemate", record.san),
        GameStatus::Draw { reason } => format!("{} — draw by {reason}", record.san),
        GameStatus::Check => format!("{} — check", record.san),
        GameStatus::Ongoing => format!("{} played", record.san),
    }
}

fn move_error_message(error: MoveError) -> String {
    match error {
        MoveError::PromotionRequired { .. } => "Choose a promotion piece".to_owned(),
        MoveError::InvalidPromotion(_) => "Invalid promotion piece".to_owned(),
        MoveError::GameOver(_) => "The game is already over".to_owned(),
        MoveError::Illegal { .. } => "That move is not legal".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_and_move_share_the_same_command_path() {
        let mut app = App::new();
        app.dispatch(Command::ActivateSquare(Square::E2));
        assert_eq!(app.selected(), Some(Square::E2));
        assert!(app.legal_moves().iter().any(|mv| mv.to == Square::E4));

        app.dispatch(Command::ActivateSquare(Square::E4));
        assert_eq!(app.game().side_to_move(), Color::Black);
        assert_eq!(app.game().last_move().unwrap().mv.to, Square::E4);
        assert_eq!(app.selected(), None);
    }

    #[test]
    fn selecting_another_friendly_piece_changes_selection() {
        let mut app = App::new();
        app.dispatch(Command::ActivateSquare(Square::E2));
        app.dispatch(Command::ActivateSquare(Square::G1));
        assert_eq!(app.selected(), Some(Square::G1));
    }

    #[test]
    fn orientation_changes_visual_cursor_direction() {
        let mut app = App::new();
        app.dispatch(Command::MoveCursor {
            file_delta: 1,
            rank_delta: 0,
        });
        assert_eq!(app.cursor(), Square::F2);
        app.dispatch(Command::FlipBoard);
        app.dispatch(Command::MoveCursor {
            file_delta: 1,
            rank_delta: 0,
        });
        assert_eq!(app.cursor(), Square::E2);
    }

    #[test]
    fn undo_restores_the_previous_turn() {
        let mut app = App::new();
        app.dispatch(Command::ActivateSquare(Square::E2));
        app.dispatch(Command::ActivateSquare(Square::E4));
        app.dispatch(Command::Undo);
        assert_eq!(app.game().side_to_move(), Color::White);
        assert!(app.game().history().is_empty());
        assert_eq!(app.cursor(), Square::E2);
    }
}
