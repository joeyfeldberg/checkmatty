//! Semantic board scenes and adaptive board renderers.
//!
//! This module deliberately stops at pixels and terminal cells. Terminal graphics
//! protocol negotiation and image placement live in [`crate::terminal`].

use std::{fmt, str::FromStr, sync::OnceLock};

use image::RgbaImage;
use ratatui::{
    buffer::Buffer,
    layout::Rect as TuiRect,
    style::{Color as TuiColor, Modifier as TuiModifier, Style as TuiStyle},
    widgets::Widget,
};
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, Path, PathBuilder,
    Pixmap, Point, Rect, SpreadMode, Stroke, Transform,
};

const BOARD_LEN: usize = 8;
const SQUARE_COUNT: usize = BOARD_LEN * BOARD_LEN;
const MIN_RASTER_SIDE: u32 = BOARD_LEN as u32;
const MAX_RASTER_SIDE: u32 = 4_096;
const LOW_RES_PIECE_GRID: u32 = 6;

/// A square using zero-based chess coordinates (`a1 == (0, 0)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Square {
    pub file: u8,
    pub rank: u8,
}

impl Square {
    /// Creates a square when both coordinates are on the board.
    #[must_use]
    pub const fn new(file: u8, rank: u8) -> Option<Self> {
        if file < 8 && rank < 8 {
            Some(Self { file, rank })
        } else {
            None
        }
    }

    /// Creates a square from its rank-major index (`a1 == 0`, `h8 == 63`).
    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < SQUARE_COUNT {
            Some(Self {
                file: (index % BOARD_LEN) as u8,
                rank: (index / BOARD_LEN) as u8,
            })
        } else {
            None
        }
    }

    /// Returns the rank-major index for this square.
    #[must_use]
    pub const fn index(self) -> usize {
        self.rank as usize * BOARD_LEN + self.file as usize
    }

    #[must_use]
    pub const fn file_char(self) -> char {
        (b'a' + self.file) as char
    }

    #[must_use]
    pub const fn rank_char(self) -> char {
        (b'1' + self.rank) as char
    }
}

impl fmt::Display for Square {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.file_char(), self.rank_char())
    }
}

/// Error returned when parsing an algebraic square such as `e4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseSquareError;

impl fmt::Display for ParseSquareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected a square from a1 through h8")
    }
}

impl std::error::Error for ParseSquareError {}

impl FromStr for Square {
    type Err = ParseSquareError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.len() != 2 {
            return Err(ParseSquareError);
        }
        let file = bytes[0].to_ascii_lowercase().wrapping_sub(b'a');
        let rank = bytes[1].wrapping_sub(b'1');
        Self::new(file, rank).ok_or(ParseSquareError)
    }
}

impl From<cozy_chess::Square> for Square {
    fn from(value: cozy_chess::Square) -> Self {
        Self::from_index(value as usize).expect("cozy-chess squares are always in range")
    }
}

impl From<Square> for cozy_chess::Square {
    fn from(value: Square) -> Self {
        Self::index(value.index())
    }
}

/// Which player owns a displayed piece.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    White,
    Black,
}

impl From<cozy_chess::Color> for Side {
    fn from(value: cozy_chess::Color) -> Self {
        match value {
            cozy_chess::Color::White => Self::White,
            cozy_chess::Color::Black => Self::Black,
        }
    }
}

impl From<Side> for cozy_chess::Color {
    fn from(value: Side) -> Self {
        match value {
            Side::White => Self::White,
            Side::Black => Self::Black,
        }
    }
}

/// The visual identity of a chess piece.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PieceKind {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}

impl From<cozy_chess::Piece> for PieceKind {
    fn from(value: cozy_chess::Piece) -> Self {
        match value {
            cozy_chess::Piece::Pawn => Self::Pawn,
            cozy_chess::Piece::Knight => Self::Knight,
            cozy_chess::Piece::Bishop => Self::Bishop,
            cozy_chess::Piece::Rook => Self::Rook,
            cozy_chess::Piece::Queen => Self::Queen,
            cozy_chess::Piece::King => Self::King,
        }
    }
}

impl From<PieceKind> for cozy_chess::Piece {
    fn from(value: PieceKind) -> Self {
        match value {
            PieceKind::Pawn => Self::Pawn,
            PieceKind::Knight => Self::Knight,
            PieceKind::Bishop => Self::Bishop,
            PieceKind::Rook => Self::Rook,
            PieceKind::Queen => Self::Queen,
            PieceKind::King => Self::King,
        }
    }
}

/// A terminal-independent piece prepared for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewPiece {
    pub side: Side,
    pub kind: PieceKind,
}

impl ViewPiece {
    #[must_use]
    pub const fn new(side: Side, kind: PieceKind) -> Self {
        Self { side, kind }
    }

    #[must_use]
    pub fn from_cozy(piece: cozy_chess::Piece, color: cozy_chess::Color) -> Self {
        Self::new(color.into(), piece.into())
    }
}

/// One piece travelling between two squares in a raster animation frame.
///
/// The board passed to [`compose_board_rgba_with_tweens`] is expected to be the
/// position after the move. The compositor temporarily hides the matching
/// destination piece and paints it at this interpolated location, avoiding a
/// duplicate piece at the end square.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PieceTween {
    pub piece: ViewPiece,
    pub from: Square,
    pub to: Square,
    /// Eased progress in the inclusive range `0.0..=1.0`.
    pub progress: f32,
}

impl PieceTween {
    #[must_use]
    pub fn new(piece: ViewPiece, from: Square, to: Square, progress: f32) -> Self {
        Self {
            piece,
            from,
            to,
            progress: normalized_progress(progress),
        }
    }

    /// Returns the fractional `(column, row)` from the displayed top-left.
    #[must_use]
    pub fn view_position(self, orientation: Orientation) -> (f32, f32) {
        let (from_column, from_row) = orientation.square_to_view(self.from);
        let (to_column, to_row) = orientation.square_to_view(self.to);
        (
            lerp(f32::from(from_column), f32::from(to_column), self.progress),
            lerp(f32::from(from_row), f32::from(to_row), self.progress),
        )
    }
}

/// A smooth, symmetric ease-in/ease-out curve for move animations.
///
/// Inputs outside `0.0..=1.0` are clamped, and non-finite inputs resolve to
/// the start of the animation.
#[must_use]
pub fn ease_move_progress(progress: f32) -> f32 {
    let progress = normalized_progress(progress);
    if progress < 0.5 {
        4.0 * progress * progress * progress
    } else {
        1.0 - (-2.0 * progress + 2.0).powi(3) / 2.0
    }
}

fn normalized_progress(progress: f32) -> f32 {
    if progress.is_finite() {
        progress.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn lerp(from: f32, to: f32, progress: f32) -> f32 {
    from + (to - from) * progress
}

/// The side from which the board is viewed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Orientation {
    #[default]
    White,
    Black,
}

impl Orientation {
    #[must_use]
    pub const fn flipped(self) -> Self {
        match self {
            Self::White => Self::Black,
            Self::Black => Self::White,
        }
    }

    /// Maps a chess square to `(column, row)` from the displayed top-left.
    #[must_use]
    pub const fn square_to_view(self, square: Square) -> (u8, u8) {
        match self {
            Self::White => (square.file, 7 - square.rank),
            Self::Black => (7 - square.file, square.rank),
        }
    }

    /// Maps a top-left-relative display coordinate back to a chess square.
    #[must_use]
    pub const fn view_to_square(self, column: u8, row: u8) -> Option<Square> {
        if column >= 8 || row >= 8 {
            return None;
        }
        match self {
            Self::White => Square::new(column, 7 - row),
            Self::Black => Square::new(7 - column, row),
        }
    }
}

/// How a legal destination should be decorated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetKind {
    Quiet,
    Capture,
}

