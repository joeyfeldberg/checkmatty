//! Chess rules, position state, and reversible move history.
//!
//! [`Game`] is the application-facing adapter around `cozy-chess`. It keeps the
//! engine's compact board private so the rest of the application does not need
//! to know about engine-specific conventions. In particular, `cozy-chess`
//! encodes castling as a king move onto its rook; this module always exposes the
//! conventional king destination (`e1g1` or `e1c1`).
//!
//! # Invariants
//!
//! - `positions` contains the initial position followed by every position in
//!   `history`, so it always has exactly `history.len() + 1` entries.
//! - Every public [`Move`] is legal for the position in which it was recorded.
//! - A terminal game has no public legal moves and refuses further play until
//!   it is undone or reset.
//! - Threefold repetition uses FIDE position equivalence, including whether an
//!   en-passant capture is actually available.

use std::{fmt, fmt::Write as _, str::FromStr};

use cozy_chess::{Board, Move as EngineMove};

pub use cozy_chess::{Color, FenParseError, File, Piece, Rank, Square};

/// A chess piece together with its side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColoredPiece {
    /// The side that owns the piece.
    pub color: Color,
    /// The kind of piece.
    pub piece: Piece,
}

/// Backwards-friendly name for a piece found on the board.
pub type BoardPiece = ColoredPiece;

/// A move in conventional long algebraic form.
///
/// Castling uses the square where the king finishes (`e1g1`, for example),
/// unlike the internal `cozy-chess` representation. Promotion is required for
/// moves reaching the last rank; the adapter never silently chooses a queen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Move {
    /// The piece's starting square.
    pub from: Square,
    /// The piece's visible destination square.
    pub to: Square,
    /// The requested promotion piece, if this is a promotion.
    pub promotion: Option<Piece>,
}

impl Move {
    /// Construct a move request.
    pub const fn new(from: Square, to: Square, promotion: Option<Piece>) -> Self {
        Self {
            from,
            to,
            promotion,
        }
    }

    /// Return whether this move includes a promotion choice.
    pub const fn is_promotion(self) -> bool {
        self.promotion.is_some()
    }
}

/// More explicit alias for application code that also imports other move types.
pub type ChessMove = Move;

/// Error returned while parsing a coordinate move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a move like e2e4 or a7a8q")]
pub struct MoveParseError;

impl FromStr for Move {
    type Err = MoveParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !value.is_ascii() || !matches!(value.len(), 4 | 5) {
            return Err(MoveParseError);
        }

        let from = value
            .get(0..2)
            .ok_or(MoveParseError)?
            .parse()
            .map_err(|_| MoveParseError)?;
        let to = value
            .get(2..4)
            .ok_or(MoveParseError)?
            .parse()
            .map_err(|_| MoveParseError)?;
        let promotion = value
            .as_bytes()
            .get(4)
            .map(|byte| match byte.to_ascii_lowercase() {
                b'n' => Ok(Piece::Knight),
                b'b' => Ok(Piece::Bishop),
                b'r' => Ok(Piece::Rook),
                b'q' => Ok(Piece::Queen),
                _ => Err(MoveParseError),
            })
            .transpose()?;

        Ok(Self::new(from, to, promotion))
    }
}

impl fmt::Display for Move {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.from, self.to)?;
        if let Some(promotion) = self.promotion {
            write!(formatter, "{promotion}")?;
        }
        Ok(())
    }
}

/// The side on which a king castles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastleSide {
    /// King-side (short) castling.
    Kingside,
    /// Queen-side (long) castling.
    Queenside,
}

/// Castling rights retained by one side in the current position.
///
/// A right does not imply that castling is legal on this turn; pieces between
/// the king and rook, check, and attacked transit squares still matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CastlingRights {
    /// Whether the king-side right is retained.
    pub kingside: bool,
    /// Whether the queen-side right is retained.
    pub queenside: bool,
}

/// Why a game was adjudicated as a draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DrawReason {
    /// Fifty full moves (100 plies) passed without a pawn move or capture.
    FiftyMoveRule,
    /// The current position occurred at least three times.
    ThreefoldRepetition,
    /// Neither side has material with which any legal sequence can mate in the
    /// common dead-position cases handled by this adapter.
    InsufficientMaterial,
}

