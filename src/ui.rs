//! Responsive Ratatui layout and widgets.

use std::{
    env,
    fmt::Write as FmtWrite,
    io::Write as IoWrite,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use image::{DynamicImage, imageops::FilterType};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect, Size},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Padding, Paragraph, Wrap},
};
use ratatui_image::{
    FontSize, Resize, ResizeEncodeRender,
    picker::{Picker, ProtocolType},
    protocol::{StatefulProtocol, StatefulProtocolType},
};

use crate::{
    app::{App, EngineSetupState},
    chess::{Color as SideColor, Piece, Square as ChessSquare},
    render::{
        BoardLayout, BoardScene, BoardTheme, BoardWidget, CellSize, LastMove, PieceKind,
        PieceTween, RasterOptions, RasterPieceStyle, Square as ViewSquare, TargetKind, ViewPiece,
        compose_board_rgba_with_tweens, ease_move_progress,
    },
};

const BACKGROUND: Color = Color::Rgb(12, 16, 16);
const PANEL: Color = Color::Rgb(19, 25, 24);
const BORDER: Color = Color::Rgb(55, 72, 66);
const TEXT: Color = Color::Rgb(231, 234, 224);
const MUTED: Color = Color::Rgb(137, 151, 143);
const ACCENT: Color = Color::Rgb(238, 194, 92);
const CYAN: Color = Color::Rgb(102, 214, 226);
const DANGER: Color = Color::Rgb(239, 105, 99);
const SIDE_RAIL_MIN_TERMINAL_WIDTH: u16 = 69;
const SIDE_RAIL_MIN_TERMINAL_HEIGHT: u16 = 24;
const MOVE_ANIMATION_DURATION: Duration = Duration::from_millis(160);
const MOVE_ANIMATION_FRAMES: u8 = 6;
const KITTY_MAX_ENCODE_DIMENSION: u32 = 768;
const KITTY_PLACEHOLDER_REFRESH: &str = "\x1b[s\x1b[u";

/// Mutable view state kept outside the deterministic application reducer.
pub struct UiState {
    board_layout: BoardLayout,
    cell_size: CellSize,
    raster: Option<RasterBoard>,
    overlay_visible: bool,
}

impl UiState {
    #[must_use]
    pub fn new(picker: Option<Picker>) -> Self {
        Self::with_animation(picker, true)
    }

    /// Construct UI state and explicitly enable or disable raster move
    /// transitions. Unicode rendering is always immediate because it has no
    /// raster backend.
    #[must_use]
    pub fn with_animation(picker: Option<Picker>, animations_enabled: bool) -> Self {
        let cell_size = picker.as_ref().map_or_else(CellSize::default, |picker| {
            let font = picker.font_size();
            CellSize::new(font.width, font.height)
        });
        Self {
            board_layout: BoardLayout::fit(Rect::default(), cell_size),
            cell_size,
            raster: picker.map(|picker| RasterBoard::new(picker, animations_enabled)),
            overlay_visible: false,
        }
    }

    #[must_use]
    pub fn unicode() -> Self {
        Self::new(None)
    }

    /// Resolve an absolute terminal-cell coordinate into a game square.
    #[must_use]
    pub fn square_at(
        &self,
        x: u16,
        y: u16,
        orientation: crate::render::Orientation,
    ) -> Option<ChessSquare> {
        self.board_layout
            .square_at(x, y, orientation)
            .map(Into::into)
    }

    #[must_use]
    pub const fn board_layout(&self) -> BoardLayout {
        self.board_layout
    }

    /// Incorporate completed background image encodes. Returns `true` when a
    /// fresh protocol frame became available and the terminal should redraw.
    pub fn poll(&mut self) -> bool {
        let inline_overlay_visible = self.overlay_visible
            && self
                .raster
                .as_ref()
                .is_some_and(|raster| raster.protocol != ProtocolType::Halfblocks);
        self.raster
            .as_mut()
            .is_some_and(|raster| raster.poll(!inline_overlay_visible))
    }

    fn raster_notice(&self) -> Option<&str> {
        self.raster.as_ref().and_then(|raster| {
            raster.disabled_reason.as_deref().or_else(|| {
                raster
                    .density_limited
                    .then_some("unicode · enlarge for pixel art")
            })
        })
    }

    fn graphics_status(&self, negotiated_label: &str) -> String {
        self.raster.as_ref().map_or_else(
            || negotiated_label.to_owned(),
            |raster| raster.status_label(negotiated_label),
        )
    }
}

/// Draw the complete application and update hit-testing state.
pub fn render(frame: &mut Frame, app: &App, state: &mut UiState, graphics_label: &str) {
    let area = frame.area();
    frame.render_widget(Block::new().bg(BACKGROUND), area);

    let overlay_visible = app.help_visible()
        || app.pending_promotion().is_some()
        || app.engine_setup_state().is_some();
    let restore_after_overlay = state.overlay_visible && !overlay_visible;
    let scene = scene_from_app(app);
    if area.width >= SIDE_RAIL_MIN_TERMINAL_WIDTH && area.height >= SIDE_RAIL_MIN_TERMINAL_HEIGHT {
        let rail_width = area.width.saturating_sub(49).clamp(20, 32);
        let [board, rail] =
            Layout::horizontal([Constraint::Min(48), Constraint::Length(rail_width)])
                .spacing(1)
                .areas(area);
        draw_board(
            frame,
            app,
            state,
            &scene,
            board,
            overlay_visible,
            restore_after_overlay,
        );
        draw_side_rail(frame, app, state, rail, graphics_label);
    } else {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .areas(area);
        draw_board(
            frame,
            app,
            state,
            &scene,
            body,
            overlay_visible,
            restore_after_overlay,
        );
        draw_compact_header(frame, app, state, header, graphics_label);
        draw_compact_footer(frame, app, state, footer);
    }

    if app.help_visible() {
        draw_help(frame, app, area);
    }
    if app.pending_promotion().is_some() {
        draw_promotion(frame, area);
    }
    if let Some(setup) = app.engine_setup_state() {
        draw_engine_setup(frame, setup, area);
    }
    state.overlay_visible = overlay_visible;
}

fn draw_compact_header(
    frame: &mut Frame,
    app: &App,
    state: &UiState,
    area: Rect,
    graphics_label: &str,
) {
    let [identity, meta] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
    let status = status_headline(app);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" CHESS", Style::new().fg(ACCENT).bold()),
            Span::styled(format!("  {status}"), status_style(app)),
        ]))
        .style(Style::new().bg(PANEL)),
        identity,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("move {}  ", app.game().fullmove_number()),
                Style::new().fg(MUTED),
            ),
            Span::styled(
                state.graphics_status(graphics_label),
                Style::new().fg(MUTED),
            ),
            Span::raw(" "),
        ]))
        .alignment(Alignment::Right)
        .style(Style::new().bg(PANEL)),
        meta,
    );
}

fn draw_compact_footer(frame: &mut Frame, app: &App, state: &UiState, area: Rect) {
    let controls = if let Some(notice) = state.raster_notice() {
        notice.to_owned()
    } else if area.width >= 68 {
        format!(
            "enter select   u {}   ? help   q quit",
            if app.is_engine_game() { "turn" } else { "undo" }
        )
    } else {
        "enter select   ? help   q quit".to_owned()
    };
    let [message, keys] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(interaction_message(app), Style::new().fg(TEXT)),
        ]))
        .style(Style::new().bg(PANEL)),
        message,
    );
    frame.render_widget(
        Paragraph::new(controls)
            .fg(MUTED)
            .alignment(Alignment::Right)
            .style(Style::new().bg(PANEL)),
        keys,
    );
}