/// A completed move highlighted on the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LastMove {
    pub from: Square,
    pub to: Square,
}

impl LastMove {
    #[must_use]
    pub const fn new(from: Square, to: Square) -> Self {
        Self { from, to }
    }

    #[must_use]
    fn contains(self, square: Square) -> bool {
        self.from == square || self.to == square
    }
}

/// Everything the renderer needs for one deterministic board frame.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BoardScene {
    pub pieces: [Option<ViewPiece>; SQUARE_COUNT],
    pub orientation: Orientation,
    /// Keyboard/mouse focus, distinct from a selected piece.
    pub cursor: Option<Square>,
    pub selected: Option<Square>,
    pub legal_targets: [Option<TargetKind>; SQUARE_COUNT],
    pub last_move: Option<LastMove>,
    pub checked: Option<Square>,
    pub show_coordinates: bool,
}

impl Default for BoardScene {
    fn default() -> Self {
        Self::empty()
    }
}

impl BoardScene {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            pieces: [None; SQUARE_COUNT],
            orientation: Orientation::White,
            cursor: None,
            selected: None,
            legal_targets: [None; SQUARE_COUNT],
            last_move: None,
            checked: None,
            show_coordinates: true,
        }
    }

    #[must_use]
    pub fn starting_position() -> Self {
        let mut scene = Self::empty();
        let back_rank = [
            PieceKind::Rook,
            PieceKind::Knight,
            PieceKind::Bishop,
            PieceKind::Queen,
            PieceKind::King,
            PieceKind::Bishop,
            PieceKind::Knight,
            PieceKind::Rook,
        ];
        for file in 0..8 {
            let white_home = Square::new(file, 0).expect("loop bounds are valid");
            let white_pawn = Square::new(file, 1).expect("loop bounds are valid");
            let black_pawn = Square::new(file, 6).expect("loop bounds are valid");
            let black_home = Square::new(file, 7).expect("loop bounds are valid");
            scene.set_piece(
                white_home,
                Some(ViewPiece::new(Side::White, back_rank[usize::from(file)])),
            );
            scene.set_piece(
                white_pawn,
                Some(ViewPiece::new(Side::White, PieceKind::Pawn)),
            );
            scene.set_piece(
                black_pawn,
                Some(ViewPiece::new(Side::Black, PieceKind::Pawn)),
            );
            scene.set_piece(
                black_home,
                Some(ViewPiece::new(Side::Black, back_rank[usize::from(file)])),
            );
        }
        scene
    }

    #[must_use]
    pub const fn piece_at(&self, square: Square) -> Option<ViewPiece> {
        self.pieces[square.index()]
    }

    pub const fn set_piece(&mut self, square: Square, piece: Option<ViewPiece>) {
        self.pieces[square.index()] = piece;
    }

    #[must_use]
    pub const fn target_at(&self, square: Square) -> Option<TargetKind> {
        self.legal_targets[square.index()]
    }

    pub const fn set_target(&mut self, square: Square, target: Option<TargetKind>) {
        self.legal_targets[square.index()] = target;
    }

    pub fn clear_targets(&mut self) {
        self.legal_targets.fill(None);
    }
}

/// An RGB color used by both the cell and pixel renderers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Rgb {
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    #[must_use]
    const fn blend(self, overlay: Self, alpha: u8) -> Self {
        let alpha = alpha as u16;
        let inverse = 255 - alpha;
        Self::new(
            ((self.red as u16 * inverse + overlay.red as u16 * alpha) / 255) as u8,
            ((self.green as u16 * inverse + overlay.green as u16 * alpha) / 255) as u8,
            ((self.blue as u16 * inverse + overlay.blue as u16 * alpha) / 255) as u8,
        )
    }

    #[must_use]
    const fn shift(self, amount: i16) -> Self {
        Self::new(
            shift_channel(self.red, amount),
            shift_channel(self.green, amount),
            shift_channel(self.blue, amount),
        )
    }

    #[must_use]
    const fn tui(self) -> TuiColor {
        TuiColor::Rgb(self.red, self.green, self.blue)
    }

    #[must_use]
    fn skia(self, alpha: u8) -> Color {
        Color::from_rgba8(self.red, self.green, self.blue, alpha)
    }
}

const fn shift_channel(value: u8, amount: i16) -> u8 {
    let shifted = value as i16 + amount;
    if shifted < 0 {
        0
    } else if shifted > 255 {
        255
    } else {
        shifted as u8
    }
}

/// Shared palette for the cell and raster renderers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BoardTheme {
    pub light_square: Rgb,
    pub dark_square: Rgb,
    pub white_piece: Rgb,
    pub black_piece: Rgb,
    pub cursor: Rgb,
    pub selected: Rgb,
    pub last_move: Rgb,
    pub legal_target: Rgb,
    pub capture_target: Rgb,
    pub check: Rgb,
}

impl Default for BoardTheme {
    fn default() -> Self {
        Self {
            light_square: Rgb::new(218, 225, 205),
            dark_square: Rgb::new(91, 120, 95),
            white_piece: Rgb::new(247, 242, 224),
            black_piece: Rgb::new(42, 47, 57),
            cursor: Rgb::new(102, 214, 226),
            selected: Rgb::new(244, 195, 64),
            last_move: Rgb::new(229, 193, 72),
            legal_target: Rgb::new(39, 78, 63),
            capture_target: Rgb::new(180, 65, 57),
            check: Rgb::new(212, 55, 55),
        }
    }
}

impl BoardTheme {
    #[must_use]
    const fn square_color(self, square: Square) -> Rgb {
        if (square.file + square.rank).is_multiple_of(2) {
            self.dark_square
        } else {
            self.light_square
        }
    }

    #[must_use]
    fn decorated_square_color(self, scene: &BoardScene, square: Square) -> Rgb {
        let mut color = self.square_color(square);
        if let Some(last_move) = scene.last_move
            && last_move.contains(square)
        {
            color = color.blend(self.last_move, 100);
        }
        if scene.selected == Some(square) {
            color = color.blend(self.selected, 145);
        }
        if scene.checked == Some(square) {
            color = color.blend(self.check, 128);
        }
        color
    }
}

/// Pixel dimensions of one terminal cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

impl Default for CellSize {
    fn default() -> Self {
        Self::new(8, 16)
    }
}

impl CellSize {
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Self {
        Self {
            width: if width == 0 { 1 } else { width },
            height: if height == 0 { 1 } else { height },
        }
    }
}

/// A centered cell-grid board whose squares are as physically square as the
/// available terminal geometry permits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoardLayout {
    pub board: TuiRect,
    pub square_columns: u16,
    pub square_rows: u16,
    pub cell_size: CellSize,
}

impl BoardLayout {
    /// Fits a board in `area`, preferring the largest physical square size and
    /// then the lowest pixel aspect-ratio error.
    #[must_use]
    pub fn fit(area: TuiRect, cell_size: CellSize) -> Self {
        let max_columns = area.width / BOARD_LEN as u16;
        let max_rows = area.height / BOARD_LEN as u16;
        if max_columns == 0 || max_rows == 0 {
            return Self {
                board: TuiRect::new(area.x, area.y, 0, 0),
                square_columns: 0,
                square_rows: 0,
                cell_size,
            };
        }

        let mut best = (0_u32, u32::MAX, 0_u32, 1_u16, 1_u16);
        for rows in 1..=max_rows {
            for columns in 1..=max_columns {
                let width_px = u32::from(columns) * u32::from(cell_size.width);
                let height_px = u32::from(rows) * u32::from(cell_size.height);
                let candidate = (
                    width_px.min(height_px),
                    width_px.abs_diff(height_px),
                    u32::from(columns) * u32::from(rows),
                    columns,
                    rows,
                );
                if better_layout(candidate, best) {
                    best = candidate;
                }
            }
        }

        let square_columns = best.3;
        let square_rows = best.4;
        let width = square_columns * BOARD_LEN as u16;
        let height = square_rows * BOARD_LEN as u16;
        Self {
            board: TuiRect::new(
                area.x.saturating_add((area.width - width) / 2),
                area.y.saturating_add((area.height - height) / 2),
                width,
                height,
            ),
            square_columns,
            square_rows,
            cell_size,
        }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.square_columns == 0 || self.square_rows == 0
    }