impl fmt::Display for DrawReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FiftyMoveRule => "fifty-move rule",
            Self::ThreefoldRepetition => "threefold repetition",
            Self::InsufficientMaterial => "insufficient material",
        })
    }
}

/// Application-level status of the current game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GameStatus {
    /// Play may continue and the side to move is not in check.
    Ongoing,
    /// Play may continue, but the side to move is in check.
    Check,
    /// The side to move is checkmated.
    Checkmate {
        /// The side that delivered mate.
        winner: Color,
    },
    /// The side to move has no legal move and is not in check.
    Stalemate,
    /// The game ended under a draw rule.
    Draw {
        /// The rule responsible for the draw.
        reason: DrawReason,
    },
}

impl GameStatus {
    /// Return whether no further moves may be played.
    pub const fn is_game_over(self) -> bool {
        matches!(
            self,
            Self::Checkmate { .. } | Self::Stalemate | Self::Draw { .. }
        )
    }

    /// Return the winning side, if this is checkmate.
    pub const fn winner(self) -> Option<Color> {
        if let Self::Checkmate { winner } = self {
            Some(winner)
        } else {
            None
        }
    }
}

impl fmt::Display for GameStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ongoing => formatter.write_str("ongoing"),
            Self::Check => formatter.write_str("check"),
            Self::Checkmate { winner } => write!(formatter, "checkmate ({winner} wins)"),
            Self::Stalemate => formatter.write_str("stalemate"),
            Self::Draw { reason } => write!(formatter, "draw by {reason}"),
        }
    }
}

/// Why a requested move could not be played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MoveError {
    /// The current game has already ended.
    #[error("the game is over ({0})")]
    GameOver(GameStatus),
    /// Pawns may only promote to a knight, bishop, rook, or queen.
    #[error("{0} is not a valid promotion piece")]
    InvalidPromotion(Piece),
    /// A legal promotion exists, but no promotion piece was selected.
    #[error("promotion piece required for {from}{to}")]
    PromotionRequired {
        /// The pawn's starting square.
        from: Square,
        /// The pawn's destination square.
        to: Square,
    },
    /// No legal move matched the request.
    #[error("illegal move from {from} to {to}")]
    Illegal {
        /// The requested starting square.
        from: Square,
        /// The requested destination square.
        to: Square,
        /// The requested promotion, if any.
        promotion: Option<Piece>,
    },
}

/// Immutable details about one successfully played move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRecord {
    /// The conventional move shown to the user.
    pub mv: Move,
    /// The side that made the move.
    pub color: Color,
    /// The moving piece before promotion.
    pub piece: Piece,
    /// The captured piece, including en-passant captures.
    pub captured: Option<ColoredPiece>,
    /// The square from which a captured piece was removed. This differs from
    /// `mv.to` only for en passant.
    pub capture_square: Option<Square>,
    /// The castling side, if this was castling.
    pub castle: Option<CastleSide>,
    /// Standard algebraic notation for the move.
    pub san: String,
    /// Whether the move left the opponent in check (also true for mate).
    pub gave_check: bool,
    /// Whether the move checkmated the opponent.
    pub gave_checkmate: bool,
}

impl MoveRecord {
    /// Return whether a piece was captured.
    pub const fn is_capture(&self) -> bool {
        self.captured.is_some()
    }

    /// Return whether this move was castling.
    pub const fn is_castle(&self) -> bool {
        self.castle.is_some()
    }

    /// Return whether the moving pawn promoted.
    pub const fn is_promotion(&self) -> bool {
        self.mv.promotion.is_some()
    }
}

/// A classical chess game with legal move generation and complete local history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    board: Board,
    initial_board: Board,
    history: Vec<MoveRecord>,
    undo_stack: Vec<Board>,
    positions: Vec<Board>,
}

/// An explicit alias for callers that prefer to avoid the generic `Game` name.
pub type ChessGame = Game;

impl Default for Game {
    fn default() -> Self {
        Self::new()
    }
}

impl Game {
    /// Create a standard chess game at its initial position.
    pub fn new() -> Self {
        Self::from_board(Board::default())
    }