fn draw_board(
    frame: &mut Frame,
    app: &App,
    state: &mut UiState,
    scene: &BoardScene,
    area: Rect,
    overlay_visible: bool,
    restore_after_overlay: bool,
) {
    let board_shell = Block::new().style(Style::new().bg(BACKGROUND));
    let inner = board_shell.inner(area);
    frame.render_widget(board_shell, area);

    state.board_layout = BoardLayout::fit(inner, state.cell_size);
    if state.board_layout.is_empty() {
        frame.render_widget(
            Paragraph::new("Terminal too small for the board")
                .fg(MUTED)
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    // Always draw the cell renderer first. It is the visible fallback while an
    // image is being encoded and remains the semantic representation for tests.
    frame.render_widget(
        BoardWidget::new(scene).cell_size(state.cell_size),
        state.board_layout.board,
    );

    // Inline-image protocols are not composited by Ratatui. If an image becomes
    // ready after a cell-based modal was already drawn, the terminal can repaint
    // it above modal cells that Ratatui considers unchanged. Render the
    // deterministic cell board while any modal is open, then let the existing
    // post-overlay refresh restore the raster when it closes. Halfblocks are
    // ordinary terminal cells, so Ratatui can safely composite them as usual.
    if overlay_visible
        && state
            .raster
            .as_ref()
            .is_some_and(|raster| raster.protocol != ProtocolType::Halfblocks)
    {
        return;
    }

    if let Some(raster) = &mut state.raster {
        raster.render(
            frame,
            scene,
            state.board_layout,
            app.game().ply_count(),
            state.board_layout.board,
            restore_after_overlay,
        );
    }
}

fn draw_side_rail(frame: &mut Frame, app: &App, state: &UiState, area: Rect, graphics_label: &str) {
    let block = Block::new()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(BORDER))
        .padding(Padding::horizontal(1))
        .style(Style::new().bg(PANEL));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [summary, message, history_title, moves, controls] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(6),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::styled("CHESS", Style::new().fg(ACCENT).bold())]),
            Line::from(
                app.engine_name()
                    .map_or_else(String::new, |name| format!("vs {name}")),
            ),
            Line::from(Span::styled(status_headline(app), status_style(app))),
            Line::from(vec![Span::styled(
                format!("move {}", app.game().fullmove_number()),
                Style::new().fg(MUTED),
            )]),
        ]),
        summary,
    );
    frame.render_widget(
        Paragraph::new(interaction_message(app))
            .fg(TEXT)
            .wrap(Wrap { trim: true }),
        message,
    );
    frame.render_widget(
        Paragraph::new("MOVES").style(Style::new().fg(MUTED).bold()),
        history_title,
    );

    let history = app.game().history();
    if history.is_empty() {
        frame.render_widget(Paragraph::new("No moves yet").fg(MUTED), moves);
    } else {
        let items: Vec<_> = history
            .chunks(2)
            .enumerate()
            .rev()
            .map(|(index, pair)| {
                let white = pair.first().map_or("", |record| record.san.as_str());
                let black = pair.get(1).map_or("", |record| record.san.as_str());
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>2}  ", index + 1), Style::new().fg(MUTED)),
                    Span::styled(format!("{white:<8}"), Style::new().fg(TEXT)),
                    Span::styled(black, Style::new().fg(TEXT)),
                ]))
            })
            .collect();
        frame.render_widget(List::new(items), moves);
    }

    let graphics_notice = state.graphics_status(graphics_label);
    let commands = if inner.width >= 20 {
        vec![
            command_line(
                "enter",
                "select",
                "u",
                if app.is_engine_game() { "turn" } else { "undo" },
            ),
            command_line("f", "flip", "n", "new"),
            command_line("?", "help", "q", "quit"),
            Line::from(""),
            Line::from(Span::styled(
                graphics_notice.clone(),
                Style::new().fg(MUTED),
            )),
        ]
    } else {
        vec![
            key_label("enter", "select"),
            command_line(
                "u",
                if app.is_engine_game() { "turn" } else { "undo" },
                "f",
                "flip",
            ),
            command_line("n", "new", "?", "help"),
            key_label("q", "quit"),
            Line::from(""),
            Line::from(Span::styled(graphics_notice, Style::new().fg(MUTED))),
        ]
    };
    frame.render_widget(Paragraph::new(commands), controls);
}

fn command_line<'a>(
    first_key: &'a str,
    first_label: &'a str,
    second_key: &'a str,
    second_label: &'a str,
) -> Line<'a> {
    Line::from(vec![
        Span::styled(first_key, Style::new().fg(ACCENT).bold()),
        Span::styled(format!(" {first_label:<7}"), Style::new().fg(MUTED)),
        Span::styled(second_key, Style::new().fg(ACCENT).bold()),
        Span::styled(format!(" {second_label}"), Style::new().fg(MUTED)),
    ])
}

fn key_label<'a>(key: &'a str, label: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(key, Style::new().fg(ACCENT).bold()),
        Span::styled(format!(" {label}"), Style::new().fg(MUTED)),
    ])
}

fn status_headline(app: &App) -> String {
    if let Some(setup) = app.engine_setup_state() {
        return match setup {
            EngineSetupState::InstallPrompt => "MAIA SETUP".to_owned(),
            EngineSetupState::Installing { .. } => "INSTALLING MAIA".to_owned(),
            EngineSetupState::Failed { .. } => "SETUP FAILED".to_owned(),
        };
    }
    if app.game().is_game_over() {
        return app.game().status().to_string().to_uppercase();
    }
    if app.engine_failure().is_some() {
        return "ENGINE FAILED".to_owned();
    }
    if app.is_engine_game() {
        if app.game().in_check() {
            return if app.is_engine_turn() {
                "ENGINE IN CHECK".to_owned()
            } else {
                "YOU ARE IN CHECK".to_owned()
            };
        }
        return if app.is_engine_turn() {
            "ENGINE THINKING".to_owned()
        } else {
            "YOUR MOVE".to_owned()
        };
    }
    let turn = match app.game().side_to_move() {
        SideColor::White => "WHITE",
        SideColor::Black => "BLACK",
    };
    if app.game().in_check() {
        format!("{turn} IN CHECK")
    } else {
        format!("{turn} TO MOVE")
    }
}

fn interaction_message(app: &App) -> &str {
    match app.message() {
        "White to move" | "Black to move" => "Select a piece",
        message => message,
    }
}

fn status_style(app: &App) -> Style {
    if app.game().is_checkmate()
        || app.game().in_check()
        || app.engine_failure().is_some()
        || matches!(
            app.engine_setup_state(),
            Some(EngineSetupState::Failed { .. })
        )
    {
        Style::new().fg(DANGER).bold()
    } else {
        Style::new().fg(CYAN)
    }
}