    #[must_use]
    pub const fn square_pixels(self) -> (u32, u32) {
        (
            self.square_columns as u32 * self.cell_size.width as u32,
            self.square_rows as u32 * self.cell_size.height as u32,
        )
    }

    /// Maps an absolute terminal cell coordinate to a chess square.
    #[must_use]
    pub fn square_at(self, x: u16, y: u16, orientation: Orientation) -> Option<Square> {
        if self.is_empty() {
            return None;
        }
        let relative_x = x.checked_sub(self.board.x)?;
        let relative_y = y.checked_sub(self.board.y)?;
        if relative_x >= self.board.width || relative_y >= self.board.height {
            return None;
        }
        let column = u8::try_from(relative_x / self.square_columns).ok()?;
        let row = u8::try_from(relative_y / self.square_rows).ok()?;
        orientation.view_to_square(column, row)
    }

    /// Returns the terminal-cell rectangle occupied by a chess square.
    #[must_use]
    pub fn square_rect(self, square: Square, orientation: Orientation) -> Option<TuiRect> {
        if self.is_empty() {
            return None;
        }
        let (column, row) = orientation.square_to_view(square);
        Some(TuiRect::new(
            self.board
                .x
                .saturating_add(u16::from(column) * self.square_columns),
            self.board
                .y
                .saturating_add(u16::from(row) * self.square_rows),
            self.square_columns,
            self.square_rows,
        ))
    }
}

const fn better_layout(
    candidate: (u32, u32, u32, u16, u16),
    current: (u32, u32, u32, u16, u16),
) -> bool {
    candidate.0 > current.0
        || (candidate.0 == current.0 && candidate.1 < current.1)
        || (candidate.0 == current.0 && candidate.1 == current.1 && candidate.2 > current.2)
}

/// A truecolor Unicode fallback board for Ratatui.
#[derive(Clone, Copy, Debug)]
pub struct BoardWidget<'a> {
    scene: &'a BoardScene,
    theme: BoardTheme,
    cell_size: CellSize,
}

impl<'a> BoardWidget<'a> {
    #[must_use]
    pub fn new(scene: &'a BoardScene) -> Self {
        Self {
            scene,
            theme: BoardTheme::default(),
            cell_size: CellSize::default(),
        }
    }

    #[must_use]
    pub const fn theme(mut self, theme: BoardTheme) -> Self {
        self.theme = theme;
        self
    }

    #[must_use]
    pub const fn cell_size(mut self, cell_size: CellSize) -> Self {
        self.cell_size = cell_size;
        self
    }

    #[must_use]
    pub fn layout(self, area: TuiRect) -> BoardLayout {
        BoardLayout::fit(area, self.cell_size)
    }
}

impl Widget for BoardWidget<'_> {
    fn render(self, area: TuiRect, buffer: &mut Buffer) {
        render_unicode_board(self.scene, self.theme, self.layout(area), buffer);
    }
}

/// Renders a scene using an already-computed layout and returns it for hit testing.
pub fn render_unicode_board(
    scene: &BoardScene,
    theme: BoardTheme,
    layout: BoardLayout,
    buffer: &mut Buffer,
) -> BoardLayout {
    if layout.is_empty() {
        return layout;
    }

    for view_row in 0..8_u8 {
        for view_column in 0..8_u8 {
            let square = scene
                .orientation
                .view_to_square(view_column, view_row)
                .expect("board display coordinates are valid");
            let area = layout
                .square_rect(square, scene.orientation)
                .expect("non-empty board has square rectangles");
            let mut background = theme.decorated_square_color(scene, square);
            if let Some(target) = scene.target_at(square) {
                background = match target {
                    TargetKind::Quiet => background.blend(theme.legal_target, 45),
                    TargetKind::Capture => background.blend(theme.capture_target, 65),
                };
            }
            fill_terminal_square(buffer, area, background);
            if scene.cursor == Some(square) {
                mark_terminal_square(buffer, area, theme.cursor);
            }

            if let Some(piece) = scene.piece_at(square) {
                if !draw_terminal_piece_sprite(buffer, area, piece, theme) {
                    let cell = &mut buffer[(area.x + area.width / 2, area.y + area.height / 2)];
                    cell.set_symbol(piece_symbol(piece));
                    cell.set_style(
                        TuiStyle::new()
                            .fg(match piece.side {
                                Side::White => theme.white_piece.tui(),
                                Side::Black => theme.black_piece.tui(),
                            })
                            .add_modifier(TuiModifier::BOLD),
                    );
                }
            } else if scene.target_at(square) == Some(TargetKind::Quiet) {
                draw_terminal_quiet_target(buffer, area, theme.legal_target);
            }

            draw_terminal_coordinates(scene, theme, square, area, view_column, view_row, buffer);
        }
    }
    layout
}

fn fill_terminal_square(buffer: &mut Buffer, area: TuiRect, color: Rgb) {
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            buffer[(x, y)].set_symbol(" ").set_bg(color.tui());
        }
    }
}

fn outline_terminal_square(buffer: &mut Buffer, area: TuiRect, color: Rgb) {
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    for x in area.x..=right {
        buffer[(x, area.y)].set_bg(color.tui());
        buffer[(x, bottom)].set_bg(color.tui());
    }
    for y in area.y..=bottom {
        buffer[(area.x, y)].set_bg(color.tui());
        buffer[(right, y)].set_bg(color.tui());
    }
}

fn mark_terminal_square(buffer: &mut Buffer, area: TuiRect, color: Rgb) {
    if area.width >= 3 && area.height >= 3 {
        outline_terminal_square(buffer, area, color);
        return;
    }

    // A full rectangular outline degenerates into a solid fill when a square
    // is only one or two rows high. Side rails remain legible at that density.
    let right = area.x + area.width - 1;
    for y in area.y..area.y.saturating_add(area.height) {
        buffer[(area.x, y)].set_bg(color.tui());
        buffer[(right, y)].set_bg(color.tui());
    }
}

fn draw_terminal_quiet_target(buffer: &mut Buffer, area: TuiRect, color: Rgb) {
    let y = area.y + area.height / 2;
    let center = area.x + area.width / 2;
    if area.width.is_multiple_of(2) && area.width >= 2 {
        // A single glyph cannot sit on the boundary at the center of an even
        // cell span. Two complementary half-circles form one centered marker.
        buffer[(center - 1, y)].set_symbol("◗").set_fg(color.tui());
        buffer[(center, y)].set_symbol("◖").set_fg(color.tui());
    } else {
        buffer[(center, y)].set_symbol("•").set_fg(color.tui());
    }
}