    /// Create a game from a FEN position.
    ///
    /// The loaded position becomes this game's reset point. History before the
    /// FEN is necessarily unknown, so repetition counting starts at one.
    pub fn from_fen(fen: &str) -> Result<Self, FenParseError> {
        fen.parse().map(Self::from_board)
    }

    fn from_board(board: Board) -> Self {
        Self {
            board: board.clone(),
            initial_board: board.clone(),
            history: Vec::new(),
            undo_stack: Vec::new(),
            positions: vec![board],
        }
    }

    /// Replace the game with a FEN position, making it the new reset point.
    ///
    /// On parse failure, the existing game is unchanged.
    pub fn load_fen(&mut self, fen: &str) -> Result<(), FenParseError> {
        let board = fen.parse()?;
        *self = Self::from_board(board);
        Ok(())
    }

    /// Restore the position with which this game was constructed or loaded.
    pub fn reset(&mut self) {
        self.board = self.initial_board.clone();
        self.history.clear();
        self.undo_stack.clear();
        self.positions.clear();
        self.positions.push(self.board.clone());
    }

    /// Replace this game and its reset point with the standard start position.
    pub fn reset_to_start_position(&mut self) {
        *self = Self::new();
    }

    /// Serialize the current position as FEN.
    pub fn fen(&self) -> String {
        self.board.to_string()
    }

    /// Serialize this game's reset position as FEN.
    pub fn initial_fen(&self) -> String {
        self.initial_board.to_string()
    }

    /// Return the side whose turn it is.
    pub fn side_to_move(&self) -> Color {
        self.board.side_to_move()
    }

    /// Return the king square for `color`.
    pub fn king_square(&self, color: Color) -> Square {
        self.board.king(color)
    }

    /// Return the piece on a square, if occupied.
    pub fn piece_at(&self, square: Square) -> Option<ColoredPiece> {
        Some(ColoredPiece {
            color: self.board.color_on(square)?,
            piece: self.board.piece_on(square)?,
        })
    }