fn scene_from_app(app: &App) -> BoardScene {
    let mut scene = BoardScene::empty();
    scene.orientation = app.orientation();
    scene.cursor = Some(app.cursor().into());
    scene.selected = app.selected().map(Into::into);
    scene.show_coordinates = app.coordinates_visible();

    for (square, piece) in app.game().pieces() {
        scene.set_piece(
            square.into(),
            Some(ViewPiece::from_cozy(piece.piece, piece.color)),
        );
    }
    for mv in app.legal_moves() {
        let capture = app.game().piece_at(mv.to).is_some()
            || (app
                .game()
                .piece_at(mv.from)
                .is_some_and(|piece| piece.piece == Piece::Pawn)
                && mv.from.file() != mv.to.file());
        scene.set_target(
            mv.to.into(),
            Some(if capture {
                TargetKind::Capture
            } else {
                TargetKind::Quiet
            }),
        );
    }
    if let Some(last) = app.game().last_move() {
        scene.last_move = Some(LastMove::new(last.mv.from.into(), last.mv.to.into()));
    }
    if app.game().in_check() {
        scene.checked = Some(app.game().king_square(app.game().side_to_move()).into());
    }
    scene
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(area, 64, 22);
    frame.render_widget(Clear, popup);
    let help = vec![
        Line::from(Span::styled("PLAY", Style::new().fg(ACCENT).bold())),
        key_line("Arrows / h j k l", "Move the board cursor"),
        key_line("Enter / Space", "Select a piece or destination"),
        key_line("Mouse left", "Select a square"),
        key_line("Esc / Mouse right", "Cancel selection or close a dialog"),
        Line::from(""),
        Line::from(Span::styled("GAME", Style::new().fg(ACCENT).bold())),
        key_line(
            "u",
            if app.is_engine_game() {
                "Undo the previous turn"
            } else {
                "Undo one move"
            },
        ),
        key_line("f", "Flip the board"),
        key_line("c", "Toggle coordinates"),
        key_line("n", "Start a new game"),
        key_line("q / Ctrl-C", "Quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Press ? or Esc to return",
            Style::new().fg(CYAN),
        )),
    ];
    frame.render_widget(
        Paragraph::new(help)
            .block(
                Block::new()
                    .title(Line::from(" CONTROLS ").fg(ACCENT).bold())
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(CYAN))
                    .style(Style::new().bg(PANEL)),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_promotion(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(area, 48, 9);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "Choose the piece your pawn becomes",
                Style::new().fg(TEXT),
            ))
            .centered(),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Q ", Style::new().fg(BACKGROUND).bg(ACCENT).bold()),
                Span::raw(" Queen    "),
                Span::styled(" R ", Style::new().fg(BACKGROUND).bg(CYAN).bold()),
                Span::raw(" Rook    "),
                Span::styled(" B ", Style::new().fg(BACKGROUND).bg(CYAN).bold()),
                Span::raw(" Bishop    "),
                Span::styled(" N ", Style::new().fg(BACKGROUND).bg(CYAN).bold()),
                Span::raw(" Knight"),
            ])
            .centered(),
        ])
        .block(
            Block::new()
                .title(Line::from(" PROMOTION ").fg(ACCENT).bold())
                .borders(Borders::ALL)
                .border_style(Style::new().fg(ACCENT))
                .style(Style::new().bg(PANEL)),
        ),
        popup,
    );
}

fn draw_engine_setup(frame: &mut Frame, setup: &EngineSetupState, area: Rect) {
    let (title, border, height, content) = match setup {
        EngineSetupState::InstallPrompt => (
            " MAIA ENGINE SETUP ",
            CYAN,
            16,
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Play against a human-like opponent",
                    Style::new().fg(ACCENT).bold(),
                ))
                .centered(),
                Line::from(""),
                Line::from("Maia needs a one-time, isolated setup on this computer.").centered(),
                Line::from("It will be kept in chess-tui's private data directory.").centered(),
                Line::from(""),
                Line::from(Span::styled(
                    "Python and PyTorch can make the first download large.",
                    Style::new().fg(TEXT),
                ))
                .centered(),
                Line::from("Setup may take several minutes.").centered(),
                Line::from(""),
                setup_action_line(" Enter ", "Install Maia", " q ", "Quit"),
            ],
        ),
        EngineSetupState::Installing { step } => (
            " INSTALLING MAIA ",
            CYAN,
            13,
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Setting up your opponent",
                    Style::new().fg(ACCENT).bold(),
                ))
                .centered(),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  ◆  ", Style::new().fg(CYAN).bold()),
                    Span::styled(step, Style::new().fg(TEXT)),
                ])
                .centered(),
                Line::from(""),
                Line::from("This is a one-time setup. Keep this terminal open.").centered(),
                Line::from(""),
                setup_quit_line(),
            ],
        ),
        EngineSetupState::Failed { message } => (
            " MAIA SETUP FAILED ",
            DANGER,
            15,
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Maia could not be installed",
                    Style::new().fg(DANGER).bold(),
                ))
                .centered(),
                Line::from(""),
                Line::from(Span::styled(message, Style::new().fg(TEXT))).centered(),
                Line::from(""),
                Line::from("Nothing outside chess-tui's private setup was changed.")
                    .fg(MUTED)
                    .centered(),
                Line::from(""),
                setup_action_line(" Enter / r ", "Retry", " q ", "Quit"),
            ],
        ),
    };

    let popup = centered_rect(area, 68, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::new()
                    .title(Line::from(title).fg(ACCENT).bold())
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(border))
                    .padding(Padding::horizontal(1))
                    .style(Style::new().bg(PANEL)),
            )
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn setup_action_line<'a>(
    primary_key: &'a str,
    primary_label: &'a str,
    secondary_key: &'a str,
    secondary_label: &'a str,
) -> Line<'a> {
    Line::from(vec![
        Span::styled(primary_key, Style::new().fg(BACKGROUND).bg(ACCENT).bold()),
        Span::styled(format!(" {primary_label}    "), Style::new().fg(TEXT)),
        Span::styled(secondary_key, Style::new().fg(BACKGROUND).bg(CYAN).bold()),
        Span::styled(format!(" {secondary_label}"), Style::new().fg(TEXT)),
    ])
    .centered()
}

fn setup_quit_line<'a>() -> Line<'a> {
    Line::from(vec![
        Span::styled(" q ", Style::new().fg(BACKGROUND).bg(CYAN).bold()),
        Span::styled(" Quit", Style::new().fg(TEXT)),
    ])
    .centered()
}

fn key_line<'a>(key: &'a str, description: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {key:<20}"), Style::new().fg(CYAN).bold()),
        Span::styled(description, Style::new().fg(TEXT)),
    ])
}

fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = max_width.min(area.width.saturating_sub(2)).max(1);
    let height = max_height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderKey {
    scene_revision: u64,
    position_revision: u64,
    area: Size,
    side_px: u32,
    animation_frame: Option<u8>,
}

impl RenderKey {
    fn accepts(self, candidate: Self) -> bool {
        if self.scene_revision != candidate.scene_revision
            || self.position_revision != candidate.position_revision
            || self.area != candidate.area
            || self.side_px != candidate.side_px
        {
            return false;
        }
        match (self.animation_frame, candidate.animation_frame) {
            (None, None) => true,
            (Some(desired), Some(completed)) => completed <= desired,
            _ => false,
        }
    }

    fn can_display(self, candidate: Self, animation_active: bool) -> bool {
        if self.area != candidate.area || self.side_px != candidate.side_px {
            return false;
        }
        if self.accepts(candidate) || self.position_revision == candidate.position_revision {
            return true;
        }
        animation_active && candidate.position_revision.wrapping_add(1) == self.position_revision
    }
}

struct EncodeRequest {
    scene: BoardScene,
    options: RasterOptions,
    tweens: Vec<PieceTween>,
    encode_font: FontSize,
    key: RenderKey,
    generation: u64,
}

struct EncodeResponse {
    frame: Result<EncodedRasterFrame, String>,
    key: RenderKey,
    generation: u64,
}

struct EncodedRasterFrame {
    protocol: StatefulProtocol,
    kitty_transmit: Option<String>,
}

#[derive(Default)]
struct EncoderState {
    request: Option<EncodeRequest>,
    response: Option<EncodeResponse>,
    shutdown: bool,
}

#[derive(Default)]
struct EncoderMailbox {
    state: Mutex<EncoderState>,
    wake: Condvar,
}