/// Paint a two-tone piece with Unicode half blocks when a square has enough
/// cells for the same 6x6 silhouette used by the low-resolution raster path.
/// This keeps the protocol-free renderer visually substantial instead of
/// shrinking every piece to one terminal glyph.
fn draw_terminal_piece_sprite(
    buffer: &mut Buffer,
    area: TuiRect,
    piece: ViewPiece,
    theme: BoardTheme,
) -> bool {
    let scale =
        (area.width / LOW_RES_PIECE_GRID as u16).min(area.height / (LOW_RES_PIECE_GRID as u16 / 2));
    if scale == 0 {
        return false;
    }

    let sprite_width = LOW_RES_PIECE_GRID as u16 * scale;
    let sprite_rows = LOW_RES_PIECE_GRID as u16 / 2 * scale;
    let origin_x = area.x + (area.width - sprite_width) / 2;
    let origin_y = area.y + (area.height - sprite_rows) / 2;
    let sprite = low_res_piece_sprite(piece.kind);
    let (body, detail) = low_res_piece_colors(piece, theme);

    for cell_row in 0..sprite_rows {
        for cell_column in 0..sprite_width {
            let sample_column = usize::from(cell_column / scale);
            let top_row = usize::from((cell_row * 2) / scale);
            let bottom_row = usize::from((cell_row * 2 + 1) / scale);
            let top = terminal_sample_color(
                sprite[top_row * LOW_RES_PIECE_GRID as usize + sample_column],
                body,
                detail,
            );
            let bottom = terminal_sample_color(
                sprite[bottom_row * LOW_RES_PIECE_GRID as usize + sample_column],
                body,
                detail,
            );
            let cell = &mut buffer[(origin_x + cell_column, origin_y + cell_row)];
            match (top, bottom) {
                (None, None) => {}
                (Some(top), None) => {
                    cell.set_symbol("▀").set_fg(top.tui());
                }
                (None, Some(bottom)) => {
                    cell.set_symbol("▄").set_fg(bottom.tui());
                }
                (Some(top), Some(bottom)) if top == bottom => {
                    cell.set_symbol("█").set_fg(top.tui());
                }
                (Some(top), Some(bottom)) => {
                    cell.set_symbol("▀").set_fg(top.tui()).set_bg(bottom.tui());
                }
            }
        }
    }
    true
}

const fn terminal_sample_color(sample: u8, body: Rgb, detail: Rgb) -> Option<Rgb> {
    match sample {
        1 => Some(body),
        2 => Some(detail),
        _ => None,
    }
}

fn draw_terminal_coordinates(
    scene: &BoardScene,
    theme: BoardTheme,
    square: Square,
    area: TuiRect,
    view_column: u8,
    view_row: u8,
    buffer: &mut Buffer,
) {
    if !scene.show_coordinates || area.width < 3 || area.height < 3 {
        return;
    }
    let coordinate_color = theme.decorated_square_color(scene, square).shift(-72).tui();
    if view_column == 0 {
        buffer[(area.x, area.y)]
            .set_char(square.rank_char())
            .set_fg(coordinate_color);
    }
    if view_row == 7 {
        buffer[(area.x + area.width - 1, area.y + area.height - 1)]
            .set_char(square.file_char())
            .set_fg(coordinate_color);
    }
}

const fn piece_symbol(piece: ViewPiece) -> &'static str {
    // Filled glyphs retain their silhouettes at terminal font sizes. Ownership
    // is carried by the high-contrast foreground color instead of the much
    // thinner hollow Unicode variants.
    match piece.kind {
        PieceKind::Pawn => "♟",
        PieceKind::Knight => "♞",
        PieceKind::Bishop => "♝",
        PieceKind::Rook => "♜",
        PieceKind::Queen => "♛",
        PieceKind::King => "♚",
    }
}

/// Piece artwork used by the deterministic RGBA compositor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RasterPieceStyle {
    /// Antialiased silhouettes intended for pixel-capable terminal protocols.
    #[default]
    Detailed,
    /// Deliberately chunky two-tone sprites that survive half-block sampling.
    PixelArt,
}

/// Settings for the deterministic RGBA compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RasterOptions {
    /// Requested side, clamped and rounded down to a multiple of eight.
    pub side_px: u32,
    pub theme: BoardTheme,
    pub show_coordinates: bool,
    pub piece_style: RasterPieceStyle,
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            side_px: 768,
            theme: BoardTheme::default(),
            show_coordinates: true,
            piece_style: RasterPieceStyle::Detailed,
        }
    }
}

impl RasterOptions {
    #[must_use]
    pub fn new(side_px: u32) -> Self {
        Self {
            side_px,
            ..Self::default()
        }
    }

    #[must_use]
    pub const fn normalized_side_px(self) -> u32 {
        let clamped = if self.side_px < MIN_RASTER_SIDE {
            MIN_RASTER_SIDE
        } else if self.side_px > MAX_RASTER_SIDE {
            MAX_RASTER_SIDE
        } else {
            self.side_px
        };
        clamped - clamped % BOARD_LEN as u32
    }
}

/// Returns a square image side appropriate for a fitted cell board.
#[must_use]
pub fn raster_side_for_cells(layout: BoardLayout) -> u32 {
    let (square_width, square_height) = layout.square_pixels();
    square_width
        .min(square_height)
        .saturating_mul(BOARD_LEN as u32)
        .clamp(MIN_RASTER_SIDE, MAX_RASTER_SIDE)
}

/// Composes a complete square board into an opaque, deterministic RGBA image.
#[must_use]
pub fn compose_board_rgba(scene: &BoardScene, options: &RasterOptions) -> RgbaImage {
    compose_board_rgba_with_tweens(scene, options, &[])
}

/// Composes a board plus interpolated moving pieces into an RGBA image.
///
/// Each tween hides a matching piece at its destination in the already-updated
/// scene, then paints that piece between its source and destination. This keeps
/// captures, en passant, selection, check, and last-move decoration consistent
/// with the final position throughout the transition.
#[must_use]
pub fn compose_board_rgba_with_tweens(
    scene: &BoardScene,
    options: &RasterOptions,
    tweens: &[PieceTween],
) -> RgbaImage {
    let side = options.normalized_side_px();
    let square_px = side / BOARD_LEN as u32;
    let mut pixmap = Pixmap::new(side, side).expect("bounded raster dimensions are valid");
    pixmap.fill(options.theme.dark_square.skia(255));

    let mut base_scene = scene.clone();
    for tween in tweens {
        if base_scene.piece_at(tween.to) == Some(tween.piece) {
            base_scene.set_piece(tween.to, None);
        }
    }
    // Coordinates and the cursor are foreground decorations. Defer them until
    // after moving pieces so their normal visual priority is preserved.
    if !tweens.is_empty() {
        base_scene.cursor = None;
        base_scene.show_coordinates = false;
    }

    for view_row in 0..8_u8 {
        for view_column in 0..8_u8 {
            let square = base_scene
                .orientation
                .view_to_square(view_column, view_row)
                .expect("board display coordinates are valid");
            paint_raster_square(
                &mut pixmap,
                &base_scene,
                *options,
                square,
                u32::from(view_column) * square_px,
                u32::from(view_row) * square_px,
                square_px,
            );
        }
    }

    for tween in tweens {
        let (column, row) = tween.view_position(scene.orientation);
        draw_piece(
            &mut pixmap,
            column * square_px as f32,
            row * square_px as f32,
            square_px,
            tween.piece,
            options.theme,
            options.piece_style,
        );
    }

    if !tweens.is_empty() {
        paint_raster_foreground(&mut pixmap, scene, *options, square_px);
    }
    if options.piece_style == RasterPieceStyle::Detailed {
        draw_board_bevel(&mut pixmap, side);
    }
    RgbaImage::from_raw(side, side, pixmap.take()).expect("pixmap byte dimensions match the image")
}

fn paint_raster_foreground(
    pixmap: &mut Pixmap,
    scene: &BoardScene,
    options: RasterOptions,
    square_px: u32,
) {
    for view_row in 0..8_u8 {
        for view_column in 0..8_u8 {
            let square = scene
                .orientation
                .view_to_square(view_column, view_row)
                .expect("board display coordinates are valid");
            let x = u32::from(view_column) * square_px;
            let y = u32::from(view_row) * square_px;
            if scene.cursor == Some(square) {
                draw_square_outline(pixmap, x, y, square_px, options.theme.cursor, 0.045, 235);
            }
            if scene.show_coordinates && options.show_coordinates {
                draw_raster_coordinates(
                    pixmap,
                    square,
                    x,
                    y,
                    square_px,
                    view_column,
                    view_row,
                    options.theme.decorated_square_color(scene, square),
                );
            }
        }
    }
}