    /// Iterate occupied squares in rank-major order, from `a1` through `h8`.
    pub fn pieces(&self) -> impl DoubleEndedIterator<Item = (Square, ColoredPiece)> + '_ {
        Square::ALL
            .into_iter()
            .filter_map(|square| self.piece_at(square).map(|piece| (square, piece)))
    }

    /// Return whether a square is occupied.
    pub fn is_occupied(&self, square: Square) -> bool {
        self.board.occupied().has(square)
    }

    /// Return the current application-level status.
    pub fn status(&self) -> GameStatus {
        if !has_engine_moves(&self.board) {
            return if self.in_check() {
                GameStatus::Checkmate {
                    winner: !self.side_to_move(),
                }
            } else {
                GameStatus::Stalemate
            };
        }

        if has_insufficient_material(&self.board) {
            return GameStatus::Draw {
                reason: DrawReason::InsufficientMaterial,
            };
        }

        if self.board.halfmove_clock() >= 100 {
            return GameStatus::Draw {
                reason: DrawReason::FiftyMoveRule,
            };
        }

        if self.repetition_count() >= 3 {
            return GameStatus::Draw {
                reason: DrawReason::ThreefoldRepetition,
            };
        }

        if self.in_check() {
            GameStatus::Check
        } else {
            GameStatus::Ongoing
        }
    }

    /// Return whether the side to move is in check.
    pub fn in_check(&self) -> bool {
        !self.board.checkers().is_empty()
    }

    /// Return whether the current position is checkmate.
    pub fn is_checkmate(&self) -> bool {
        matches!(self.status(), GameStatus::Checkmate { .. })
    }

    /// Return whether the current position is stalemate.
    pub fn is_stalemate(&self) -> bool {
        self.status() == GameStatus::Stalemate
    }

    /// Return whether the current game refuses further moves.
    pub fn is_game_over(&self) -> bool {
        self.status().is_game_over()
    }

    /// Generate every legal move in conventional UI coordinates.
    ///
    /// Once an application-level draw has ended the game, this returns an
    /// empty list even though the underlying position may contain chess moves.
    pub fn legal_moves(&self) -> Vec<Move> {
        if self.is_game_over() {
            return Vec::new();
        }
        engine_moves(&self.board)
            .into_iter()
            .map(|mv| public_move(&self.board, mv))
            .collect()
    }

    /// Generate legal moves for the piece on `from`.
    pub fn legal_moves_from(&self, from: Square) -> Vec<Move> {
        self.legal_moves()
            .into_iter()
            .filter(|mv| mv.from == from)
            .collect()
    }

    /// Generate unique visible destination squares for a selected piece.
    ///
    /// Promotion choices share a destination and therefore appear only once.
    pub fn legal_destinations_from(&self, from: Square) -> Vec<Square> {
        let mut destinations = self
            .legal_moves_from(from)
            .into_iter()
            .map(|mv| mv.to)
            .collect::<Vec<_>>();
        destinations.sort_unstable();
        destinations.dedup();
        destinations
    }

    /// Resolve a legal move by start, visible destination, and promotion.
    pub fn find_move(
        &self,
        from: Square,
        to: Square,
        promotion: Option<Piece>,
    ) -> Result<Move, MoveError> {
        let status = self.status();
        if status.is_game_over() {
            return Err(MoveError::GameOver(status));
        }
        validate_promotion(promotion)?;

        let candidates = engine_moves(&self.board)
            .into_iter()
            .map(|mv| public_move(&self.board, mv))
            .filter(|mv| mv.from == from && mv.to == to)
            .collect::<Vec<_>>();

        if let Some(found) = candidates
            .iter()
            .copied()
            .find(|mv| mv.promotion == promotion)
        {
            return Ok(found);
        }

        if promotion.is_none() && candidates.iter().any(|mv| mv.promotion.is_some()) {
            return Err(MoveError::PromotionRequired { from, to });
        }

        Err(MoveError::Illegal {
            from,
            to,
            promotion,
        })
    }

    /// Return whether an exact move request can currently be played.
    pub fn is_legal_move(&self, mv: Move) -> bool {
        self.find_move(mv.from, mv.to, mv.promotion).is_ok()
    }

    /// Resolve and play a move selected by UI squares.
    pub fn play(
        &mut self,
        from: Square,
        to: Square,
        promotion: Option<Piece>,
    ) -> Result<MoveRecord, MoveError> {
        let mv = self.find_move(from, to, promotion)?;
        self.play_move(mv)
    }

    /// Play an exact conventional move and append it to history.
    pub fn play_move(&mut self, mv: Move) -> Result<MoveRecord, MoveError> {
        let mv = self.find_move(mv.from, mv.to, mv.promotion)?;
        let engine_move = find_engine_move(&self.board, mv).ok_or(MoveError::Illegal {
            from: mv.from,
            to: mv.to,
            promotion: mv.promotion,
        })?;

        let before = self.board.clone();
        let color = before.side_to_move();
        let piece = before
            .piece_on(engine_move.from)
            .expect("legal move must have a piece on its starting square");
        let castle = engine_castle_side(&before, engine_move);
        let (captured, capture_square) = captured_piece(&before, engine_move, castle);

        let mut after = before.clone();
        after.play_unchecked(engine_move);
        let gave_check = !after.checkers().is_empty();
        let gave_checkmate = gave_check && !has_engine_moves(&after);
        let san = move_san(
            &before,
            engine_move,
            mv,
            piece,
            captured.is_some(),
            castle,
            gave_check,
            gave_checkmate,
        );

        let record = MoveRecord {
            mv,
            color,
            piece,
            captured,
            capture_square,
            castle,
            san,
            gave_check,
            gave_checkmate,
        };

        self.undo_stack.push(before);
        self.board = after;
        self.history.push(record.clone());
        self.positions.push(self.board.clone());
        debug_assert_eq!(self.positions.len(), self.history.len() + 1);
        debug_assert_eq!(self.undo_stack.len(), self.history.len());
        Ok(record)
    }

    /// Undo one ply, returning its history record.
    pub fn undo(&mut self) -> Option<MoveRecord> {
        let board = self.undo_stack.pop()?;
        let record = self
            .history
            .pop()
            .expect("an undo snapshot must have a matching history record");
        self.positions
            .pop()
            .expect("an undo snapshot must have a matching position");
        self.board = board;
        debug_assert_eq!(self.positions.len(), self.history.len() + 1);
        Some(record)
    }

    /// Return whether at least one move can be undone.
    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    /// Return all successfully played moves.
    pub fn history(&self) -> &[MoveRecord] {
        &self.history
    }

    /// Return the most recently played move.
    pub fn last_move(&self) -> Option<&MoveRecord> {
        self.history.last()
    }

    /// Return the number of played plies.
    pub fn ply_count(&self) -> usize {
        self.history.len()
    }

    /// Return how many times the current FIDE-equivalent position occurred.
    pub fn repetition_count(&self) -> usize {
        self.positions
            .iter()
            .filter(|position| position.same_position(&self.board))
            .count()
    }

    /// Return the number of reversible plies since the last pawn move/capture.
    pub fn halfmove_clock(&self) -> u8 {
        self.board.halfmove_clock()
    }

    /// Return the FEN fullmove number.
    pub fn fullmove_number(&self) -> u16 {
        self.board.fullmove_number()
    }

    /// Return remaining plies before the fifty-move rule threshold.
    pub fn plies_until_fifty_move_rule(&self) -> u8 {
        100_u8.saturating_sub(self.halfmove_clock())
    }

    /// Return the legal en-passant target from FEN semantics, if one exists.
    pub fn en_passant_target(&self) -> Option<Square> {
        self.board
            .en_passant()
            .map(|file| Square::new(file, Rank::Sixth.relative_to(self.board.side_to_move())))
    }

    /// Return one side's retained castling rights.
    pub fn castle_rights(&self, color: Color) -> CastlingRights {
        let rights = self.board.castle_rights(color);
        CastlingRights {
            kingside: rights.short.is_some(),
            queenside: rights.long.is_some(),
        }
    }

    /// Return whether one castling right is retained.
    pub fn has_castle_right(&self, color: Color, side: CastleSide) -> bool {
        let rights = self.castle_rights(color);
        match side {
            CastleSide::Kingside => rights.kingside,
            CastleSide::Queenside => rights.queenside,
        }
    }

    /// Return the engine's incremental hash for cache invalidation.
    ///
    /// This excludes the move counters. Use [`Game::repetition_count`] rather
    /// than comparing hashes to adjudicate repetition.
    pub fn position_hash(&self) -> u64 {
        self.board.hash()
    }
}