impl EncoderMailbox {
    fn submit(&self, request: EncodeRequest) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.shutdown {
            // A single replaceable slot gives us the newest board frame without
            // an unbounded queue of obsolete animation encodes.
            state.request = Some(request);
            self.wake.notify_one();
        }
    }

    fn wait_for_request(&self) -> Option<EncodeRequest> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while state.request.is_none() && !state.shutdown {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        if state.shutdown {
            None
        } else {
            state.request.take()
        }
    }

    fn complete(&self, response: EncodeResponse) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.shutdown {
            // The UI only needs the newest completed frame as well.
            state.response = Some(response);
        }
    }

    fn take_response(&self) -> Option<EncodeResponse> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .response
            .take()
    }

    fn discard_pending(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .request = None;
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.shutdown = true;
        state.request = None;
        state.response = None;
        self.wake.notify_one();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AnimationLeg {
    piece: ViewPiece,
    from: ViewSquare,
    to: ViewSquare,
}

struct MoveAnimation {
    started: Instant,
    legs: Vec<AnimationLeg>,
}

impl MoveAnimation {
    fn frame_at(&self, now: Instant) -> Option<(u8, Vec<PieceTween>)> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= MOVE_ANIMATION_DURATION {
            return None;
        }
        let elapsed_units = elapsed
            .as_nanos()
            .saturating_mul(u128::from(MOVE_ANIMATION_FRAMES));
        let frame = u8::try_from(elapsed_units / MOVE_ANIMATION_DURATION.as_nanos())
            .unwrap_or(MOVE_ANIMATION_FRAMES - 1)
            .min(MOVE_ANIMATION_FRAMES - 1);
        let progress = ease_move_progress(f32::from(frame) / f32::from(MOVE_ANIMATION_FRAMES));
        let tweens = self
            .legs
            .iter()
            .map(|leg| PieceTween::new(leg.piece, leg.from, leg.to, progress))
            .collect();
        Some((frame, tweens))
    }
}

struct DisplayedFrame {
    frame: EncodedRasterFrame,
    key: RenderKey,
    generation: u64,
}

struct RasterBoard {
    encoder: Arc<EncoderMailbox>,
    worker: Option<JoinHandle<()>>,
    displayed: Option<DisplayedFrame>,
    desired_key: Option<RenderKey>,
    next_generation: u64,
    previous_scene: Option<BoardScene>,
    scene_revision: u64,
    position_revision: u64,
    previous_ply_count: usize,
    animation: Option<MoveAnimation>,
    animations_enabled: bool,
    protocol: ProtocolType,
    piece_style: RasterPieceStyle,
    density_limited: bool,
    display_ready: bool,
    placeholder_refresh_pending: bool,
    disabled_reason: Option<String>,
}

fn piece_style_for_protocol(protocol: ProtocolType) -> RasterPieceStyle {
    if protocol == ProtocolType::Halfblocks {
        RasterPieceStyle::PixelArt
    } else {
        RasterPieceStyle::Detailed
    }
}

impl RasterBoard {
    fn new(picker: Picker, animations_enabled: bool) -> Self {
        let protocol = picker.protocol_type();
        let piece_style = piece_style_for_protocol(protocol);
        let compressed_kitty = protocol == ProtocolType::Kitty && direct_kitty_transport();
        let encoder = Arc::new(EncoderMailbox::default());
        let worker_mailbox = Arc::clone(&encoder);
        let worker = thread::Builder::new()
            .name("chess-image-encoder".to_owned())
            .spawn(move || {
                let mut reusable_protocol: Option<StatefulProtocolType> = None;
                while let Some(request) = worker_mailbox.wait_for_request() {
                    let frame = match prepare_raster_image(
                        &request.scene,
                        request.options,
                        &request.tweens,
                        request.key.area,
                        request.encode_font,
                    ) {
                        Ok(image) => {
                            let protocol_type = reusable_protocol.take().unwrap_or_else(|| {
                                picker
                                    .new_resize_protocol(DynamicImage::new_rgba8(1, 1))
                                    .protocol_type()
                                    .clone()
                            });
                            encode_raster_frame(
                                image,
                                request.encode_font,
                                request.key.area,
                                protocol_type,
                                compressed_kitty,
                            )
                        }
                        Err(error) => Err(error),
                    };
                    if let Ok(frame) = &frame {
                        reusable_protocol = Some(frame.protocol.protocol_type().clone());
                    }
                    worker_mailbox.complete(EncodeResponse {
                        frame,
                        key: request.key,
                        generation: request.generation,
                    });
                }
            })
            .expect("image encoder worker should start");
        Self {
            encoder,
            worker: Some(worker),
            displayed: None,
            desired_key: None,
            next_generation: 0,
            previous_scene: None,
            scene_revision: 0,
            position_revision: 0,
            previous_ply_count: 0,
            animation: None,
            animations_enabled,
            protocol,
            piece_style,
            density_limited: false,
            display_ready: false,
            placeholder_refresh_pending: false,
            disabled_reason: None,
        }
    }

    fn status_label(&self, negotiated_label: &str) -> String {
        if self.disabled_reason.is_some() {
            "cell fallback · encoder error".to_owned()
        } else if self.density_limited {
            "cell fallback · enlarge".to_owned()
        } else if self.display_ready {
            format!("{negotiated_label} · ready")
        } else {
            format!("{negotiated_label} · preparing")
        }
    }

    fn render(
        &mut self,
        frame: &mut Frame,
        scene: &BoardScene,
        layout: BoardLayout,
        ply_count: usize,
        area: Rect,
        restore_after_overlay: bool,
    ) {
        self.poll(true);
        self.display_ready = false;
        self.placeholder_refresh_pending |= restore_after_overlay;
        let now = Instant::now();
        self.observe_scene(scene, ply_count, now);

        self.density_limited =
            self.piece_style == RasterPieceStyle::PixelArt && !pixel_art_density_sufficient(layout);
        if self.disabled_reason.is_some() || self.density_limited {
            self.desired_key = None;
            self.encoder.discard_pending();
            return;
        }

        let animation = self
            .animation
            .as_ref()
            .and_then(|animation| animation.frame_at(now));
        if self.animation.is_some() && animation.is_none() {
            self.animation = None;
        }
        let (animation_frame, tweens) = animation.map_or_else(
            || (None, Vec::new()),
            |(frame, tweens)| (Some(frame), tweens),
        );
        let animation_active = animation_frame.is_some();
        let encoding = raster_encoding_plan(self.protocol, layout);
        let side_px = encoding.side_px;
        let key = RenderKey {
            scene_revision: self.scene_revision,
            position_revision: self.position_revision,
            area: area.as_size(),
            side_px,
            animation_frame,
        };
        if self.desired_key != Some(key) {
            self.next_generation = self.next_generation.wrapping_add(1);
            self.encoder.submit(EncodeRequest {
                scene: scene.clone(),
                options: RasterOptions {
                    side_px,
                    theme: BoardTheme::default(),
                    show_coordinates: scene.show_coordinates
                        && self.piece_style == RasterPieceStyle::Detailed,
                    piece_style: self.piece_style,
                },
                tweens,
                encode_font: encoding.encode_font,
                key,
                generation: self.next_generation,
            });
            self.desired_key = Some(key);
        }

        if let Some(displayed) = &mut self.displayed
            && key.can_display(displayed.key, animation_active)
        {
            let force_placeholder_refresh = self.placeholder_refresh_pending;
            self.placeholder_refresh_pending = false;
            render_encoded_frame(
                &mut displayed.frame,
                area,
                frame.buffer_mut(),
                force_placeholder_refresh,
            );
            self.display_ready = true;
        }
    }

    fn observe_scene(&mut self, scene: &BoardScene, ply_count: usize, now: Instant) {
        if let Some(previous) = &self.previous_scene {
            if previous != scene {
                self.scene_revision = self.scene_revision.wrapping_add(1);
            }
            if previous.pieces != scene.pieces || previous.orientation != scene.orientation {
                self.position_revision = self.position_revision.wrapping_add(1);
            }
            if !self.animations_enabled {
                self.animation = None;
            } else if ply_count == self.previous_ply_count.saturating_add(1)
                && previous.orientation == scene.orientation
            {
                self.animation = animation_legs(previous, scene)
                    .map(|legs| MoveAnimation { started: now, legs });
            } else if ply_count != self.previous_ply_count
                || previous.orientation != scene.orientation
                || previous.pieces != scene.pieces
            {
                // Undo, reset, external position changes, and board flips are
                // intentionally immediate rather than played backwards.
                self.animation = None;
            }
        } else {
            self.scene_revision = self.scene_revision.wrapping_add(1);
            self.position_revision = self.position_revision.wrapping_add(1);
        }
        self.previous_scene = Some(scene.clone());
        self.previous_ply_count = ply_count;
    }