fn paint_raster_square(
    pixmap: &mut Pixmap,
    scene: &BoardScene,
    options: RasterOptions,
    square: Square,
    x: u32,
    y: u32,
    square_px: u32,
) {
    let theme = options.theme;
    let color = theme.decorated_square_color(scene, square);
    match options.piece_style {
        RasterPieceStyle::Detailed => {
            fill_gradient_rect(pixmap, x, y, square_px, color.shift(7), color.shift(-6));
        }
        RasterPieceStyle::PixelArt => fill_solid_rect(pixmap, x, y, square_px, color),
    }
    if let Some(target) = scene.target_at(square) {
        draw_target(pixmap, x, y, square_px, target, theme);
    }
    if scene.checked == Some(square) {
        draw_square_outline(pixmap, x, y, square_px, theme.check, 0.065, 210);
    }
    if let Some(piece) = scene.piece_at(square) {
        draw_piece(
            pixmap,
            x as f32,
            y as f32,
            square_px,
            piece,
            theme,
            options.piece_style,
        );
    }
    if scene.cursor == Some(square) {
        draw_square_outline(pixmap, x, y, square_px, theme.cursor, 0.045, 235);
    }
    if scene.show_coordinates && options.show_coordinates {
        let (view_column, view_row) = scene.orientation.square_to_view(square);
        draw_raster_coordinates(
            pixmap,
            square,
            x,
            y,
            square_px,
            view_column,
            view_row,
            color,
        );
    }
}

fn fill_solid_rect(pixmap: &mut Pixmap, x: u32, y: u32, size: u32, color: Rgb) {
    let Some(rect) = Rect::from_xywh(x as f32, y as f32, size as f32, size as f32) else {
        return;
    };
    pixmap.fill_rect(rect, &solid_paint(color, 255), Transform::identity(), None);
}