impl fmt::Display for Game {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.board.fmt(formatter)
    }
}

fn validate_promotion(promotion: Option<Piece>) -> Result<(), MoveError> {
    if let Some(piece @ (Piece::Pawn | Piece::King)) = promotion {
        Err(MoveError::InvalidPromotion(piece))
    } else {
        Ok(())
    }
}

fn engine_moves(board: &Board) -> Vec<EngineMove> {
    let mut result = Vec::new();
    board.generate_moves(|moves| {
        result.extend(moves);
        false
    });
    result
}

fn has_engine_moves(board: &Board) -> bool {
    board.generate_moves(|_| true)
}

fn engine_castle_side(board: &Board, mv: EngineMove) -> Option<CastleSide> {
    let color = board.side_to_move();
    (board.piece_on(mv.from) == Some(Piece::King) && board.color_on(mv.to) == Some(color))
        .then_some(if mv.to.file() > mv.from.file() {
            CastleSide::Kingside
        } else {
            CastleSide::Queenside
        })
}

fn public_move(board: &Board, mv: EngineMove) -> Move {
    let to = match engine_castle_side(board, mv) {
        Some(CastleSide::Kingside) => {
            Square::new(File::G, Rank::First.relative_to(board.side_to_move()))
        }
        Some(CastleSide::Queenside) => {
            Square::new(File::C, Rank::First.relative_to(board.side_to_move()))
        }
        None => mv.to,
    };
    Move::new(mv.from, to, mv.promotion)
}

fn find_engine_move(board: &Board, requested: Move) -> Option<EngineMove> {
    engine_moves(board)
        .into_iter()
        .find(|&candidate| public_move(board, candidate) == requested)
}