    fn poll(&mut self, request_animation_redraw: bool) -> bool {
        let mut changed = false;
        if let Some(response) = self.encoder.take_response()
            && self
                .desired_key
                .is_some_and(|desired| desired.accepts(response.key))
        {
            match response.frame {
                Ok(frame) => {
                    if self
                        .displayed
                        .as_ref()
                        .is_none_or(|frame| response.generation >= frame.generation)
                    {
                        self.displayed = Some(DisplayedFrame {
                            frame,
                            key: response.key,
                            generation: response.generation,
                        });
                        changed = true;
                    }
                }
                Err(error) => {
                    self.disabled_reason =
                        Some(format!("graphics encoder failed; using Unicode ({error})"));
                    self.displayed = None;
                    self.display_ready = false;
                    self.desired_key = None;
                    self.animation = None;
                    self.encoder.discard_pending();
                    changed = true;
                }
            }
        }
        changed || (request_animation_redraw && self.animation.is_some())
    }
}

impl Drop for RasterBoard {
    fn drop(&mut self) {
        self.encoder.shutdown();
        // Dropping a JoinHandle detaches the at-most-one in-flight encode. We
        // deliberately do not join here: terminal restoration and process exit
        // must never wait for graphics compression to finish.
        self.worker.take();
    }
}

fn direct_kitty_transport() -> bool {
    let term = env::var("TERM").unwrap_or_default();
    let term_program = env::var("TERM_PROGRAM").unwrap_or_default();
    let wrapped_term = term == "tmux"
        || term.starts_with("tmux-")
        || term == "screen"
        || term.starts_with("screen-");
    let wrapped_program =
        term_program.eq_ignore_ascii_case("tmux") || term_program.eq_ignore_ascii_case("zellij");
    !wrapped_term
        && !wrapped_program
        && !environment_variable_nonempty("TMUX")
        && !environment_variable_nonempty("ZELLIJ")
}