fn fill_gradient_rect(pixmap: &mut Pixmap, x: u32, y: u32, size: u32, top: Rgb, bottom: Rgb) {
    let Some(rect) = Rect::from_xywh(x as f32, y as f32, size as f32, size as f32) else {
        return;
    };
    let paint = Paint {
        anti_alias: false,
        shader: LinearGradient::new(
            Point::from_xy(x as f32, y as f32),
            Point::from_xy(x as f32, (y + size) as f32),
            vec![
                GradientStop::new(0.0, top.skia(255)),
                GradientStop::new(1.0, bottom.skia(255)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .expect("gradient endpoints differ"),
        ..Paint::default()
    };
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
}

fn draw_target(
    pixmap: &mut Pixmap,
    x: u32,
    y: u32,
    size: u32,
    target: TargetKind,
    theme: BoardTheme,
) {
    let center_x = x as f32 + size as f32 * 0.5;
    let center_y = y as f32 + size as f32 * 0.5;
    let mut builder = PathBuilder::new();
    builder.push_circle(
        center_x,
        center_y,
        size as f32
            * match target {
                TargetKind::Quiet => 0.115,
                TargetKind::Capture => 0.405,
            },
    );
    let Some(path) = builder.finish() else {
        return;
    };
    match target {
        TargetKind::Quiet => {
            let mut paint = solid_paint(theme.legal_target, 155);
            paint.force_hq_pipeline = true;
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        TargetKind::Capture => {
            let stroke = Stroke {
                width: (size as f32 * 0.065).max(1.0),
                ..Stroke::default()
            };
            pixmap.stroke_path(
                &path,
                &solid_paint(theme.capture_target, 175),
                &stroke,
                Transform::identity(),
                None,
            );
        }
    }
}

fn draw_square_outline(
    pixmap: &mut Pixmap,
    x: u32,
    y: u32,
    size: u32,
    color: Rgb,
    width_ratio: f32,
    alpha: u8,
) {
    let inset = size as f32 * width_ratio;
    let Some(rect) = Rect::from_xywh(
        x as f32 + inset,
        y as f32 + inset,
        size as f32 - inset * 2.0,
        size as f32 - inset * 2.0,
    ) else {
        return;
    };
    let mut builder = PathBuilder::new();
    builder.push_rect(rect);
    let Some(path) = builder.finish() else {
        return;
    };
    let stroke = Stroke {
        width: (size as f32 * width_ratio).max(1.0),
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(
        &path,
        &solid_paint(color, alpha),
        &stroke,
        Transform::identity(),
        None,
    );
}

fn draw_piece(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    size: u32,
    piece: ViewPiece,
    theme: BoardTheme,
    style: RasterPieceStyle,
) {
    if size < 4 {
        return;
    }
    if style == RasterPieceStyle::PixelArt && size >= LOW_RES_PIECE_GRID {
        draw_low_res_piece(pixmap, x, y, size, piece, theme);
        return;
    }
    let art = piece_art(piece.kind);
    let scale = size as f32 / 100.0;
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, x, y);
    let mut shadow = PathBuilder::new();
    shadow.push_oval(
        Rect::from_xywh(
            50.0 - art.base_half_width,
            91.0,
            art.base_half_width * 2.0,
            6.0,
        )
        .expect("piece shadow has positive dimensions"),
    );
    pixmap.fill_path(
        &shadow.finish().expect("piece shadow is non-empty"),
        &solid_paint(Rgb::new(9, 13, 17), 72),
        FillRule::Winding,
        transform,
        None,
    );

    let (top, bottom, outline, detail) = match piece.side {
        Side::White => (
            theme.white_piece.shift(7),
            theme.white_piece.shift(-13),
            Rgb::new(70, 75, 76),
            Rgb::new(104, 104, 98),
        ),
        Side::Black => (
            theme.black_piece.shift(19),
            theme.black_piece.shift(-8),
            Rgb::new(14, 17, 23),
            Rgb::new(139, 145, 146),
        ),
    };
    let fill = Paint {
        force_hq_pipeline: true,
        shader: LinearGradient::new(
            Point::from_xy(x + size as f32 * 0.22, y + size as f32 * 0.08),
            Point::from_xy(x + size as f32 * 0.76, y + size as f32 * 0.94),
            vec![
                GradientStop::new(0.0, top.skia(255)),
                GradientStop::new(0.62, bottom.shift(7).skia(255)),
                GradientStop::new(1.0, bottom.skia(255)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .expect("piece gradient endpoints differ"),
        ..Paint::default()
    };
    pixmap.fill_path(&art.body, &fill, FillRule::Winding, transform, None);

    for accent_path in &art.accents {
        pixmap.fill_path(
            accent_path,
            &solid_paint(detail, 225),
            FillRule::Winding,
            transform,
            None,
        );
    }

    let outline_stroke = Stroke {
        width: 1.6,
        line_join: LineJoin::Round,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(
        &art.body,
        &solid_paint(outline, 230),
        &outline_stroke,
        transform,
        None,
    );

    let detail_stroke = Stroke {
        width: 1.75,
        line_join: LineJoin::Round,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    for detail_path in &art.details {
        pixmap.stroke_path(
            detail_path,
            &solid_paint(detail, 180),
            &detail_stroke,
            transform,
            None,
        );
    }
}

/// Paint a chess piece for the half-block renderer's genuinely tiny output.
///
/// The normal piece art is intentionally detailed, but at six output pixels per
/// square its curves still alias into blobs. These sprites scale six deliberate
/// samples across the source square: `1` is the body and `2` is a contrasting
/// structural mark. Side gutters keep adjacent back-rank pieces from merging
/// into a bar; the extra samples preserve crowns, the bishop slash, the knight
/// muzzle, and the king's cross.
fn draw_low_res_piece(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    size: u32,
    piece: ViewPiece,
    theme: BoardTheme,
) {
    let scale = (size / LOW_RES_PIECE_GRID).max(1);
    let sprite_side = scale * LOW_RES_PIECE_GRID;
    let inset = (size - sprite_side) / 2;
    let (body, detail) = low_res_piece_colors(piece, theme);
    let sprite = low_res_piece_sprite(piece.kind);

    for (index, sample) in sprite.iter().copied().enumerate() {
        let color = match sample {
            1 => body,
            2 => detail,
            _ => continue,
        };
        let column = u32::try_from(index % LOW_RES_PIECE_GRID as usize)
            .expect("low-resolution sprite column fits in u32");
        let row = u32::try_from(index / LOW_RES_PIECE_GRID as usize)
            .expect("low-resolution sprite row fits in u32");
        let Some(rect) = Rect::from_xywh(
            x + (inset + column * scale) as f32,
            y + (inset + row * scale) as f32,
            scale as f32,
            scale as f32,
        ) else {
            continue;
        };
        pixmap.fill_rect(rect, &solid_paint(color, 255), Transform::identity(), None);
    }
}

fn low_res_piece_colors(piece: ViewPiece, theme: BoardTheme) -> (Rgb, Rgb) {
    match piece.side {
        Side::White => (theme.white_piece.shift(8), Rgb::new(55, 61, 64)),
        Side::Black => (theme.black_piece.shift(-10), Rgb::new(182, 188, 181)),
    }
}

/// Piece silhouettes at the terminal renderer's final 6x6 pixel resolution.
/// Zeroes are transparent; ones are body pixels; twos are high-contrast marks.
#[rustfmt::skip]
const fn low_res_piece_sprite(kind: PieceKind) -> &'static [u8; 36] {
    match kind {
        PieceKind::Pawn => &[
            0, 0, 1, 1, 0, 0,
            0, 1, 1, 1, 1, 0,
            0, 0, 1, 1, 0, 0,
            0, 0, 1, 1, 0, 0,
            0, 1, 1, 1, 1, 0,
            0, 2, 2, 2, 2, 0,
        ],
        PieceKind::Knight => &[
            0, 0, 1, 1, 0, 0,
            0, 1, 1, 1, 0, 0,
            0, 1, 2, 1, 1, 0,
            0, 0, 1, 1, 1, 1,
            0, 1, 1, 1, 0, 0,
            0, 2, 2, 2, 2, 0,
        ],
        PieceKind::Bishop => &[
            0, 0, 1, 1, 0, 0,
            0, 0, 2, 1, 0, 0,
            0, 1, 2, 1, 1, 0,
            0, 0, 1, 1, 0, 0,
            0, 1, 1, 1, 1, 0,
            0, 2, 2, 2, 2, 0,
        ],
        PieceKind::Rook => &[
            0, 1, 0, 0, 1, 0,
            0, 1, 1, 1, 1, 0,
            0, 0, 1, 1, 0, 0,
            0, 0, 1, 1, 0, 0,
            0, 1, 1, 1, 1, 0,
            0, 2, 2, 2, 2, 0,
        ],
        PieceKind::Queen => &[
            2, 0, 2, 0, 2, 0,
            0, 1, 1, 1, 1, 0,
            0, 0, 1, 1, 0, 0,
            0, 0, 1, 1, 0, 0,
            0, 1, 1, 1, 1, 0,
            0, 2, 2, 2, 2, 0,
        ],
        PieceKind::King => &[
            0, 0, 2, 0, 0, 0,
            0, 2, 2, 2, 0, 0,
            0, 0, 2, 0, 0, 0,
            0, 1, 1, 1, 1, 0,
            0, 1, 1, 1, 1, 0,
            0, 2, 2, 2, 2, 0,
        ],
    }
}

struct PieceArt {
    body: Path,
    details: Vec<Path>,
    accents: Vec<Path>,
    base_half_width: f32,
}

fn piece_art(kind: PieceKind) -> &'static PieceArt {
    static PIECES: OnceLock<[PieceArt; 6]> = OnceLock::new();
    let pieces = PIECES.get_or_init(|| {
        [
            pawn_art(),
            knight_art(),
            bishop_art(),
            rook_art(),
            queen_art(),
            king_art(),
        ]
    });
    &pieces[match kind {
        PieceKind::Pawn => 0,
        PieceKind::Knight => 1,
        PieceKind::Bishop => 2,
        PieceKind::Rook => 3,
        PieceKind::Queen => 4,
        PieceKind::King => 5,
    }]
}

fn pawn_art() -> PieceArt {
    let mut body = PathBuilder::new();
    body.push_circle(50.0, 29.5, 8.5);
    body.move_to(41.0, 37.0);
    body.line_to(59.0, 37.0);
    body.cubic_to(63.0, 37.5, 64.0, 41.0, 60.0, 44.5);
    body.line_to(40.0, 44.5);
    body.cubic_to(36.0, 41.0, 37.0, 37.5, 41.0, 37.0);
    body.close();

    body.move_to(43.0, 44.0);
    body.cubic_to(44.0, 51.0, 42.0, 57.0, 37.0, 63.0);
    body.cubic_to(34.0, 67.0, 32.0, 69.0, 31.0, 71.0);
    body.line_to(69.0, 71.0);
    body.cubic_to(68.0, 69.0, 66.0, 67.0, 63.0, 63.0);
    body.cubic_to(58.0, 57.0, 56.0, 51.0, 57.0, 44.0);
    body.close();
    finish_piece(
        body,
        vec![horizontal_detail(40.0, 44.0, 60.0, 44.0)],
        Vec::new(),
        24.0,
    )
}

fn knight_art() -> PieceArt {
    let mut body = PathBuilder::new();
    body.move_to(28.0, 72.0);
    body.cubic_to(30.0, 60.0, 36.0, 50.0, 44.0, 43.0);
    body.cubic_to(38.0, 40.0, 33.0, 35.0, 32.0, 29.0);
    body.cubic_to(31.0, 23.0, 35.0, 16.0, 39.0, 11.0);
    body.line_to(46.0, 23.0);
    body.cubic_to(50.0, 20.0, 52.0, 18.0, 54.0, 17.0);
    body.cubic_to(63.0, 20.0, 71.0, 27.0, 75.0, 34.0);
    body.line_to(78.0, 41.0);
    body.cubic_to(79.0, 45.0, 76.0, 48.0, 72.0, 49.0);
    body.line_to(63.0, 52.0);
    body.cubic_to(58.0, 53.0, 53.0, 50.0, 49.0, 46.0);
    body.cubic_to(51.0, 54.0, 58.0, 64.0, 64.0, 72.0);
    body.close();
    finish_piece(
        body,
        vec![
            curved_detail(&[(34.0, 26.0), (42.0, 31.0), (48.0, 42.0)]),
            curved_detail(&[(72.0, 43.0), (74.5, 42.5), (76.5, 41.0)]),
        ],
        vec![circle_path(59.0, 28.5, 1.8)],
        30.0,
    )
}

fn bishop_art() -> PieceArt {
    let mut body = PathBuilder::new();
    body.move_to(50.0, 7.0);
    body.cubic_to(61.0, 16.0, 66.0, 25.0, 63.0, 35.0);
    body.cubic_to(61.0, 42.0, 56.0, 47.0, 50.0, 51.0);
    body.cubic_to(44.0, 47.0, 39.0, 42.0, 37.0, 35.0);
    body.cubic_to(34.0, 25.0, 39.0, 16.0, 50.0, 7.0);
    body.close();

    body.move_to(38.0, 49.0);
    body.line_to(62.0, 49.0);
    body.cubic_to(66.0, 49.0, 67.0, 53.0, 64.0, 57.0);
    body.line_to(36.0, 57.0);
    body.cubic_to(33.0, 53.0, 34.0, 49.0, 38.0, 49.0);
    body.close();

    body.move_to(40.0, 57.0);
    body.line_to(60.0, 57.0);
    body.cubic_to(60.0, 63.0, 66.0, 68.0, 69.0, 71.0);
    body.line_to(31.0, 71.0);
    body.cubic_to(34.0, 68.0, 40.0, 63.0, 40.0, 57.0);
    body.close();

    let mut slot = PathBuilder::new();
    slot.move_to(54.5, 16.5);
    slot.line_to(58.0, 20.0);
    slot.line_to(46.0, 40.5);
    slot.line_to(42.8, 37.0);
    slot.close();
    finish_piece(
        body,
        vec![horizontal_detail(36.5, 57.0, 63.5, 57.0)],
        vec![slot.finish().expect("bishop slot is non-empty")],
        28.0,
    )
}

fn rook_art() -> PieceArt {
    let mut body = PathBuilder::new();
    body.move_to(25.0, 13.0);
    body.line_to(36.0, 13.0);
    body.line_to(36.0, 24.0);
    body.line_to(44.0, 24.0);
    body.line_to(44.0, 13.0);
    body.line_to(56.0, 13.0);
    body.line_to(56.0, 24.0);
    body.line_to(64.0, 24.0);
    body.line_to(64.0, 13.0);
    body.line_to(75.0, 13.0);
    body.line_to(72.0, 35.0);
    body.line_to(28.0, 35.0);
    body.close();

    body.move_to(28.0, 34.5);
    body.line_to(72.0, 34.5);
    body.cubic_to(76.0, 35.0, 77.0, 39.0, 73.0, 42.0);
    body.line_to(27.0, 42.0);
    body.cubic_to(23.0, 39.0, 24.0, 35.0, 28.0, 34.5);
    body.close();

    body.move_to(34.0, 41.0);
    body.line_to(66.0, 41.0);
    body.cubic_to(64.0, 50.0, 63.0, 60.0, 63.0, 70.0);
    body.line_to(37.0, 70.0);
    body.cubic_to(37.0, 60.0, 36.0, 50.0, 34.0, 41.0);
    body.close();
    finish_piece(
        body,
        vec![
            horizontal_detail(27.0, 41.5, 73.0, 41.5),
            horizontal_detail(37.0, 68.5, 63.0, 68.5),
        ],
        Vec::new(),
        30.0,
    )
}

fn queen_art() -> PieceArt {
    let mut body = PathBuilder::new();
    body.push_circle(22.0, 19.0, 3.4);
    body.push_circle(36.0, 10.5, 3.4);
    body.push_circle(50.0, 6.5, 3.6);
    body.push_circle(64.0, 10.5, 3.4);
    body.push_circle(78.0, 19.0, 3.4);
    body.move_to(22.0, 23.0);
    body.line_to(33.0, 40.0);
    body.line_to(36.0, 15.0);
    body.line_to(45.0, 38.0);
    body.line_to(50.0, 11.0);
    body.line_to(55.0, 38.0);
    body.line_to(64.0, 15.0);
    body.line_to(67.0, 40.0);
    body.line_to(78.0, 23.0);
    body.line_to(70.0, 49.0);
    body.cubic_to(64.0, 53.0, 36.0, 53.0, 30.0, 49.0);
    body.close();

    body.move_to(32.0, 48.0);
    body.line_to(68.0, 48.0);
    body.cubic_to(72.0, 49.0, 71.0, 54.0, 66.0, 56.0);
    body.line_to(34.0, 56.0);
    body.cubic_to(29.0, 54.0, 28.0, 49.0, 32.0, 48.0);
    body.close();

    body.move_to(40.0, 55.0);
    body.line_to(60.0, 55.0);
    body.cubic_to(60.0, 66.0, 65.0, 69.0, 68.0, 71.0);
    body.line_to(32.0, 71.0);
    body.cubic_to(35.0, 69.0, 40.0, 66.0, 40.0, 55.0);
    body.close();
    finish_piece(
        body,
        vec![
            curved_detail(&[(30.0, 48.5), (50.0, 53.5), (70.0, 48.5)]),
            horizontal_detail(34.0, 55.5, 66.0, 55.5),
        ],
        Vec::new(),
        31.0,
    )
}

fn king_art() -> PieceArt {
    let mut body = PathBuilder::new();
    body.move_to(46.0, 3.0);
    body.line_to(54.0, 3.0);
    body.line_to(54.0, 12.0);
    body.line_to(61.0, 12.0);
    body.line_to(61.0, 20.0);
    body.line_to(54.0, 20.0);
    body.line_to(54.0, 28.0);
    body.line_to(46.0, 28.0);
    body.line_to(46.0, 20.0);
    body.line_to(39.0, 20.0);
    body.line_to(39.0, 12.0);
    body.line_to(46.0, 12.0);
    body.close();

    body.move_to(46.0, 27.0);
    body.line_to(54.0, 27.0);
    body.line_to(54.0, 31.0);
    body.cubic_to(61.0, 33.0, 65.0, 38.0, 65.0, 43.0);
    body.cubic_to(65.0, 48.0, 62.0, 52.0, 58.0, 55.0);
    body.cubic_to(58.0, 61.0, 64.0, 67.0, 68.0, 71.0);
    body.line_to(32.0, 71.0);
    body.cubic_to(36.0, 67.0, 42.0, 61.0, 42.0, 55.0);
    body.cubic_to(38.0, 52.0, 35.0, 48.0, 35.0, 43.0);
    body.cubic_to(35.0, 38.0, 39.0, 33.0, 46.0, 31.0);
    body.close();
    finish_piece(
        body,
        vec![
            curved_detail(&[(36.0, 43.5), (50.0, 39.5), (64.0, 43.5)]),
            horizontal_detail(41.0, 55.0, 59.0, 55.0),
        ],
        Vec::new(),
        32.0,
    )
}

fn finish_piece(
    mut body: PathBuilder,
    mut details: Vec<Path>,
    accents: Vec<Path>,
    base_half_width: f32,
) -> PieceArt {
    append_base(&mut body, base_half_width);
    let collar = base_half_width - 7.0;
    details.push(horizontal_detail(49.0 - collar, 77.5, 51.0 + collar, 77.5));
    PieceArt {
        body: body.finish().expect("piece path is non-empty"),
        details,
        accents,
        base_half_width,
    }
}

fn append_base(builder: &mut PathBuilder, half_width: f32) {
    let collar = half_width - 7.0;
    let shoulder = half_width - 2.0;
    builder.move_to(50.0 - collar, 69.5);
    builder.line_to(50.0 + collar, 69.5);
    builder.cubic_to(
        53.0 + collar,
        69.5,
        55.0 + collar,
        73.0,
        52.0 + collar,
        77.0,
    );
    builder.line_to(50.0 + shoulder, 84.0);
    builder.cubic_to(
        49.0 + half_width,
        86.0,
        50.0 + half_width,
        88.0,
        50.0 + half_width,
        90.0,
    );
    builder.cubic_to(
        51.0 + half_width,
        92.0,
        49.0 + half_width,
        94.0,
        45.0 + half_width,
        94.0,
    );
    builder.line_to(55.0 - half_width, 94.0);
    builder.cubic_to(
        51.0 - half_width,
        94.0,
        49.0 - half_width,
        92.0,
        50.0 - half_width,
        90.0,
    );
    builder.cubic_to(
        50.0 - half_width,
        88.0,
        51.0 - half_width,
        86.0,
        50.0 - shoulder,
        84.0,
    );
    builder.line_to(48.0 - collar, 77.0);
    builder.cubic_to(
        45.0 - collar,
        73.0,
        47.0 - collar,
        69.5,
        50.0 - collar,
        69.5,
    );
    builder.close();
}

fn circle_path(x: f32, y: f32, radius: f32) -> Path {
    let mut builder = PathBuilder::new();
    builder.push_circle(x, y, radius);
    builder.finish().expect("circle path is non-empty")
}

fn horizontal_detail(x1: f32, y1: f32, x2: f32, y2: f32) -> Path {
    let mut builder = PathBuilder::new();
    builder.move_to(x1, y1);
    builder.line_to(x2, y2);
    builder.finish().expect("detail path is non-empty")
}

fn curved_detail(points: &[(f32, f32); 3]) -> Path {
    let mut builder = PathBuilder::new();
    builder.move_to(points[0].0, points[0].1);
    builder.quad_to(points[1].0, points[1].1, points[2].0, points[2].1);
    builder.finish().expect("detail path is non-empty")
}

#[allow(clippy::too_many_arguments)]
fn draw_raster_coordinates(
    pixmap: &mut Pixmap,
    square: Square,
    x: u32,
    y: u32,
    size: u32,
    view_column: u8,
    view_row: u8,
    square_color: Rgb,
) {
    if size < 12 {
        return;
    }
    let glyph_scale = (size / 34).clamp(1, 4);
    let ink = if luminance(square_color) > 145 {
        square_color.shift(-105)
    } else {
        square_color.shift(118)
    };
    if view_column == 0 {
        draw_bitmap_glyph(
            pixmap,
            square.rank_char(),
            x + glyph_scale * 2,
            y + glyph_scale * 2,
            glyph_scale,
            ink,
        );
    }
    if view_row == 7 {
        let glyph_width = glyph_scale * 3;
        let glyph_height = glyph_scale * 5;
        draw_bitmap_glyph(
            pixmap,
            square.file_char(),
            x + size.saturating_sub(glyph_width + glyph_scale * 2),
            y + size.saturating_sub(glyph_height + glyph_scale * 2),
            glyph_scale,
            ink,
        );
    }
}

const fn luminance(color: Rgb) -> u16 {
    (color.red as u16 * 54 + color.green as u16 * 183 + color.blue as u16 * 19) / 256
}

fn draw_bitmap_glyph(pixmap: &mut Pixmap, character: char, x: u32, y: u32, scale: u32, color: Rgb) {
    let rows = glyph_rows(character);
    let paint = solid_paint(color, 190);
    for (row_index, row) in rows.iter().copied().enumerate() {
        for column in 0..3_u32 {
            if row & (1_u8 << (2 - column)) == 0 {
                continue;
            }
            let Some(rect) = Rect::from_xywh(
                (x + column * scale) as f32,
                (y + u32::try_from(row_index).expect("glyph row fits") * scale) as f32,
                scale as f32,
                scale as f32,
            ) else {
                continue;
            };
            pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    }
}

const fn glyph_rows(character: char) -> [u8; 5] {
    match character {
        'a' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'b' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'c' => [0b011, 0b100, 0b100, 0b100, 0b011],
        'd' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'e' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'f' => [0b111, 0b100, 0b110, 0b100, 0b100],
        'g' => [0b011, 0b100, 0b101, 0b101, 0b011],
        'h' => [0b101, 0b101, 0b111, 0b101, 0b101],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b110, 0b001, 0b010, 0b100, 0b111],
        '3' => [0b110, 0b001, 0b010, 0b001, 0b110],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b110, 0b001, 0b110],
        '6' => [0b011, 0b100, 0b110, 0b101, 0b010],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b010, 0b101, 0b010, 0b101, 0b010],
        _ => [0; 5],
    }
}

fn draw_board_bevel(pixmap: &mut Pixmap, side: u32) {
    let Some(rect) = Rect::from_xywh(0.5, 0.5, side as f32 - 1.0, side as f32 - 1.0) else {
        return;
    };
    let mut builder = PathBuilder::new();
    builder.push_rect(rect);
    let Some(path) = builder.finish() else {
        return;
    };
    let stroke = Stroke {
        width: (side as f32 / 420.0).clamp(1.0, 3.0),
        ..Stroke::default()
    };
    pixmap.stroke_path(
        &path,
        &solid_paint(Rgb::new(18, 23, 24), 95),
        &stroke,
        Transform::identity(),
        None,
    );
}

fn solid_paint(color: Rgb, alpha: u8) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color.skia(alpha));
    paint
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test square is valid")
    }

    #[test]
    fn move_easing_is_clamped_symmetric_and_monotonic() {
        assert_eq!(ease_move_progress(-1.0), 0.0);
        assert_eq!(ease_move_progress(f32::NAN), 0.0);
        assert_eq!(ease_move_progress(0.0), 0.0);
        assert_eq!(ease_move_progress(0.5), 0.5);
        assert_eq!(ease_move_progress(1.0), 1.0);
        assert_eq!(ease_move_progress(2.0), 1.0);

        let samples: Vec<_> = (0..=20)
            .map(|step| ease_move_progress(step as f32 / 20.0))
            .collect();
        assert!(samples.windows(2).all(|pair| pair[0] <= pair[1]));
        for step in 0..=20 {
            let forward = ease_move_progress(step as f32 / 20.0);
            let reverse = ease_move_progress((20 - step) as f32 / 20.0);
            assert!((forward - (1.0 - reverse)).abs() < 0.000_01);
        }
    }

    #[test]
    fn tween_position_respects_orientation() {
        let tween = PieceTween::new(
            ViewPiece::new(Side::White, PieceKind::Pawn),
            square(4, 1),
            square(4, 3),
            0.5,
        );
        assert_eq!(tween.view_position(Orientation::White), (4.0, 5.0));
        assert_eq!(tween.view_position(Orientation::Black), (3.0, 2.0));
    }

    #[test]
    fn completed_tween_matches_the_static_destination_frame() {
        let mut scene = BoardScene::empty();
        scene.show_coordinates = false;
        let piece = ViewPiece::new(Side::White, PieceKind::Knight);
        let from = square(1, 0);
        let to = square(2, 2);
        scene.set_piece(to, Some(piece));
        let options = RasterOptions::new(128);

        let static_frame = compose_board_rgba(&scene, &options);
        let tweened_frame = compose_board_rgba_with_tweens(
            &scene,
            &options,
            &[PieceTween::new(piece, from, to, 1.0)],
        );

        assert_eq!(static_frame, tweened_frame);
    }

    #[test]
    fn detailed_piece_geometry_keeps_the_staunton_hierarchy() {
        let pawn = piece_art(PieceKind::Pawn);
        let knight = piece_art(PieceKind::Knight);
        let bishop = piece_art(PieceKind::Bishop);
        let rook = piece_art(PieceKind::Rook);
        let queen = piece_art(PieceKind::Queen);
        let king = piece_art(PieceKind::King);

        assert!(
            pawn.body.bounds().top() >= 20.0,
            "pawns must remain visibly shorter than the back rank"
        );
        assert!(pawn.body.bounds().top() > rook.body.bounds().top() + 5.0);
        assert!(bishop.body.bounds().top() < rook.body.bounds().top());
        assert!(queen.body.bounds().top() <= king.body.bounds().top());
        assert_eq!(
            [
                pawn.base_half_width,
                bishop.base_half_width,
                knight.base_half_width,
                rook.base_half_width,
                queen.base_half_width,
                king.base_half_width,
            ],
            [24.0, 28.0, 30.0, 30.0, 31.0, 32.0]
        );
        assert!(!knight.accents.is_empty());
        assert!(!bishop.accents.is_empty());
    }
}