fn captured_piece(
    board: &Board,
    mv: EngineMove,
    castle: Option<CastleSide>,
) -> (Option<ColoredPiece>, Option<Square>) {
    if castle.is_some() {
        return (None, None);
    }

    let moved = board
        .piece_on(mv.from)
        .expect("legal move must have a moving piece");
    let capture_square = if board.piece_on(mv.to).is_some() {
        Some(mv.to)
    } else if moved == Piece::Pawn && mv.from.file() != mv.to.file() {
        Some(Square::new(mv.to.file(), mv.from.rank()))
    } else {
        None
    };
    let captured = capture_square.and_then(|square| {
        Some(ColoredPiece {
            color: board.color_on(square)?,
            piece: board.piece_on(square)?,
        })
    });
    (captured, capture_square.filter(|_| captured.is_some()))
}

#[allow(clippy::too_many_arguments)]
fn move_san(
    before: &Board,
    engine_move: EngineMove,
    mv: Move,
    piece: Piece,
    is_capture: bool,
    castle: Option<CastleSide>,
    gave_check: bool,
    gave_checkmate: bool,
) -> String {
    let mut san = match castle {
        Some(CastleSide::Kingside) => String::from("O-O"),
        Some(CastleSide::Queenside) => String::from("O-O-O"),
        None => {
            let mut notation = String::new();
            if piece == Piece::Pawn {
                if is_capture {
                    write!(notation, "{}", mv.from.file())
                        .expect("writing to a string cannot fail");
                }
            } else {
                notation.push(san_piece_letter(piece));
                write_disambiguation(&mut notation, before, engine_move, mv, piece);
            }
            if is_capture {
                notation.push('x');
            }
            write!(notation, "{}", mv.to).expect("writing to a string cannot fail");
            if let Some(promotion) = mv.promotion {
                notation.push('=');
                notation.push(san_piece_letter(promotion));
            }
            notation
        }
    };

    if gave_checkmate {
        san.push('#');
    } else if gave_check {
        san.push('+');
    }
    san
}

fn write_disambiguation(
    notation: &mut String,
    board: &Board,
    played: EngineMove,
    public: Move,
    piece: Piece,
) {
    let alternatives = engine_moves(board)
        .into_iter()
        .filter(|candidate| {
            candidate.from != played.from
                && board.piece_on(candidate.from) == Some(piece)
                && public_move(board, *candidate).to == public.to
        })
        .collect::<Vec<_>>();
    if alternatives.is_empty() {
        return;
    }

    let file_is_unique = alternatives
        .iter()
        .all(|candidate| candidate.from.file() != played.from.file());
    let rank_is_unique = alternatives
        .iter()
        .all(|candidate| candidate.from.rank() != played.from.rank());
    if file_is_unique {
        write!(notation, "{}", played.from.file()).expect("writing to a string cannot fail");
    } else if rank_is_unique {
        write!(notation, "{}", played.from.rank()).expect("writing to a string cannot fail");
    } else {
        write!(notation, "{}", played.from).expect("writing to a string cannot fail");
    }
}

const fn san_piece_letter(piece: Piece) -> char {
    match piece {
        Piece::Pawn => 'P',
        Piece::Knight => 'N',
        Piece::Bishop => 'B',
        Piece::Rook => 'R',
        Piece::Queen => 'Q',
        Piece::King => 'K',
    }
}

fn has_insufficient_material(board: &Board) -> bool {
    if !(board.pieces(Piece::Pawn) | board.pieces(Piece::Rook) | board.pieces(Piece::Queen))
        .is_empty()
    {
        return false;
    }

    let knights = board.pieces(Piece::Knight);
    let bishops = board.pieces(Piece::Bishop);
    let minor_count = knights.len() + bishops.len();
    if minor_count <= 1 {
        return true;
    }

    // With no knights, bishops confined to one color complex cannot mate.
    if knights.is_empty() {
        let mut square_colors = bishops.into_iter().map(square_is_light);
        if let Some(first) = square_colors.next() {
            return square_colors.all(|color| color == first);
        }
    }
    false
}

const fn square_is_light(square: Square) -> bool {
    (square.file() as u8 + square.rank() as u8).is_multiple_of(2)
}