fn environment_variable_nonempty(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn encode_raster_frame(
    image: DynamicImage,
    encode_font: FontSize,
    area: Size,
    protocol_type: StatefulProtocolType,
    compressed_kitty: bool,
) -> Result<EncodedRasterFrame, String> {
    if compressed_kitty && matches!(&protocol_type, StatefulProtocolType::Kitty(_)) {
        return encode_compressed_kitty_frame(image, area, protocol_type);
    }

    let mut protocol = StatefulProtocol::new(image, encode_font, None, protocol_type);
    protocol.resize_encode(&Resize::Fit(None), area);
    finish_protocol_encoding(&mut protocol)?;
    Ok(EncodedRasterFrame {
        protocol,
        kitty_transmit: None,
    })
}

fn encode_compressed_kitty_frame(
    image: DynamicImage,
    area: Size,
    protocol_type: StatefulProtocolType,
) -> Result<EncodedRasterFrame, String> {
    let rgba = image.to_rgba8();

    // Let ratatui-image build the protocol ID and standards-compliant Unicode
    // placeholders, but consume its tiny dummy transmission off-screen. We
    // replace that raw payload with the same image ID using Kitty's zlib mode.
    let mut protocol = StatefulProtocol::new(
        DynamicImage::new_rgba8(1, 1),
        FontSize::new(1, 1),
        None,
        protocol_type,
    );
    protocol.resize_encode(&Resize::Fit(None), area);
    finish_protocol_encoding(&mut protocol)?;

    let scratch_area = Rect::new(0, 0, area.width, area.height);
    let mut scratch = Buffer::empty(scratch_area);
    ResizeEncodeRender::render(&mut protocol, scratch_area, &mut scratch);
    let first_symbol = scratch
        .cell((0, 0))
        .ok_or_else(|| "Kitty placeholder encoder produced an empty buffer".to_owned())?
        .symbol();
    let image_id = parse_kitty_image_id(first_symbol)?;
    let kitty_transmit = kitty_zlib_transmit(&rgba, image_id, area)?;

    Ok(EncodedRasterFrame {
        protocol,
        kitty_transmit: Some(kitty_transmit),
    })
}

fn finish_protocol_encoding(protocol: &mut StatefulProtocol) -> Result<(), String> {
    match protocol.last_encoding_result() {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("graphics encoder produced no result".to_owned()),
    }
}

fn parse_kitty_image_id(symbol: &str) -> Result<u32, String> {
    const PREFIX: &str = "_Gq=2,i=";
    let start = symbol
        .find(PREFIX)
        .map(|index| index + PREFIX.len())
        .ok_or_else(|| "Kitty placeholder transmission omitted its image ID".to_owned())?;
    let digits = symbol[start..]
        .split_once(',')
        .map_or(&symbol[start..], |(digits, _)| digits);
    digits
        .parse()
        .map_err(|error| format!("Kitty placeholder image ID was invalid ({error})"))
}

fn kitty_zlib_transmit(
    image: &image::RgbaImage,
    image_id: u32,
    placement: Size,
) -> Result<String, String> {
    let mut compressor = ZlibEncoder::new(Vec::new(), Compression::fast());
    compressor
        .write_all(image.as_raw())
        .map_err(|error| format!("Kitty zlib compression failed ({error})"))?;
    let compressed = compressor
        .finish()
        .map_err(|error| format!("Kitty zlib compression failed ({error})"))?;

    // Kitty limits each base64 payload to 4096 bytes, which corresponds to
    // 3072 bytes of binary input without padding.
    const CHUNK_SIZE: usize = 3_072;
    let chunks = compressed.chunks(CHUNK_SIZE);
    let chunk_count = chunks.len();
    let mut transmit = String::with_capacity(compressed.len().div_ceil(3) * 4 + chunk_count * 80);
    for (index, chunk) in chunks.enumerate() {
        transmit.push_str("\x1b_Gq=2,");
        if index == 0 {
            write!(
                transmit,
                "i={image_id},a=T,U=1,f=32,o=z,t=d,s={},v={},c={},r={},",
                image.width(),
                image.height(),
                placement.width,
                placement.height
            )
            .expect("writing to a String is infallible");
        }
        let more = u8::from(index + 1 < chunk_count);
        write!(transmit, "m={more};").expect("writing to a String is infallible");
        BASE64_STANDARD.encode_string(chunk, &mut transmit);
        transmit.push_str("\x1b\\");
    }
    Ok(transmit)
}

fn render_encoded_frame(
    frame: &mut EncodedRasterFrame,
    area: Rect,
    buffer: &mut Buffer,
    force_placeholder_refresh: bool,
) {
    let kitty_placeholders = matches!(
        frame.protocol.protocol_type(),
        StatefulProtocolType::Kitty(_)
    );
    ResizeEncodeRender::render(&mut frame.protocol, area, buffer);
    if frame.kitty_transmit.is_some()
        && let Some(cell) = buffer.cell_mut((area.x, area.y))
    {
        let placeholder = cell.symbol().to_owned();
        let transmit = frame
            .kitty_transmit
            .take()
            .expect("presence was checked before taking the Kitty payload");
        let mut symbol = String::with_capacity(transmit.len() + placeholder.len());
        symbol.push_str(&transmit);
        symbol.push_str(&placeholder);
        cell.set_symbol(&symbol);
    }
    if force_placeholder_refresh && kitty_placeholders {
        force_kitty_placeholder_rows(area, buffer);
    }
}

fn force_kitty_placeholder_rows(area: Rect, buffer: &mut Buffer) {
    for y in area.top()..area.bottom() {
        let Some(cell) = buffer.cell_mut((area.x, y)) else {
            continue;
        };
        let placeholder = cell.symbol();
        if !placeholder.contains('\u{10eeee}') {
            continue;
        }
        // Kitty encodes a complete placeholder row in this one buffer cell.
        // A balanced save/restore pair is terminal-state-neutral while making
        // Ratatui's cell diff emit the row again after a modal covered it.
        let mut refreshed =
            String::with_capacity(KITTY_PLACEHOLDER_REFRESH.len() + placeholder.len());
        refreshed.push_str(KITTY_PLACEHOLDER_REFRESH);
        refreshed.push_str(placeholder);
        cell.set_symbol(&refreshed);
    }
}

#[derive(Clone, Copy, Debug)]
struct RasterEncodingPlan {
    encode_font: FontSize,
    side_px: u32,
}

fn raster_encoding_plan(protocol: ProtocolType, layout: BoardLayout) -> RasterEncodingPlan {
    let area = layout.board.as_size();
    let actual_font = FontSize::new(
        layout.cell_size.width.max(1),
        layout.cell_size.height.max(1),
    );
    let encode_font = match protocol {
        // Kitty placements are sized in terminal cells independently of the
        // uploaded bitmap. Keep enough pixels for crisp pieces without sending
        // a full physical-screen RGBA buffer for every interaction.
        ProtocolType::Kitty => capped_kitty_font(area, actual_font),
        // The half-block encoder ultimately samples one pixel horizontally and
        // two vertically per terminal cell. Anything denser is discarded.
        ProtocolType::Halfblocks => FontSize::new(1, 2),
        // Sixel and iTerm2 placement is pixel-sized, so retain the real cell
        // geometry for those protocols.
        ProtocolType::Sixel | ProtocolType::Iterm2 => actual_font,
    };
    let (target_width, target_height) = raster_target_pixels(area, encode_font)
        .expect("a non-empty board and non-zero encoding font have pixel dimensions");
    let side_px = RasterOptions::new(target_width.min(target_height)).normalized_side_px();
    RasterEncodingPlan {
        encode_font,
        side_px,
    }
}

fn capped_kitty_font(area: Size, actual: FontSize) -> FontSize {
    let Some((actual_width, actual_height)) = raster_target_pixels(area, actual) else {
        return FontSize::new(1, 1);
    };
    if actual_width <= KITTY_MAX_ENCODE_DIMENSION && actual_height <= KITTY_MAX_ENCODE_DIMENSION {
        return actual;
    }

    let width_limit = (KITTY_MAX_ENCODE_DIMENSION / u32::from(area.width.max(1)))
        .max(1)
        .min(u32::from(actual.width));
    let height_limit = (KITTY_MAX_ENCODE_DIMENSION / u32::from(area.height.max(1)))
        .max(1)
        .min(u32::from(actual.height));
    let actual_ratio = f64::from(actual.width) / f64::from(actual.height);
    let mut best = (FontSize::new(1, 1), f64::INFINITY, 0_u64);
    for height in 1..=height_limit {
        for width in 1..=width_limit {
            let ratio = f64::from(width) / f64::from(height);
            let aspect_error = (ratio / actual_ratio).ln().abs();
            let pixels = u64::from(area.width)
                * u64::from(width)
                * u64::from(area.height)
                * u64::from(height);
            if aspect_error < best.1
                || ((aspect_error - best.1).abs() <= f64::EPSILON && pixels > best.2)
            {
                best = (
                    FontSize::new(
                        u16::try_from(width).expect("font width began as u16"),
                        u16::try_from(height).expect("font height began as u16"),
                    ),
                    aspect_error,
                    pixels,
                );
            }
        }
    }
    best.0
}

fn raster_target_pixels(area: Size, font_size: FontSize) -> Option<(u32, u32)> {
    let width = u32::from(area.width).checked_mul(u32::from(font_size.width))?;
    let height = u32::from(area.height).checked_mul(u32::from(font_size.height))?;
    (width > 0 && height > 0).then_some((width, height))
}

fn prepare_raster_image(
    scene: &BoardScene,
    options: RasterOptions,
    tweens: &[PieceTween],
    area: Size,
    font_size: FontSize,
) -> Result<DynamicImage, String> {
    let (target_width, target_height) = raster_target_pixels(area, font_size)
        .ok_or_else(|| "board has invalid terminal pixel dimensions".to_owned())?;
    let image = compose_board_rgba_with_tweens(scene, &options, tweens);
    let filter = match options.piece_style {
        RasterPieceStyle::Detailed => FilterType::Lanczos3,
        RasterPieceStyle::PixelArt => FilterType::Nearest,
    };
    // BoardLayout permits the closest physical square grid. Stretch the
    // usually tiny residual cell-aspect error so the image covers the exact
    // hit-test rectangle instead of being silently suppressed.
    Ok(DynamicImage::ImageRgba8(image).resize_exact(target_width, target_height, filter))
}

fn pixel_art_density_sufficient(layout: BoardLayout) -> bool {
    layout.square_columns >= 6 && layout.square_rows.saturating_mul(2) >= 6
}

fn animation_legs(previous: &BoardScene, current: &BoardScene) -> Option<Vec<AnimationLeg>> {
    let last_move = current.last_move?;
    let previous_piece = previous.piece_at(last_move.from)?;
    let moved_piece = current.piece_at(last_move.to)?;
    if previous_piece.side != moved_piece.side {
        return None;
    }

    let mut legs = vec![AnimationLeg {
        piece: moved_piece,
        from: last_move.from,
        to: last_move.to,
    }];
    if previous_piece.kind == PieceKind::King
        && last_move.from.file.abs_diff(last_move.to.file) == 2
    {
        let king_side = last_move.to.file > last_move.from.file;
        let rook_from = ViewSquare::new(if king_side { 7 } else { 0 }, last_move.to.rank)
            .expect("castling rook source is on the board");
        let rook_to = ViewSquare::new(
            if king_side {
                last_move.to.file - 1
            } else {
                last_move.to.file + 1
            },
            last_move.to.rank,
        )
        .expect("castling rook destination is on the board");
        if let Some(rook) = current.piece_at(rook_to)
            && rook.side == moved_piece.side
            && rook.kind == PieceKind::Rook
            && previous.piece_at(rook_from) == Some(rook)
        {
            legs.push(AnimationLeg {
                piece: rook,
                from: rook_from,
                to: rook_to,
            });
        }
    }
    Some(legs)
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use flate2::read::ZlibDecoder;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::render::Square as ViewSquare;

    #[test]
    fn ui_snapshot_contains_board_and_status() {
        let app = App::new();
        let mut state = UiState::unicode();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("CHESS"));
        assert!(rendered.contains("WHITE TO MOVE"));
        assert_eq!(rendered.matches("WHITE TO MOVE").count(), 1);
        assert!(rendered.contains("Select a piece"));
        assert!(rendered.contains("MOVES"));
        assert!(rendered.contains("No moves yet"));
        assert!(!rendered.contains("terminal atelier"));
        assert!(rendered.contains('▀') || rendered.contains('▄') || rendered.contains('█'));
        assert_eq!(state.board_layout().square_columns, 6);
        assert_eq!(state.board_layout().square_rows, 3);
        assert_eq!(state.board_layout().board.x, 0);
        assert_eq!(state.board_layout().board.height, 24);
    }

    #[test]
    fn engine_game_ui_names_the_opponent_and_uses_turn_level_undo() {
        let mut app = App::vs_engine(SideColor::White);
        app.set_engine_name("Maia-3 5M");
        app.dispatch(crate::app::Command::ToggleHelp);
        let mut state = UiState::unicode();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("YOUR MOVE"));
        assert!(rendered.contains("vs Maia-3 5M"));
        assert!(rendered.contains("Undo the previous turn"));
    }

    #[test]
    fn managed_maia_setup_overlay_explains_the_download_and_refreshes_on_close() {
        let mut app = App::vs_engine(SideColor::White);
        app.set_engine_setup_state(EngineSetupState::InstallPrompt);
        let mut state = UiState::unicode();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("MAIA ENGINE SETUP"));
        assert!(rendered.contains("one-time, isolated setup"));
        assert!(rendered.contains("Python and PyTorch"));
        assert!(rendered.contains("Install Maia"));
        assert!(state.overlay_visible);

        app.clear_engine_setup_state();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        assert!(
            !state.overlay_visible,
            "closing setup must use the same overlay restoration path as help"
        );
    }

    #[test]
    fn managed_maia_setup_shows_progress_and_retryable_errors() {
        let mut app = App::vs_engine(SideColor::White);
        let mut state = UiState::unicode();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        app.set_engine_setup_state(EngineSetupState::Installing {
            step: "Downloading the Maia model".to_owned(),
        });
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let installing = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(installing.contains("INSTALLING MAIA"));
        assert!(installing.contains("Downloading the Maia model"));
        assert!(installing.contains("Keep this terminal open"));

        app.set_engine_setup_state(EngineSetupState::Failed {
            message: "download failed: connection timed out".to_owned(),
        });
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let failed = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(failed.contains("MAIA SETUP FAILED"));
        assert!(failed.contains("download failed: connection timed out"));
        assert!(failed.contains("Retry"));
    }

    #[test]
    fn kitty_raster_is_not_emitted_under_an_already_visible_setup_modal() {
        let mut app = App::vs_engine(SideColor::White);
        app.set_engine_setup_state(EngineSetupState::InstallPrompt);
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut state = UiState::with_animation(Some(picker), false);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render(frame, &app, &mut state, "kitty graphics"))
            .unwrap();
        let modal = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(modal.contains("MAIA ENGINE SETUP"));
        assert!(
            !modal.contains('\u{10eeee}') && !modal.contains("\x1b_G"),
            "inline images must not be emitted underneath a cell-based modal"
        );
        assert!(
            state
                .raster
                .as_ref()
                .is_some_and(|raster| raster.desired_key.is_none()),
            "an already-visible modal must prevent the late raster submission"
        );

        app.clear_engine_setup_state();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            terminal
                .draw(|frame| render(frame, &app, &mut state, "kitty graphics"))
                .unwrap();
            if state
                .raster
                .as_ref()
                .is_some_and(|raster| raster.display_ready)
            {
                break;
            }
            assert!(Instant::now() < deadline, "Kitty frame was not prepared");
            thread::sleep(Duration::from_millis(10));
        }

        let restored = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            restored.contains('\u{10eeee}') || restored.contains("\x1b_G"),
            "the Kitty raster should render after the modal closes"
        );

        app.set_engine_setup_state(EngineSetupState::InstallPrompt);
        terminal
            .draw(|frame| render(frame, &app, &mut state, "kitty graphics"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("MAIA ENGINE SETUP"));
        assert!(
            !rendered.contains('\u{10eeee}') && !rendered.contains("\x1b_G"),
            "an active inline image must also be cleared before drawing a modal"
        );
    }

    #[test]
    fn inline_overlay_suspends_animation_ticks_but_halfblocks_keep_compositing() {
        let animation = || MoveAnimation {
            started: Instant::now(),
            legs: Vec::new(),
        };

        let mut kitty_picker = Picker::halfblocks();
        kitty_picker.set_protocol_type(ProtocolType::Kitty);
        let mut kitty = UiState::with_animation(Some(kitty_picker), true);
        kitty.overlay_visible = true;
        kitty.raster.as_mut().unwrap().animation = Some(animation());
        assert!(
            !kitty.poll(),
            "a hidden inline animation must not keep redrawing a modal"
        );
        kitty.overlay_visible = false;
        assert!(
            kitty.poll(),
            "the animation should resume after modal close"
        );

        let mut halfblocks = UiState::with_animation(Some(Picker::halfblocks()), true);
        halfblocks.overlay_visible = true;
        halfblocks.raster.as_mut().unwrap().animation = Some(animation());
        assert!(
            halfblocks.poll(),
            "cell-composited halfblocks may continue animating under a modal"
        );
    }

    #[test]
    fn engine_failure_replaces_the_thinking_headline_and_stays_visible() {
        let mut app = App::vs_engine(SideColor::White);
        app.set_engine_failure("Engine failed: maia3-uci was not found");
        app.dispatch(crate::app::Command::ActivateSquare(
            crate::chess::Square::E2,
        ));
        app.dispatch(crate::app::Command::ActivateSquare(
            crate::chess::Square::E4,
        ));
        let mut state = UiState::unicode();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("ENGINE FAILED"));
        assert!(rendered.contains("Engine failed:"));
        assert_eq!(app.message(), "Engine failed: maia3-uci was not found");
        assert!(!rendered.contains("ENGINE THINKING"));
    }

    #[test]
    fn narrow_ui_keeps_compact_single_line_chrome() {
        let app = App::new();
        let mut state = UiState::unicode();
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("CHESS"));
        assert!(rendered.contains("WHITE TO MOVE"));
        assert!(rendered.contains("enter select"));
        assert_eq!(state.board_layout().board.height, 16);
    }

    #[test]
    fn roomy_ui_centers_the_board_without_spending_extra_rows_on_chrome() {
        let app = App::new();
        let mut state = UiState::unicode();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();

        let board = state.board_layout();
        assert_eq!((board.square_columns, board.square_rows), (6, 3));
        assert_eq!((board.board.width, board.board.height), (48, 24));
        assert_eq!(board.board.y, 3);
    }

    #[test]
    fn mouse_hit_testing_respects_the_current_board() {
        let app = App::new();
        let mut state = UiState::unicode();
        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &mut state, "unicode"))
            .unwrap();
        let rect = state
            .board_layout()
            .square_rect(ViewSquare::from(app.cursor()), app.orientation())
            .unwrap();
        assert_eq!(
            state.square_at(rect.x, rect.y, app.orientation()),
            Some(app.cursor())
        );
    }

    #[test]
    fn raster_target_pixels_exactly_cover_screenshot_like_board_geometry() {
        assert_eq!(
            raster_target_pixels(Size::new(80, 40), FontSize::new(14, 27)),
            Some((1_120, 1_080))
        );

        let close_aspect = BoardLayout {
            board: Rect::new(0, 0, 80, 40),
            square_columns: 10,
            square_rows: 5,
            cell_size: CellSize::new(14, 27),
        };
        assert_eq!(crate::render::raster_side_for_cells(close_aspect), 1_080);

        let compact_halfblocks = BoardLayout {
            board: Rect::new(0, 0, 32, 16),
            square_columns: 4,
            square_rows: 2,
            cell_size: CellSize::new(10, 20),
        };
        assert!(!pixel_art_density_sufficient(compact_halfblocks));

        let readable_halfblocks = BoardLayout {
            board: Rect::new(0, 0, 48, 24),
            square_columns: 6,
            square_rows: 3,
            cell_size: CellSize::new(10, 20),
        };
        assert!(pixel_art_density_sufficient(readable_halfblocks));
    }

    #[test]
    fn prepared_raster_image_has_the_exact_terminal_pixel_extent() {
        let image = prepare_raster_image(
            &BoardScene::empty(),
            RasterOptions {
                side_px: 64,
                piece_style: RasterPieceStyle::PixelArt,
                ..RasterOptions::default()
            },
            &[],
            Size::new(8, 3),
            FontSize::new(10, 20),
        )
        .unwrap();

        assert_eq!((image.width(), image.height()), (80, 60));
    }

    #[test]
    fn kitty_large_boards_use_an_interactive_pixel_budget() {
        let layout = BoardLayout {
            board: Rect::new(0, 0, 64, 48),
            square_columns: 8,
            square_rows: 6,
            cell_size: CellSize::new(24, 35),
        };
        let plan = raster_encoding_plan(ProtocolType::Kitty, layout);
        let (target_width, target_height) =
            raster_target_pixels(layout.board.as_size(), plan.encode_font).unwrap();

        assert!(target_width <= KITTY_MAX_ENCODE_DIMENSION);
        assert!(target_height <= KITTY_MAX_ENCODE_DIMENSION);
        assert!(plan.side_px >= 512, "piece detail budget fell too low");
        assert!(
            u64::from(target_width) * u64::from(target_height) * 4
                <= u64::from(KITTY_MAX_ENCODE_DIMENSION).pow(2) * 4
        );
    }

    #[test]
    fn halfblocks_encode_only_the_subcell_sample_grid() {
        let layout = BoardLayout {
            board: Rect::new(0, 0, 64, 48),
            square_columns: 8,
            square_rows: 6,
            cell_size: CellSize::new(24, 35),
        };
        let plan = raster_encoding_plan(ProtocolType::Halfblocks, layout);
        let target = raster_target_pixels(layout.board.as_size(), plan.encode_font).unwrap();

        assert_eq!((plan.encode_font.width, plan.encode_font.height), (1, 2));
        assert_eq!(target, (64, 96));
        assert_eq!(plan.side_px, 64);
    }

    #[test]
    fn direct_kitty_frames_transmit_zlib_once_then_reuse_placeholders() {
        let image = image::RgbaImage::from_fn(256, 256, |x, y| {
            if (x / 32 + y / 32).is_multiple_of(2) {
                image::Rgba([220, 225, 205, 255])
            } else {
                image::Rgba([92, 120, 98, 255])
            }
        });
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let protocol_type = picker
            .new_resize_protocol(DynamicImage::new_rgba8(1, 1))
            .protocol_type()
            .clone();
        // Keep raster pixels deliberately unrelated to terminal cells. The
        // virtual placement must use this 8x6 grid, not infer one from 256px.
        let area = Size::new(8, 6);
        let raw_base64_size = image.as_raw().len().div_ceil(3) * 4;
        let expected_rgba = image.as_raw().clone();
        let mut frame = encode_raster_frame(
            DynamicImage::ImageRgba8(image),
            FontSize::new(1, 1),
            area,
            protocol_type,
            true,
        )
        .unwrap();
        let compressed_size = frame.kitty_transmit.as_ref().unwrap().len();

        assert!(compressed_size < raw_base64_size / 8);
        let transmit = frame.kitty_transmit.as_ref().unwrap();
        assert!(transmit.contains("f=32,o=z"));
        assert!(
            transmit.contains("s=256,v=256,c=8,r=6"),
            "bitmap pixels and the virtual placement grid must both be declared: {transmit}"
        );
        let encoded = transmit
            .split("\x1b_Gq=2,")
            .skip(1)
            .map(|command| {
                command
                    .split_once(';')
                    .and_then(|(_, payload)| payload.split_once("\x1b\\"))
                    .map(|(payload, _)| payload)
                    .expect("each Kitty command contains a base64 payload")
            })
            .collect::<String>();
        let compressed = BASE64_STANDARD.decode(encoded).unwrap();
        let mut decoded = Vec::new();
        ZlibDecoder::new(compressed.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, expected_rgba);

        let rect = Rect::new(0, 0, area.width, area.height);
        let mut first = Buffer::empty(rect);
        render_encoded_frame(&mut frame, rect, &mut first, false);
        assert!(first[(0, 0)].symbol().contains("f=32,o=z"));
        assert!(first[(0, 0)].symbol().contains('\u{10eeee}'));
        assert!(frame.kitty_transmit.is_none());

        let mut second = Buffer::empty(rect);
        render_encoded_frame(&mut frame, rect, &mut second, false);
        assert!(!second[(0, 0)].symbol().contains("_Gq=2"));
        assert!(second[(0, 0)].symbol().contains('\u{10eeee}'));

        // A modal overwrites cells in the middle of each placeholder row while
        // leaving the row anchor untouched. The next normal Kitty frame skips
        // those cells, so Ratatui sees nothing it can emit to restore them.
        let mut modal = second.clone();
        for y in 1..5 {
            for x in 1..7 {
                let cell = modal
                    .cell_mut((x, y))
                    .expect("modal cell is inside the test board");
                cell.reset();
                cell.set_symbol("modal");
            }
        }
        let mut unforced = Buffer::empty(rect);
        render_encoded_frame(&mut frame, rect, &mut unforced, false);
        assert!(modal.diff(&unforced).is_empty());

        let mut refreshed = Buffer::empty(rect);
        render_encoded_frame(&mut frame, rect, &mut refreshed, true);
        let restored_rows = modal
            .diff(&refreshed)
            .into_iter()
            .filter(|(_, _, cell)| {
                cell.symbol().contains(KITTY_PLACEHOLDER_REFRESH)
                    && cell.symbol().contains('\u{10eeee}')
            })
            .count();
        assert_eq!(restored_rows, usize::from(area.height));
    }

    #[test]
    fn halfblocks_use_pixel_art_while_inline_images_keep_detailed_pieces() {
        assert_eq!(
            piece_style_for_protocol(ProtocolType::Halfblocks),
            RasterPieceStyle::PixelArt
        );
        assert_eq!(
            piece_style_for_protocol(ProtocolType::Kitty),
            RasterPieceStyle::Detailed
        );
        assert_eq!(
            piece_style_for_protocol(ProtocolType::Sixel),
            RasterPieceStyle::Detailed
        );
    }

    #[test]
    fn animation_tracks_both_castling_pieces() {
        let e1 = ViewSquare::new(4, 0).unwrap();
        let f1 = ViewSquare::new(5, 0).unwrap();
        let g1 = ViewSquare::new(6, 0).unwrap();
        let h1 = ViewSquare::new(7, 0).unwrap();
        let king = ViewPiece::new(crate::render::Side::White, PieceKind::King);
        let rook = ViewPiece::new(crate::render::Side::White, PieceKind::Rook);

        let mut previous = BoardScene::empty();
        previous.set_piece(e1, Some(king));
        previous.set_piece(h1, Some(rook));
        let mut current = BoardScene::empty();
        current.set_piece(g1, Some(king));
        current.set_piece(f1, Some(rook));
        current.last_move = Some(LastMove::new(e1, g1));

        let legs = animation_legs(&previous, &current).unwrap();
        assert_eq!(legs.len(), 2);
        assert!(legs.contains(&AnimationLeg {
            piece: king,
            from: e1,
            to: g1,
        }));
        assert!(legs.contains(&AnimationLeg {
            piece: rook,
            from: h1,
            to: f1,
        }));
    }

    #[test]
    fn raster_keeps_overlay_frames_but_not_wrong_geometry_or_positions() {
        let current = RenderKey {
            scene_revision: 2,
            position_revision: 1,
            area: Size::new(80, 40),
            side_px: 1_080,
            animation_frame: None,
        };
        let overlay_stale = RenderKey {
            scene_revision: 1,
            ..current
        };
        assert!(current.can_display(overlay_stale, false));

        let resized = RenderKey {
            area: Size::new(72, 40),
            ..overlay_stale
        };
        assert!(!current.can_display(resized, false));

        let previous_position = RenderKey {
            position_revision: 0,
            ..overlay_stale
        };
        assert!(!current.can_display(previous_position, false));
        assert!(current.can_display(previous_position, true));
    }

    #[test]
    fn encoder_mailbox_keeps_only_the_latest_request() {
        let mailbox = EncoderMailbox::default();
        let request = |generation| EncodeRequest {
            scene: BoardScene::empty(),
            options: RasterOptions::new(64),
            tweens: Vec::new(),
            encode_font: FontSize::new(1, 1),
            key: RenderKey {
                scene_revision: generation,
                position_revision: generation,
                area: Size::new(8, 8),
                side_px: 64,
                animation_frame: None,
            },
            generation,
        };
        mailbox.submit(request(1));
        mailbox.submit(request(2));

        assert_eq!(mailbox.wait_for_request().unwrap().generation, 2);
        mailbox.shutdown();
        assert!(mailbox.wait_for_request().is_none());
    }
}
