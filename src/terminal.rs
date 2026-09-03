//! Terminal lifecycle, capability negotiation, and the event loop.
//!
//! The rest of the application should not manipulate raw mode or terminal escape
//! sequences directly. [`Runtime`] owns those details and restores the user's
//! terminal on normal return, errors, and unwinding panics.

use std::{
    env,
    io::{self, Read, Stdout, Write},
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use crossterm::{
    ExecutableCommand,
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};
use ratatui_image::{
    FontSize,
    picker::{
        Picker, ProtocolType,
        cap_parser::{Parser as CapabilityParser, QueryStdioOptions, Response},
    },
};

use crate::{
    app::{App, Command, CommandResult, EngineSetupState},
    chess::{Color as ChessColor, Piece},
    cli::{Cli, GraphicsMode, HumanColor},
    engine::{EngineConfig, EngineEvent, SearchLimit, SearchRequest, UciOpponent, UciOption},
    managed_maia::{
        ManagedMaia, ManagedMaiaConfig, ManagedMaiaEvent, ManagedMaiaInstaller, ManagedMaiaPhase,
    },
    ui::{self, UiState},
};

/// Time between application ticks in the default runtime.
pub const DEFAULT_TICK_RATE: Duration = Duration::from_millis(16);

/// Maximum time spent asking the terminal about graphics capabilities.
pub const DEFAULT_CAPABILITY_QUERY_TIMEOUT: Duration = Duration::from_millis(800);

const MAX_ENGINE_EVENTS_PER_POLL: usize = 64;

/// The concrete Ratatui terminal used by the application.
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Options for terminal setup and event delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeOptions {
    /// Interval between [`RuntimeEvent::Tick`] events.
    pub tick_rate: Duration,
    /// Upper bound for the graphics capability query.
    pub capability_query_timeout: Duration,
    /// Whether mouse events should be captured.
    pub mouse_capture: bool,
    /// Whether Kitty keyboard enhancements may be enabled when supported.
    pub enhanced_keyboard: bool,
    /// Whether each Ratatui draw should use synchronized terminal updates.
    pub synchronized_updates: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            tick_rate: DEFAULT_TICK_RATE,
            capability_query_timeout: DEFAULT_CAPABILITY_QUERY_TIMEOUT,
            mouse_capture: true,
            enhanced_keyboard: true,
            synchronized_updates: true,
        }
    }
}

/// The renderer selected after terminal capability negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsKind {
    /// Kitty's graphics protocol, including Ghostty's implementation.
    Kitty,
    /// Sixel graphics.
    Sixel,
    /// iTerm2's inline-image protocol.
    Iterm2,
    /// True-color Unicode half-blocks.
    Halfblocks,
    /// Cell-based chess glyphs without inline images.
    Unicode,
}

impl GraphicsKind {
    /// Whether this selection uses an inline-image terminal protocol.
    #[must_use]
    pub const fn is_inline_image(self) -> bool {
        matches!(self, Self::Kitty | Self::Sixel | Self::Iterm2)
    }
}

impl From<ProtocolType> for GraphicsKind {
    fn from(protocol: ProtocolType) -> Self {
        match protocol {
            ProtocolType::Kitty => Self::Kitty,
            ProtocolType::Sixel => Self::Sixel,
            ProtocolType::Iterm2 => Self::Iterm2,
            ProtocolType::Halfblocks => Self::Halfblocks,
        }
    }
}

/// Graphics capabilities and the picker used to construct image widgets.
#[derive(Debug, Clone)]
pub struct Graphics {
    kind: GraphicsKind,
    picker: Option<Picker>,
}

impl Graphics {
    /// Resolve the requested mode by querying the active terminal when necessary.
    fn negotiate(mode: GraphicsMode, query_timeout: Duration) -> Result<Self> {
        match mode {
            GraphicsMode::Unicode => Ok(Self {
                kind: GraphicsKind::Unicode,
                picker: None,
            }),
            GraphicsMode::Halfblocks => Ok(Self::halfblocks()),
            GraphicsMode::Auto | GraphicsMode::Kitty => {
                let queried = query_picker(nonzero_duration(query_timeout))
                    .map_err(|error| error.to_string());
                Self::resolve_query(
                    mode,
                    queried,
                    GraphicsNegotiationContext::from_environment(),
                )
            }
        }
    }

    fn resolve_query(
        mode: GraphicsMode,
        queried: std::result::Result<Picker, String>,
        context: GraphicsNegotiationContext,
    ) -> Result<Self> {
        match queried {
            Ok(picker) => match (mode, picker.protocol_type()) {
                // A positive terminal response is authoritative, even when an
                // environment hint suggests a different renderer.
                (GraphicsMode::Auto, protocol) if protocol != ProtocolType::Halfblocks => {
                    Ok(Self::from_picker(picker))
                }
                (GraphicsMode::Kitty, ProtocolType::Kitty) => Ok(Self::from_picker(picker)),
                // Halfblocks is not probed by the capability parser. It is the
                // synthesized result when no inline-image response was usable,
                // so a direct Ghostty session can safely refine that fallback.
                (GraphicsMode::Auto, ProtocolType::Halfblocks) if context.direct_ghostty => {
                    Ok(Self::kitty(context.force_font_size(Some(&picker))))
                }
                (GraphicsMode::Auto, ProtocolType::Halfblocks) => Ok(Self::from_picker(picker)),
                // An explicit request is a useful escape hatch for terminals
                // whose Kitty query is incomplete, but do not blindly emit the
                // protocol through multiplexers or SSH sessions.
                (GraphicsMode::Kitty, ProtocolType::Halfblocks) if context.direct_local => {
                    Ok(Self::kitty(context.force_font_size(Some(&picker))))
                }
                (GraphicsMode::Kitty, ProtocolType::Halfblocks) => bail!(
                    "--graphics kitty was requested, but capability detection found no Kitty \
                     response and forcing graphics is unsafe through a multiplexer or SSH session; \
                     use --graphics auto for a safe fallback"
                ),
                (GraphicsMode::Kitty, detected) => bail!(
                    "--graphics kitty was requested, but the terminal reported {detected:?}; \
                     use --graphics auto for a safe fallback"
                ),
                // Callers resolve non-query modes without entering this function.
                _ => unreachable!("non-query graphics modes returned from the query branch"),
            },
            Err(_) if mode == GraphicsMode::Auto && context.direct_ghostty => {
                Ok(Self::kitty(context.force_font_size(None)))
            }
            Err(_) if mode == GraphicsMode::Auto => Ok(Self::halfblocks()),
            Err(_) if mode == GraphicsMode::Kitty && context.direct_local => {
                Ok(Self::kitty(context.force_font_size(None)))
            }
            Err(error) if mode == GraphicsMode::Kitty => Err(eyre!(error)).wrap_err(
                "--graphics kitty was requested, but capability detection failed and forcing \
                 graphics is unsafe through a multiplexer or SSH session; use --graphics auto \
                 for a safe fallback",
            ),
            // Callers resolve non-query modes without entering this function.
            Err(_) => unreachable!("non-query graphics modes returned from the query branch"),
        }
    }

    fn halfblocks() -> Self {
        let picker = window_font_size().map_or_else(Picker::halfblocks, |font_size| {
            picker_with(font_size, ProtocolType::Halfblocks)
        });
        Self::from_picker(picker)
    }

    fn kitty(font_size: FontSize) -> Self {
        Self::from_picker(picker_with(font_size, ProtocolType::Kitty))
    }

    fn from_picker(picker: Picker) -> Self {
        Self {
            kind: picker.protocol_type().into(),
            picker: Some(picker),
        }
    }

    /// The resolved renderer kind.
    #[must_use]
    pub const fn kind(&self) -> GraphicsKind {
        self.kind
    }

    /// The Ratatui Image picker, absent only for the Unicode renderer.
    #[must_use]
    pub const fn picker(&self) -> Option<&Picker> {
        self.picker.as_ref()
    }

    /// Mutable access to the image picker, for example to configure its background.
    #[must_use]
    pub const fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.picker.as_mut()
    }
}

#[derive(Debug, Clone, Copy)]
struct GraphicsNegotiationContext {
    direct_local: bool,
    direct_ghostty: bool,
    window_font_size: Option<FontSize>,
}

impl GraphicsNegotiationContext {
    fn from_environment() -> Self {
        let term_program = env::var("TERM_PROGRAM").ok();
        let term = env::var("TERM").ok();
        let direct_local = is_direct_local_terminal(
            term_program.as_deref(),
            term.as_deref(),
            env_var_nonempty("TMUX"),
            env_var_nonempty("ZELLIJ"),
            env_var_nonempty("SSH_TTY"),
            env_var_nonempty("SSH_CONNECTION"),
        );
        let direct_ghostty = direct_local && is_ghostty_program(term_program.as_deref());
        Self {
            direct_local,
            direct_ghostty,
            window_font_size: window_font_size(),
        }
    }

    fn force_font_size(self, queried: Option<&Picker>) -> FontSize {
        self.window_font_size
            .or_else(|| queried.map(Picker::font_size))
            .unwrap_or(FontSize::new(10, 20))
    }
}

fn is_ghostty_program(term_program: Option<&str>) -> bool {
    term_program.is_some_and(|value| value.eq_ignore_ascii_case("ghostty"))
}

fn is_direct_local_terminal(
    term_program: Option<&str>,
    term: Option<&str>,
    tmux: bool,
    zellij: bool,
    ssh_tty: bool,
    ssh_connection: bool,
) -> bool {
    let multiplexer_program = term_program.is_some_and(|value| {
        value.eq_ignore_ascii_case("tmux") || value.eq_ignore_ascii_case("zellij")
    });
    let multiplexer_term = term.is_some_and(|value| {
        value == "tmux"
            || value.starts_with("tmux-")
            || value == "screen"
            || value.starts_with("screen-")
    });
    !multiplexer_program && !multiplexer_term && !tmux && !zellij && !ssh_tty && !ssh_connection
}

#[derive(Debug, Default, Clone, Copy)]
struct CapabilityReport {
    protocol: Option<ProtocolType>,
    font_size: Option<FontSize>,
}

impl CapabilityReport {
    fn observe(&mut self, response: Response) -> bool {
        match response {
            Response::Kitty => self.protocol = Some(ProtocolType::Kitty),
            Response::Sixel if self.protocol != Some(ProtocolType::Kitty) => {
                self.protocol = Some(ProtocolType::Sixel);
            }
            Response::CellSize(Some((width, height))) => {
                self.font_size = Some(FontSize::new(width, height));
            }
            Response::Status => return true,
            Response::Sixel
            | Response::RectangularOps
            | Response::CellSize(None)
            | Response::CursorPositionReport(_, _)
            | Response::Background(_, _, _) => {}
        }
        false
    }
}

#[cfg(unix)]
fn query_picker(timeout: Duration) -> io::Result<Picker> {
    let report = query_capabilities(timeout)?;
    let protocol = report
        .protocol
        .or_else(environment_iterm2_protocol)
        .unwrap_or(ProtocolType::Halfblocks);
    Ok(picker_with(
        report
            .font_size
            .or_else(window_font_size)
            .unwrap_or(FontSize::new(10, 20)),
        protocol,
    ))
}

#[cfg(not(unix))]
fn query_picker(_timeout: Duration) -> io::Result<Picker> {
    let Some(protocol) = environment_iterm2_protocol() else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe terminal capability probing is unavailable on this platform",
        ));
    };
    Ok(picker_with(
        window_font_size().unwrap_or(FontSize::new(10, 20)),
        protocol,
    ))
}

#[allow(deprecated)]
fn picker_with(font_size: FontSize, protocol: ProtocolType) -> Picker {
    let mut picker = Picker::from_fontsize(font_size);
    picker.set_protocol_type(protocol);
    picker
}

fn window_font_size() -> Option<FontSize> {
    let window = crossterm::terminal::window_size().ok()?;
    if window.columns == 0 || window.rows == 0 || window.width == 0 || window.height == 0 {
        return None;
    }
    Some(FontSize::new(
        window.width / window.columns,
        window.height / window.rows,
    ))
}

fn environment_iterm2_protocol() -> Option<ProtocolType> {
    let term_program = env::var("TERM_PROGRAM").unwrap_or_default();
    let compatible_program = [
        "iTerm",
        "WezTerm",
        "mintty",
        "vscode",
        "Tabby",
        "Hyper",
        "rio",
        "Bobcat",
        "WarpTerminal",
    ]
    .iter()
    .any(|hint| term_program.contains(hint));
    let lc_terminal_is_iterm = env::var("LC_TERMINAL").is_ok_and(|value| value.contains("iTerm"));
    (compatible_program || lc_terminal_is_iterm).then_some(ProtocolType::Iterm2)
}

#[cfg(unix)]
fn query_capabilities(timeout: Duration) -> io::Result<CapabilityReport> {
    let mut options = QueryStdioOptions {
        timeout,
        ..QueryStdioOptions::default()
    };
    if env_var_nonempty("WEZTERM_EXECUTABLE") || env_var_nonempty("KONSOLE_VERSION") {
        options.blacklist_protocols = vec![ProtocolType::Kitty, ProtocolType::Sixel];
    }
    let is_tmux = env::var("TERM").is_ok_and(|value| value.starts_with("tmux"))
        || env::var("TERM_PROGRAM").is_ok_and(|value| value == "tmux");
    let query = CapabilityParser::query(is_tmux, options);
    io::stdout().write_all(query.as_bytes())?;
    io::stdout().flush()?;

    let deadline = Instant::now() + timeout;
    let mut input = io::stdin().lock();
    let mut parser = CapabilityParser::new();
    let mut report = CapabilityReport::default();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let readable = match wait_until_readable(&input, remaining) {
            Ok(readable) => readable,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if remaining.is_zero() || !readable {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal capability query timed out",
            ));
        }

        let mut bytes = [0_u8; 256];
        let read = input.read(&mut bytes)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal capability query reached end of input",
            ));
        }
        for byte in bytes.into_iter().take(read) {
            if consume_capability_byte(&mut parser, &mut report, byte) {
                return Ok(report);
            }
        }
    }
}

#[cfg(unix)]
fn wait_until_readable(input: &impl std::os::fd::AsFd, timeout: Duration) -> io::Result<bool> {
    use nix::{
        errno::Errno,
        sys::{
            select::{FdSet, select},
            time::{TimeVal, TimeValLike},
        },
    };

    let micros = timeout.as_nanos().div_ceil(1_000).min(i64::MAX as u128);
    let mut timeout = TimeVal::microseconds(i64::try_from(micros).unwrap_or(i64::MAX));
    let mut read_fds = FdSet::new();
    read_fds.insert(input.as_fd());
    match select(
        None,
        &mut read_fds,
        None::<&mut FdSet<'_>>,
        None::<&mut FdSet<'_>>,
        &mut timeout,
    ) {
        Ok(ready) => Ok(ready > 0),
        Err(Errno::EINTR) => Err(io::Error::from(io::ErrorKind::Interrupted)),
        Err(error) => Err(error.into()),
    }
}

fn consume_capability_byte(
    parser: &mut CapabilityParser,
    report: &mut CapabilityReport,
    byte: u8,
) -> bool {
    parser
        .push(char::from(byte))
        .into_iter()
        .any(|response| report.observe(response))
}

fn env_var_nonempty(name: &str) -> bool {
    env::var(name).is_ok_and(|value| !value.is_empty())
}

/// Input and timer events delivered to the application reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// A keyboard press, repeat, or release.
    Key(KeyEvent),
    /// A mouse press, release, drag, move, or wheel event.
    Mouse(MouseEvent),
    /// The terminal's cell dimensions changed.
    Resize { width: u16, height: u16 },
    /// A periodic animation/application update.
    Tick,
}

/// Indicates whether [`Runtime::run_loop`] should continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopControl {
    Continue,
    Exit,
}

/// Abstracts Crossterm's event reader so event-loop behavior can be tested.
pub trait EventSource {
    /// Report whether an event is available within `timeout`.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying terminal event source cannot be polled.
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;

    /// Read one available event.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying terminal event source cannot be read.
    fn read(&mut self) -> io::Result<CrosstermEvent>;
}

/// Event source backed by Crossterm's process-wide terminal reader.
#[derive(Debug, Default, Clone, Copy)]
pub struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<CrosstermEvent> {
        event::read()
    }
}

/// Adds deterministic periodic ticks to a terminal event source.
#[derive(Debug)]
pub struct EventPump<S = CrosstermEventSource> {
    source: S,
    tick_rate: Duration,
    next_tick: Instant,
}

impl<S> EventPump<S>
where
    S: EventSource,
{
    /// Construct an event pump. A zero interval is clamped to one millisecond.
    #[must_use]
    pub fn new(source: S, tick_rate: Duration) -> Self {
        let tick_rate = nonzero_duration(tick_rate);
        Self {
            source,
            tick_rate,
            next_tick: Instant::now() + tick_rate,
        }
    }

    /// Block until a relevant input, resize, or tick event is available.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying terminal event source fails.
    pub fn next_event(&mut self) -> io::Result<RuntimeEvent> {
        loop {
            let now = Instant::now();
            if now >= self.next_tick {
                self.advance_tick_deadline(now);
                return Ok(RuntimeEvent::Tick);
            }

            let timeout = self.next_tick.saturating_duration_since(now);

            if self.source.poll(timeout)?
                && let Some(runtime_event) = self.source.read()?.into_runtime_event()
            {
                return Ok(runtime_event);
            }

            // A timeout loops back through the deadline check. Irrelevant events
            // do the same, preventing mouse-move floods from starving ticks.
        }
    }

    fn advance_tick_deadline(&mut self, now: Instant) {
        // Do not emit a large burst of stale ticks after the app was busy.
        self.next_tick = now + self.tick_rate;
    }
}

trait IntoRuntimeEvent {
    fn into_runtime_event(self) -> Option<RuntimeEvent>;
}

impl IntoRuntimeEvent for CrosstermEvent {
    fn into_runtime_event(self) -> Option<RuntimeEvent> {
        match self {
            Self::Key(key) => Some(RuntimeEvent::Key(key)),
            Self::Mouse(mouse) => Some(RuntimeEvent::Mouse(mouse)),
            Self::Resize(width, height) => Some(RuntimeEvent::Resize { width, height }),
            Self::FocusGained | Self::FocusLost | Self::Paste(_) => None,
        }
    }
}

/// A fully initialized terminal, graphics picker, and event source.
pub struct Runtime {
    terminal: Tui,
    graphics: Graphics,
    events: EventPump,
    session: TerminalSession,
    synchronized_updates: bool,
}

impl Runtime {
    /// Enter the fullscreen terminal using default options.
    ///
    /// # Errors
    ///
    /// Returns an error if terminal setup or graphics negotiation fails. Any
    /// partially-applied terminal modes are restored before returning.
    pub fn enter(mode: GraphicsMode) -> Result<Self> {
        Self::enter_with_options(mode, RuntimeOptions::default())
    }

    /// Enter the fullscreen terminal with explicit runtime options.
    ///
    /// # Errors
    ///
    /// Returns an error if terminal setup or graphics negotiation fails. Any
    /// partially-applied terminal modes are restored before returning.
    pub fn enter_with_options(mode: GraphicsMode, options: RuntimeOptions) -> Result<Self> {
        let mut session = TerminalSession::enter().wrap_err("failed to initialize the terminal")?;

        // Probe synchronously before enabling mouse reporting or starting the
        // normal event reader, so response bytes cannot race user input.
        let graphics = Graphics::negotiate(mode, options.capability_query_timeout)?;

        if options.mouse_capture {
            session
                .enable_mouse_capture()
                .wrap_err("failed to enable mouse capture")?;
        }

        if options.enhanced_keyboard && graphics.kind() == GraphicsKind::Kitty {
            session
                .enable_enhanced_keyboard()
                .wrap_err("failed to enable Kitty keyboard enhancements")?;
        }

        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).wrap_err("failed to create the Ratatui terminal")?;

        Ok(Self {
            terminal,
            graphics,
            events: EventPump::new(CrosstermEventSource, options.tick_rate),
            session,
            synchronized_updates: options.synchronized_updates,
        })
    }

    /// Access the negotiated graphics configuration.
    #[must_use]
    pub const fn graphics(&self) -> &Graphics {
        &self.graphics
    }

    /// Mutably access the graphics configuration.
    #[must_use]
    pub const fn graphics_mut(&mut self) -> &mut Graphics {
        &mut self.graphics
    }

    /// Access the underlying Ratatui terminal for advanced operations.
    #[must_use]
    pub const fn terminal_mut(&mut self) -> &mut Tui {
        &mut self.terminal
    }

    /// Render one frame, protected by DEC synchronized-update sequences.
    ///
    /// # Errors
    ///
    /// Returns an error if beginning synchronization or drawing the frame fails.
    pub fn draw<F>(&mut self, render: F) -> Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        let sync = if self.synchronized_updates {
            Some(
                SynchronizedFrame::begin(Arc::clone(&self.session.lifecycle))
                    .wrap_err("failed to begin a synchronized terminal update")?,
            )
        } else {
            None
        };

        self.terminal
            .draw(render)
            .wrap_err("failed to draw a terminal frame")?;
        drop(sync);
        Ok(())
    }

    /// Wait for the next reducer event.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal event source fails.
    pub fn next_event(&mut self) -> Result<RuntimeEvent> {
        self.events
            .next_event()
            .wrap_err("failed to read a terminal event")
    }

    /// Drive a callback until it returns [`LoopControl::Exit`].
    ///
    /// Panics in the callback restore the terminal before resuming the unwind.
    ///
    /// # Errors
    ///
    /// Returns an error if event reading or the callback fails.
    ///
    /// # Panics
    ///
    /// Resumes any panic raised by the callback after restoring the terminal.
    pub fn run_loop<F>(&mut self, mut handle: F) -> Result<()>
    where
        F: FnMut(&mut Self, RuntimeEvent) -> Result<LoopControl>,
    {
        loop {
            let runtime_event = self.next_event()?;
            let handled = panic::catch_unwind(AssertUnwindSafe(|| handle(self, runtime_event)));

            match handled {
                Ok(Ok(LoopControl::Continue)) => {}
                Ok(Ok(LoopControl::Exit)) => return Ok(()),
                Ok(Err(error)) => return Err(error),
                Err(payload) => {
                    self.session.restore();
                    self.session.panic_hook.restore();
                    panic::resume_unwind(payload);
                }
            }
        }
    }
}

/// Convenience boundary for an app that wants Runtime to own its entire terminal scope.
///
/// # Errors
///
/// Returns an error if terminal initialization or the application callback fails.
///
/// # Panics
///
/// Resumes any panic raised by the callback after restoring the terminal.
pub fn with_runtime<T, F>(mode: GraphicsMode, options: RuntimeOptions, app: F) -> Result<T>
where
    F: FnOnce(&mut Runtime) -> Result<T>,
{
    let mut runtime = Runtime::enter_with_options(mode, options)?;
    let result = panic::catch_unwind(AssertUnwindSafe(|| app(&mut runtime)));

    match result {
        Ok(result) => result,
        Err(payload) => {
            runtime.session.restore();
            runtime.session.panic_hook.restore();
            panic::resume_unwind(payload);
        }
    }
}

/// Main entrypoint for the complete chess application.
///
/// # Errors
///
/// Returns an error if the terminal cannot be initialized or the application fails.
pub fn run(cli: Cli) -> Result<()> {
    let mut engine_config = engine_config_from_cli(&cli)?;
    let managed_maia_requested = cli
        .engine
        .as_deref()
        .is_some_and(|engine| engine.eq_ignore_ascii_case("maia"));
    let human_color = match cli.human_color {
        HumanColor::White => ChessColor::White,
        HumanColor::Black => ChessColor::Black,
    };
    let graphics_mode = cli.graphics;
    let animations_enabled = !cli.no_animation;
    let options = RuntimeOptions {
        tick_rate: if cli.no_animation {
            Duration::from_millis(50)
        } else {
            DEFAULT_TICK_RATE
        },
        ..RuntimeOptions::default()
    };
    with_runtime(graphics_mode, options, move |runtime| {
        let picker = runtime.graphics().picker().cloned();
        let graphics_label = graphics_label(runtime.graphics().kind());
        let mut managed_maia = managed_maia_requested.then(|| {
            ManagedMaiaBootstrap::new(
                engine_config
                    .take()
                    .expect("the Maia preset always produces an engine configuration"),
            )
        });
        let mut app = if engine_config.is_some() || managed_maia.is_some() {
            App::vs_engine(human_color)
        } else {
            App::new()
        };
        let mut engine = if let Some(bootstrap) = &mut managed_maia {
            bootstrap
                .prepare(&mut app)
                .and_then(|config| spawn_managed_engine(config, &mut app))
        } else {
            engine_config
                .map(|config| EngineSession::spawn(config, &mut app))
                .transpose()
                .wrap_err("failed to start the UCI engine supervisor")?
        };
        let mut ui_state = UiState::with_animation(picker, animations_enabled);

        if let Some(engine) = &mut engine {
            engine.schedule_if_needed(&mut app);
        }

        runtime.draw(|frame| ui::render(frame, &app, &mut ui_state, graphics_label))?;
        runtime.run_loop(|runtime, event| {
            let revision_before = app.position_revision();
            let (control, mut redraw, action) = handle_event(&mut app, &mut ui_state, event);
            if control == LoopControl::Exit {
                return Ok(control);
            }

            if action == RuntimeAction::InstallEngine
                && let Some(bootstrap) = &mut managed_maia
            {
                redraw |= bootstrap.start_install(&mut app);
            }
            if let Some(bootstrap) = &mut managed_maia {
                let update = bootstrap.poll(&mut app);
                redraw |= update.redraw;
                if let Some(config) = update.engine_config {
                    engine = spawn_managed_engine(config, &mut app);
                    redraw = true;
                    if let Some(engine) = &mut engine {
                        engine.schedule_if_needed(&mut app);
                    }
                }
            }
            if let Some(engine) = &mut engine {
                if app.position_revision() != revision_before {
                    let reset_position = app.game().history().is_empty();
                    redraw |= engine.position_changed(&mut app, reset_position);
                }
                redraw |= engine.poll(&mut app);
            }
            if redraw {
                runtime.draw(|frame| {
                    ui::render(frame, &app, &mut ui_state, graphics_label);
                })?;
            }
            Ok(control)
        })
    })
}

struct ManagedMaiaBootstrap {
    engine_template: EngineConfig,
    config: Option<ManagedMaiaConfig>,
    config_error: Option<String>,
    installer: Option<ManagedMaiaInstaller>,
}

impl ManagedMaiaBootstrap {
    fn new(engine_template: EngineConfig) -> Self {
        match ManagedMaiaConfig::from_environment() {
            Ok(config) => Self::with_config(engine_template, config),
            Err(error) => Self {
                engine_template,
                config: None,
                config_error: Some(concise_setup_error(&error.to_string())),
                installer: None,
            },
        }
    }

    fn with_config(engine_template: EngineConfig, config: ManagedMaiaConfig) -> Self {
        Self {
            engine_template,
            config: Some(config),
            config_error: None,
            installer: None,
        }
    }

    fn prepare(&mut self, app: &mut App) -> Option<EngineConfig> {
        let Some(config) = &self.config else {
            self.show_configuration_error(app);
            return None;
        };
        match config.discover() {
            Ok(Some(installed)) => self.configure_engine(installed, app),
            Ok(None) => {
                app.set_engine_setup_state(EngineSetupState::InstallPrompt);
                None
            }
            Err(error) => {
                app.set_engine_setup_state(EngineSetupState::Failed {
                    message: concise_setup_error(&format!(
                        "Could not inspect the managed Maia installation: {error}"
                    )),
                });
                None
            }
        }
    }

    fn start_install(&mut self, app: &mut App) -> bool {
        if self.installer.is_some() {
            return false;
        }
        if self.config.is_none() {
            match ManagedMaiaConfig::from_environment() {
                Ok(config) => {
                    self.config = Some(config);
                    self.config_error = None;
                }
                Err(error) => {
                    self.config_error = Some(concise_setup_error(&error.to_string()));
                    self.show_configuration_error(app);
                    return true;
                }
            }
        }
        let config = self
            .config
            .as_ref()
            .expect("configuration was resolved above")
            .clone();
        match ManagedMaiaInstaller::start_with_config(config) {
            Ok(installer) => {
                self.installer = Some(installer);
                app.set_engine_setup_state(EngineSetupState::Installing {
                    step: ManagedMaiaPhase::Preparing.message().to_owned(),
                });
            }
            Err(error) => {
                app.set_engine_setup_state(EngineSetupState::Failed {
                    message: concise_setup_error(&format!(
                        "Could not start the Maia installer: {error}"
                    )),
                });
            }
        }
        true
    }

    fn poll(&mut self, app: &mut App) -> ManagedMaiaUpdate {
        let Some(installer) = &self.installer else {
            return ManagedMaiaUpdate::default();
        };
        let mut update = ManagedMaiaUpdate::default();
        let mut terminal_event = false;
        let mut ready = None;

        for _ in 0..16 {
            match installer.try_recv() {
                Ok(ManagedMaiaEvent::Progress(phase)) => {
                    app.set_engine_setup_state(EngineSetupState::Installing {
                        step: phase.message().to_owned(),
                    });
                    update.redraw = true;
                }
                Ok(ManagedMaiaEvent::Ready(installed)) => {
                    ready = Some(installed);
                    terminal_event = true;
                    break;
                }
                Ok(ManagedMaiaEvent::Failed { message }) => {
                    app.set_engine_setup_state(EngineSetupState::Failed {
                        message: concise_setup_error(&message),
                    });
                    update.redraw = true;
                    terminal_event = true;
                    break;
                }
                Ok(ManagedMaiaEvent::Cancelled) => {
                    app.set_engine_setup_state(EngineSetupState::Failed {
                        message: "Setup was cancelled. Press Enter to retry.".to_owned(),
                    });
                    update.redraw = true;
                    terminal_event = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    app.set_engine_setup_state(EngineSetupState::Failed {
                        message: "The Maia installer stopped unexpectedly. Press Enter to retry."
                            .to_owned(),
                    });
                    update.redraw = true;
                    terminal_event = true;
                    break;
                }
            }
        }

        if terminal_event {
            self.installer.take();
        }
        if let Some(installed) = ready {
            update.engine_config = self.configure_engine(installed, app);
            update.redraw = true;
        }
        update
    }

    fn configure_engine(&self, installed: ManagedMaia, app: &mut App) -> Option<EngineConfig> {
        match managed_engine_config(&self.engine_template, &installed) {
            Ok(config) => Some(config),
            Err(message) => {
                app.set_engine_setup_state(EngineSetupState::Failed {
                    message: concise_setup_error(&message),
                });
                None
            }
        }
    }

    fn show_configuration_error(&self, app: &mut App) {
        let detail = self
            .config_error
            .as_deref()
            .unwrap_or("the managed data directory is unavailable");
        app.set_engine_setup_state(EngineSetupState::Failed {
            message: concise_setup_error(&format!("Could not configure Maia setup: {detail}")),
        });
    }
}

#[derive(Default)]
struct ManagedMaiaUpdate {
    redraw: bool,
    engine_config: Option<EngineConfig>,
}

fn managed_engine_config(
    template: &EngineConfig,
    installed: &ManagedMaia,
) -> std::result::Result<EngineConfig, String> {
    const PRESET_ARGUMENTS: [&str; 3] = ["--model", "maia3-5m", "--use-uci-history"];

    let mut config = template.clone();
    let has_expected_preset = config
        .args
        .get(..PRESET_ARGUMENTS.len())
        .is_some_and(|arguments| arguments.iter().map(String::as_str).eq(PRESET_ARGUMENTS));
    if !has_expected_preset {
        return Err("The internal Maia engine preset is incomplete.".to_owned());
    }
    let custom_arguments = config.args.split_off(PRESET_ARGUMENTS.len());
    config.command = installed
        .command()
        .to_str()
        .ok_or_else(|| "The managed Maia path is not valid UTF-8.".to_owned())?
        .to_owned();
    config.args = installed
        .args()
        .into_iter()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| "The managed Maia cache path is not valid UTF-8.".to_owned())
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    config.args.extend(custom_arguments);
    config.environment = installed.environment().to_vec();
    Ok(config)
}

fn spawn_managed_engine(config: EngineConfig, app: &mut App) -> Option<EngineSession> {
    match EngineSession::spawn(config, app) {
        Ok(engine) => {
            app.clear_engine_setup_state();
            Some(engine)
        }
        Err(error) => {
            app.set_engine_setup_state(EngineSetupState::Failed {
                message: concise_setup_error(&format!(
                    "Maia is installed but could not start: {error}"
                )),
            });
            None
        }
    }
}

fn concise_setup_error(message: &str) -> String {
    const MAX_CHARACTERS: usize = 180;
    const HEAD_CHARACTERS: usize = 105;
    const TAIL_CHARACTERS: usize = 65;

    let flattened = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= MAX_CHARACTERS {
        return flattened;
    }
    let head = flattened.chars().take(HEAD_CHARACTERS).collect::<String>();
    let tail = flattened
        .chars()
        .rev()
        .take(TAIL_CHARACTERS)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{head} ... {tail}")
}

#[derive(Debug)]
struct PendingEngineSearch {
    id: u64,
    position_revision: u64,
    fen: String,
}

impl PendingEngineSearch {
    fn still_matches(&self, app: &App, search_id: u64) -> bool {
        self.id == search_id
            && self.position_revision == app.position_revision()
            && self.fen == app.game().fen()
            && app.is_engine_turn()
    }
}

struct EngineSession {
    opponent: UciOpponent,
    pending: Option<PendingEngineSearch>,
    next_search_id: u64,
    failed: bool,
}

impl EngineSession {
    fn spawn(config: EngineConfig, app: &mut App) -> io::Result<Self> {
        app.set_engine_name(config.display_name.clone());
        app.set_engine_status(format!("Starting {}", config.display_name));
        Ok(Self {
            opponent: UciOpponent::spawn(config)?,
            pending: None,
            next_search_id: 0,
            failed: false,
        })
    }

    fn poll(&mut self, app: &mut App) -> bool {
        if self.failed {
            return false;
        }
        let mut changed = false;
        for _ in 0..MAX_ENGINE_EVENTS_PER_POLL {
            let event = match self.opponent.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.fail(app, "Engine supervisor disconnected".to_owned());
                    return true;
                }
            };
            changed = true;
            match event {
                EngineEvent::Starting => {
                    let name = app.engine_name().unwrap_or("Engine");
                    app.set_engine_status(format!("Starting {name}"));
                }
                EngineEvent::Ready { name } => {
                    app.set_engine_name(name);
                    if app.is_human_turn() {
                        app.set_engine_status("Your move");
                    }
                }
                EngineEvent::Thinking { search_id } => {
                    if self
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.id == search_id)
                    {
                        let name = app.engine_name().unwrap_or("Engine");
                        app.set_engine_status(format!("{name} is thinking"));
                    }
                }
                EngineEvent::BestMove {
                    search_id,
                    best_move,
                } => {
                    let Some(pending) = self.pending.take_if(|pending| pending.id == search_id)
                    else {
                        continue;
                    };
                    if !pending.still_matches(app, search_id) {
                        continue;
                    }
                    match best_move {
                        Some(mv) => {
                            if let Err(error) = app.apply_engine_move(mv) {
                                self.fail(app, format!("Engine move {mv} was rejected: {error}"));
                            }
                        }
                        None if !app.game().is_game_over() => {
                            self.fail(
                                app,
                                "Engine returned no move in a playable position".to_owned(),
                            );
                        }
                        None => {}
                    }
                }
                EngineEvent::ProtocolError { message } | EngineEvent::Failed { message } => {
                    self.pending = None;
                    self.fail(app, message);
                    break;
                }
            }
        }
        if !self.failed {
            changed |= self.schedule_if_needed(app);
        }
        changed
    }

    fn position_changed(&mut self, app: &mut App, reset_position: bool) -> bool {
        if self.pending.take().is_some() && self.opponent.cancel().is_err() {
            self.fail(app, "Engine supervisor disconnected".to_owned());
            return true;
        }
        if reset_position && self.opponent.new_game().is_err() {
            self.fail(app, "Engine supervisor disconnected".to_owned());
            return true;
        }
        self.schedule_if_needed(app)
    }

    fn schedule_if_needed(&mut self, app: &mut App) -> bool {
        if self.failed || self.pending.is_some() || !app.is_engine_turn() {
            return false;
        }
        self.next_search_id = self.next_search_id.wrapping_add(1);
        let request = SearchRequest {
            id: self.next_search_id,
            initial_fen: app.game().initial_fen(),
            moves: app
                .game()
                .history()
                .iter()
                .map(|record| record.mv)
                .collect(),
        };
        let pending = PendingEngineSearch {
            id: request.id,
            position_revision: app.position_revision(),
            fen: app.game().fen(),
        };
        if self.opponent.search(request).is_err() {
            self.fail(app, "Engine supervisor disconnected".to_owned());
            return true;
        }
        self.pending = Some(pending);
        let name = app.engine_name().unwrap_or("Engine");
        app.set_engine_status(format!("{name} is thinking"));
        true
    }

    fn fail(&mut self, app: &mut App, message: String) {
        self.failed = true;
        self.pending = None;
        app.set_engine_failure(format!("Engine failed: {message}"));
    }
}

fn engine_config_from_cli(cli: &Cli) -> Result<Option<EngineConfig>> {
    let Some(requested) = cli.engine.as_deref() else {
        return Ok(None);
    };
    let maia_preset = requested.eq_ignore_ascii_case("maia");
    if !maia_preset
        && (cli.engine_elo.is_some()
            || cli.engine_temperature.is_some()
            || cli.engine_top_p.is_some())
    {
        bail!(
            "--engine-elo, --engine-temperature, and --engine-top-p are only valid with --engine maia; use --engine-option for other UCI engines"
        );
    }
    let mut config = EngineConfig::new(if maia_preset { "maia3-uci" } else { requested });
    if maia_preset {
        let elo = cli.engine_elo.unwrap_or(1500);
        let temperature = cli.engine_temperature.unwrap_or(1.0);
        let top_p = cli.engine_top_p.unwrap_or(1.0);
        config.display_name = format!("Maia-3 5M · {elo} Elo");
        config.args.extend([
            "--model".to_owned(),
            "maia3-5m".to_owned(),
            "--use-uci-history".to_owned(),
        ]);
        config.options.extend([
            UciOption::new("Elo", elo.to_string()),
            UciOption::new("Temperature", temperature.to_string()),
            UciOption::new("TopP", top_p.to_string()),
        ]);
        config.search_limit = SearchLimit::Nodes(1);
    }
    config.args.extend(cli.engine_args.iter().cloned());
    config.options.extend(
        cli.engine_options
            .iter()
            .map(|option| UciOption::new(option.name.clone(), option.value.clone())),
    );
    if let Some(nodes) = cli.engine_nodes {
        config.search_limit = SearchLimit::Nodes(nodes);
    } else if let Some(milliseconds) = cli.engine_movetime_ms {
        config.search_limit = SearchLimit::MoveTime(Duration::from_millis(milliseconds));
    }
    Ok(Some(config))
}

fn graphics_label(kind: GraphicsKind) -> &'static str {
    match kind {
        GraphicsKind::Kitty => "kitty graphics",
        GraphicsKind::Sixel => "sixel graphics",
        GraphicsKind::Iterm2 => "iterm2 graphics",
        GraphicsKind::Halfblocks => "truecolor halfblocks",
        GraphicsKind::Unicode => "unicode cells",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeAction {
    None,
    InstallEngine,
}

fn handle_event(
    app: &mut App,
    ui: &mut UiState,
    event: RuntimeEvent,
) -> (LoopControl, bool, RuntimeAction) {
    let command = match event {
        RuntimeEvent::Key(key) => command_for_key(app, key),
        RuntimeEvent::Mouse(mouse) => command_for_mouse(app, ui, mouse),
        RuntimeEvent::Resize { .. } => {
            return (LoopControl::Continue, true, RuntimeAction::None);
        }
        RuntimeEvent::Tick => {
            return (LoopControl::Continue, ui.poll(), RuntimeAction::None);
        }
    };
    let Some(command) = command else {
        return (LoopControl::Continue, false, RuntimeAction::None);
    };
    match app.dispatch(command) {
        CommandResult::Continue => (LoopControl::Continue, true, RuntimeAction::None),
        CommandResult::InstallEngineRequested => {
            (LoopControl::Continue, true, RuntimeAction::InstallEngine)
        }
        CommandResult::Quit => (LoopControl::Exit, false, RuntimeAction::None),
    }
}

fn command_for_key(app: &App, key: KeyEvent) -> Option<Command> {
    if key.kind == KeyEventKind::Repeat {
        return cursor_command_for_key(key.code);
    }
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Command::Quit);
    }

    if app.engine_setup_state().is_some() {
        return match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => Some(Command::InstallEngine),
            KeyCode::Esc => Some(Command::Quit),
            KeyCode::Char(character) => match character.to_ascii_lowercase() {
                'i' | 'r' => Some(Command::InstallEngine),
                'q' => Some(Command::Quit),
                _ => None,
            },
            _ => None,
        };
    }

    if app.pending_promotion().is_some() {
        return match key.code {
            KeyCode::Char(character) => match character.to_ascii_lowercase() {
                'q' => Some(Command::Promote(Piece::Queen)),
                'r' => Some(Command::Promote(Piece::Rook)),
                'b' => Some(Command::Promote(Piece::Bishop)),
                'n' => Some(Command::Promote(Piece::Knight)),
                _ => None,
            },
            KeyCode::Esc => Some(Command::Cancel),
            _ => None,
        };
    }

    cursor_command_for_key(key.code).or_else(|| match key.code {
        KeyCode::Enter | KeyCode::Char(' ') => Some(Command::Activate),
        KeyCode::Esc => Some(Command::Cancel),
        KeyCode::Char(character) => match character.to_ascii_lowercase() {
            'u' => Some(Command::Undo),
            'n' => Some(Command::NewGame),
            'f' => Some(Command::FlipBoard),
            'c' => Some(Command::ToggleCoordinates),
            '?' => Some(Command::ToggleHelp),
            'q' => Some(Command::Quit),
            _ => None,
        },
        _ => None,
    })
}

fn cursor_command_for_key(code: KeyCode) -> Option<Command> {
    match code {
        KeyCode::Left => Some(Command::MoveCursor {
            file_delta: -1,
            rank_delta: 0,
        }),
        KeyCode::Right => Some(Command::MoveCursor {
            file_delta: 1,
            rank_delta: 0,
        }),
        KeyCode::Up => Some(Command::MoveCursor {
            file_delta: 0,
            rank_delta: 1,
        }),
        KeyCode::Down => Some(Command::MoveCursor {
            file_delta: 0,
            rank_delta: -1,
        }),
        KeyCode::Char(character) => match character.to_ascii_lowercase() {
            'h' => Some(Command::MoveCursor {
                file_delta: -1,
                rank_delta: 0,
            }),
            'l' => Some(Command::MoveCursor {
                file_delta: 1,
                rank_delta: 0,
            }),
            'k' => Some(Command::MoveCursor {
                file_delta: 0,
                rank_delta: 1,
            }),
            'j' => Some(Command::MoveCursor {
                file_delta: 0,
                rank_delta: -1,
            }),
            _ => None,
        },
        _ => None,
    }
}

fn command_for_mouse(app: &App, ui: &UiState, mouse: MouseEvent) -> Option<Command> {
    if app.engine_setup_state().is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => Some(Command::Quit),
            _ => None,
        };
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => ui
            .square_at(mouse.column, mouse.row, app.orientation())
            .map(Command::ActivateSquare),
        MouseEventKind::Down(MouseButton::Right) => Some(Command::Cancel),
        _ => None,
    }
}

const RAW_MODE: u8 = 1 << 0;
const ALTERNATE_SCREEN: u8 = 1 << 1;
const CURSOR_HIDDEN: u8 = 1 << 2;
const MOUSE_CAPTURE: u8 = 1 << 3;
const ENHANCED_KEYBOARD: u8 = 1 << 4;
const SYNCHRONIZED_UPDATE: u8 = 1 << 5;
const ENABLE_BUTTON_MOUSE_CAPTURE: &[u8] = b"\x1b[?1000h\x1b[?1006h";

#[derive(Debug, Default)]
struct Lifecycle {
    state: AtomicU8,
}

impl Lifecycle {
    fn mark(&self, flag: u8) {
        self.state.fetch_or(flag, Ordering::Release);
    }

    fn take(&self, flag: u8) -> bool {
        self.state.fetch_and(!flag, Ordering::AcqRel) & flag != 0
    }

    fn restore(&self) -> io::Result<()> {
        let state = self.state.swap(0, Ordering::AcqRel);
        if state == 0 {
            return Ok(());
        }

        let mut first_error = None;
        record_error(
            &mut first_error,
            restore_ansi_modes(&mut io::stdout(), state),
        );
        if state & RAW_MODE != 0 {
            record_error(&mut first_error, disable_raw_mode());
        }

        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

struct TerminalSession {
    lifecycle: Arc<Lifecycle>,
    panic_hook: PanicHookGuard,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        let lifecycle = Arc::new(Lifecycle::default());

        enable_raw_mode()?;
        lifecycle.mark(RAW_MODE);

        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        lifecycle.mark(ALTERNATE_SCREEN);
        stdout.execute(Hide)?;
        lifecycle.mark(CURSOR_HIDDEN);

        let panic_hook = PanicHookGuard::install(Arc::clone(&lifecycle));
        Ok(Self {
            lifecycle,
            panic_hook,
        })
    }

    fn enable_mouse_capture(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        stdout.write_all(ENABLE_BUTTON_MOUSE_CAPTURE)?;
        stdout.flush()?;
        self.lifecycle.mark(MOUSE_CAPTURE);
        Ok(())
    }

    fn enable_enhanced_keyboard(&mut self) -> io::Result<()> {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        io::stdout().execute(PushKeyboardEnhancementFlags(flags))?;
        self.lifecycle.mark(ENHANCED_KEYBOARD);
        Ok(())
    }

    fn restore(&self) {
        let _ = self.lifecycle.restore();
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.restore();
        self.panic_hook.restore();
    }
}

type PanicHook = dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static;

struct PanicHookGuard {
    previous: Arc<PanicHook>,
    installed: bool,
}

impl PanicHookGuard {
    fn install(lifecycle: Arc<Lifecycle>) -> Self {
        let previous: Arc<PanicHook> = panic::take_hook().into();
        let previous_for_hook = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            let _ = lifecycle.restore();
            previous_for_hook(info);
        }));
        Self {
            previous,
            installed: true,
        }
    }

    fn restore(&mut self) {
        if !self.installed || std::thread::panicking() {
            return;
        }

        // Remove our hook before installing a wrapper around the original one.
        drop(panic::take_hook());
        let previous = Arc::clone(&self.previous);
        panic::set_hook(Box::new(move |info| previous(info)));
        self.installed = false;
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

struct SynchronizedFrame {
    lifecycle: Arc<Lifecycle>,
}

impl SynchronizedFrame {
    fn begin(lifecycle: Arc<Lifecycle>) -> io::Result<Self> {
        io::stdout().execute(BeginSynchronizedUpdate)?;
        lifecycle.mark(SYNCHRONIZED_UPDATE);
        Ok(Self { lifecycle })
    }
}

impl Drop for SynchronizedFrame {
    fn drop(&mut self) {
        if self.lifecycle.take(SYNCHRONIZED_UPDATE) {
            let _ = io::stdout().execute(EndSynchronizedUpdate);
        }
    }
}

fn nonzero_duration(duration: Duration) -> Duration {
    if duration.is_zero() {
        Duration::from_millis(1)
    } else {
        duration
    }
}

fn record_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
}

fn restore_ansi_modes<W>(writer: &mut W, state: u8) -> io::Result<()>
where
    W: io::Write,
{
    let mut first_error = None;

    if state & SYNCHRONIZED_UPDATE != 0 {
        record_error(
            &mut first_error,
            writer.execute(EndSynchronizedUpdate).map(|_| ()),
        );
    }
    if state & ENHANCED_KEYBOARD != 0 {
        record_error(
            &mut first_error,
            writer.execute(PopKeyboardEnhancementFlags).map(|_| ()),
        );
    }
    if state & MOUSE_CAPTURE != 0 {
        record_error(
            &mut first_error,
            writer.execute(DisableMouseCapture).map(|_| ()),
        );
    }
    if state & CURSOR_HIDDEN != 0 {
        record_error(&mut first_error, writer.execute(Show).map(|_| ()));
    }
    if state & ALTERNATE_SCREEN != 0 {
        record_error(
            &mut first_error,
            writer.execute(LeaveAlternateScreen).map(|_| ()),
        );
    }

    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    #[cfg(unix)]
    use std::fs;

    use clap::Parser as _;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    use super::*;

    #[derive(Debug, Default)]
    struct FakeEvents {
        events: VecDeque<CrosstermEvent>,
    }

    impl EventSource for FakeEvents {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            Ok(!self.events.is_empty())
        }

        fn read(&mut self) -> io::Result<CrosstermEvent> {
            self.events
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no event"))
        }
    }

    fn negotiation_context(
        direct_local: bool,
        direct_ghostty: bool,
        window_font_size: Option<FontSize>,
    ) -> GraphicsNegotiationContext {
        GraphicsNegotiationContext {
            direct_local,
            direct_ghostty,
            window_font_size,
        }
    }

    #[cfg(unix)]
    fn write_test_executable(path: &std::path::Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn crossterm_events_map_to_runtime_events() {
        let key = KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(
            CrosstermEvent::Key(key).into_runtime_event(),
            Some(RuntimeEvent::Key(key))
        );
        assert_eq!(
            CrosstermEvent::Resize(120, 40).into_runtime_event(),
            Some(RuntimeEvent::Resize {
                width: 120,
                height: 40
            })
        );
        assert_eq!(CrosstermEvent::FocusGained.into_runtime_event(), None);
    }

    #[test]
    fn maia_cli_preset_builds_the_human_history_uci_configuration() {
        let cli = Cli::try_parse_from([
            "chess-tui",
            "--engine",
            "maia",
            "--engine-elo",
            "1725",
            "--engine-temperature",
            "0.7",
        ])
        .unwrap();
        let config = engine_config_from_cli(&cli).unwrap().unwrap();

        assert_eq!(config.command, "maia3-uci");
        assert_eq!(config.args, ["--model", "maia3-5m", "--use-uci-history"]);
        assert_eq!(config.search_limit, SearchLimit::Nodes(1));
        assert!(config.options.contains(&UciOption::new("Elo", "1725")));
        assert!(
            config
                .options
                .contains(&UciOption::new("Temperature", "0.7"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_maia_bootstrap_installs_then_preserves_preset_and_custom_arguments() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "chess-terminal-maia-bootstrap-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let fake_uv = root.join("fake-uv");
        write_test_executable(
            &fake_uv,
            r#"#!/bin/sh
set -eu
if [ "$1" = "venv" ]; then
  venv="$4"
  mkdir -p "$venv/bin"
  printf '#!/bin/sh\nexit 0\n' > "$venv/bin/maia3-cache"
  printf '#!/bin/sh\nexit 0\n' > "$venv/bin/maia3-uci"
  chmod +x "$venv/bin/maia3-cache" "$venv/bin/maia3-uci"
fi
"#,
        );

        let cli = Cli::try_parse_from([
            "chess-tui",
            "--engine",
            "maia",
            "--engine-arg=--device",
            "--engine-arg=cpu",
        ])
        .unwrap();
        let template = engine_config_from_cli(&cli).unwrap().unwrap();
        let managed_config = ManagedMaiaConfig::in_data_root(&root).with_uv_program(fake_uv);
        let mut bootstrap = ManagedMaiaBootstrap::with_config(template, managed_config.clone());
        let mut app = App::vs_engine(ChessColor::White);

        assert!(bootstrap.prepare(&mut app).is_none());
        assert_eq!(
            app.engine_setup_state(),
            Some(&EngineSetupState::InstallPrompt)
        );
        assert!(bootstrap.start_install(&mut app));

        let deadline = Instant::now() + Duration::from_secs(5);
        let configured = loop {
            let update = bootstrap.poll(&mut app);
            if let Some(config) = update.engine_config {
                break config;
            }
            assert!(
                !matches!(
                    app.engine_setup_state(),
                    Some(EngineSetupState::Failed { .. })
                ),
                "managed setup failed: {:?}",
                app.engine_setup_state()
            );
            assert!(Instant::now() < deadline, "managed setup timed out");
            std::thread::sleep(Duration::from_millis(10));
        };

        assert!(
            configured
                .command
                .starts_with(root.to_string_lossy().as_ref())
        );
        assert_eq!(
            configured
                .args
                .iter()
                .filter(|argument| argument.as_str() == "--use-uci-history")
                .count(),
            1
        );
        assert!(
            configured
                .args
                .windows(2)
                .any(|arguments| { arguments[0] == "--model" && arguments[1] == "maia3-5m" })
        );
        assert!(configured.args.contains(&"--local-files-only".to_owned()));
        assert!(
            configured
                .args
                .ends_with(&["--device".to_owned(), "cpu".to_owned()])
        );
        let huggingface_home = configured
            .environment
            .iter()
            .find(|(name, _)| name == "HF_HOME")
            .map(|(_, value)| value)
            .expect("managed engine should receive its private cache");
        assert!(std::path::Path::new(huggingface_home).starts_with(&root));
        assert!(managed_config.discover().unwrap().is_some());

        drop(bootstrap);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maia_sampling_flags_are_rejected_for_other_engines() {
        let cli = Cli::try_parse_from([
            "chess-tui",
            "--engine",
            "stockfish",
            "--engine-temperature",
            "0.7",
        ])
        .unwrap();

        let error = engine_config_from_cli(&cli).unwrap_err();
        assert!(error.to_string().contains("only valid with --engine maia"));
    }

    #[test]
    fn stale_engine_searches_no_longer_match_after_undo() {
        let mut app = App::vs_engine(ChessColor::White);
        app.dispatch(Command::ActivateSquare(crate::chess::Square::E2));
        app.dispatch(Command::ActivateSquare(crate::chess::Square::E4));
        let pending = PendingEngineSearch {
            id: 4,
            position_revision: app.position_revision(),
            fen: app.game().fen(),
        };
        assert!(pending.still_matches(&app, 4));

        app.dispatch(Command::Undo);

        assert!(!pending.still_matches(&app, 4));
    }

    #[test]
    fn capability_parser_prefers_kitty_and_records_cell_size() {
        let bytes = b"\x1b[?64;4c\x1b_Gi=31;OK\x1b\\\x1b[6;7;14t\x1b[0n";
        let mut parser = CapabilityParser::new();
        let mut report = CapabilityReport::default();
        let mut complete = false;
        for byte in bytes {
            complete |= consume_capability_byte(&mut parser, &mut report, *byte);
        }

        assert!(complete);
        assert_eq!(report.protocol, Some(ProtocolType::Kitty));
        let font = report.font_size.expect("cell size response should be kept");
        assert_eq!((font.width, font.height), (14, 7));
    }

    #[test]
    fn mouse_capture_requests_buttons_without_motion_floods() {
        let sequence = std::str::from_utf8(ENABLE_BUTTON_MOUSE_CAPTURE).unwrap();
        assert!(sequence.contains("?1000h"));
        assert!(sequence.contains("?1006h"));
        assert!(!sequence.contains("?1002h"));
        assert!(!sequence.contains("?1003h"));
    }

    #[test]
    fn event_pump_skips_irrelevant_events() {
        let source = FakeEvents {
            events: VecDeque::from([CrosstermEvent::FocusLost, CrosstermEvent::Resize(88, 24)]),
        };
        let mut pump = EventPump::new(source, Duration::from_secs(1));

        assert_eq!(
            pump.next_event().unwrap(),
            RuntimeEvent::Resize {
                width: 88,
                height: 24
            }
        );
    }

    #[test]
    fn due_tick_is_not_starved_by_ready_input() {
        let source = FakeEvents {
            events: VecDeque::from([CrosstermEvent::Resize(88, 24)]),
        };
        let mut pump = EventPump::new(source, Duration::from_secs(1));
        pump.next_tick = Instant::now();

        assert_eq!(pump.next_event().unwrap(), RuntimeEvent::Tick);
        assert_eq!(
            pump.next_event().unwrap(),
            RuntimeEvent::Resize {
                width: 88,
                height: 24
            }
        );
    }

    #[test]
    fn zero_tick_rate_is_clamped() {
        let pump = EventPump::new(FakeEvents::default(), Duration::ZERO);
        assert_eq!(pump.tick_rate, Duration::from_millis(1));
    }

    #[test]
    fn key_repeats_only_move_the_cursor() {
        let app = App::new();
        let repeat = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        };

        assert_eq!(
            command_for_key(&app, repeat(KeyCode::Up)),
            Some(Command::MoveCursor {
                file_delta: 0,
                rank_delta: 1,
            })
        );
        assert_eq!(
            command_for_key(&app, repeat(KeyCode::Char('j'))),
            Some(Command::MoveCursor {
                file_delta: 0,
                rank_delta: -1,
            })
        );
        assert_eq!(command_for_key(&app, repeat(KeyCode::Char('q'))), None);
        assert_eq!(command_for_key(&app, repeat(KeyCode::Char('n'))), None);
        assert_eq!(command_for_key(&app, repeat(KeyCode::Enter)), None);
    }

    #[test]
    fn maia_setup_keys_request_install_or_quit_without_reaching_the_board() {
        let mut app = App::vs_engine(ChessColor::White);
        app.set_engine_setup_state(EngineSetupState::InstallPrompt);
        let press = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert_eq!(
            command_for_key(&app, press(KeyCode::Enter)),
            Some(Command::InstallEngine)
        );
        assert_eq!(
            command_for_key(&app, press(KeyCode::Char('r'))),
            Some(Command::InstallEngine)
        );
        assert_eq!(
            command_for_key(&app, press(KeyCode::Char('q'))),
            Some(Command::Quit)
        );
        assert_eq!(command_for_key(&app, press(KeyCode::Left)), None);

        let mut ui = UiState::new(None);
        let (_, _, action) =
            handle_event(&mut app, &mut ui, RuntimeEvent::Key(press(KeyCode::Enter)));
        assert_eq!(action, RuntimeAction::InstallEngine);
    }

    #[test]
    fn lifecycle_flags_are_taken_once() {
        let lifecycle = Lifecycle::default();
        lifecycle.mark(SYNCHRONIZED_UPDATE);

        assert!(lifecycle.take(SYNCHRONIZED_UPDATE));
        assert!(!lifecycle.take(SYNCHRONIZED_UPDATE));
    }

    #[test]
    fn graphics_kind_identifies_inline_protocols() {
        assert!(GraphicsKind::Kitty.is_inline_image());
        assert!(GraphicsKind::Sixel.is_inline_image());
        assert!(GraphicsKind::Iterm2.is_inline_image());
        assert!(!GraphicsKind::Halfblocks.is_inline_image());
        assert!(!GraphicsKind::Unicode.is_inline_image());
    }

    #[test]
    fn auto_query_error_falls_back_to_halfblocks() {
        let graphics = Graphics::resolve_query(
            GraphicsMode::Auto,
            Err("timed out".to_owned()),
            negotiation_context(true, false, Some(FontSize::new(12, 24))),
        )
        .expect("auto mode should degrade gracefully");

        assert_eq!(graphics.kind(), GraphicsKind::Halfblocks);
        assert_eq!(
            graphics.picker().map(Picker::protocol_type),
            Some(ProtocolType::Halfblocks)
        );
    }

    #[test]
    fn explicit_kitty_rejects_a_positive_non_kitty_response() {
        let picker = picker_with(FontSize::new(12, 24), ProtocolType::Sixel);
        let error = Graphics::resolve_query(
            GraphicsMode::Kitty,
            Ok(picker),
            negotiation_context(true, false, Some(FontSize::new(12, 24))),
        )
        .expect_err("a positive terminal response must remain authoritative");

        assert!(error.to_string().contains("terminal reported Sixel"));
        assert!(error.to_string().contains("--graphics auto"));
    }

    #[test]
    fn explicit_kitty_accepts_positive_detection() {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);

        let graphics = Graphics::resolve_query(
            GraphicsMode::Kitty,
            Ok(picker),
            negotiation_context(false, false, None),
        )
        .unwrap();

        assert_eq!(graphics.kind(), GraphicsKind::Kitty);
    }

    #[test]
    fn terminal_classifier_guards_multiplexers_and_ssh() {
        assert!(is_direct_local_terminal(
            Some("ghostty"),
            Some("xterm-256color"),
            false,
            false,
            false,
            false,
        ));
        assert!(is_ghostty_program(Some("Ghostty")));
        assert!(!is_ghostty_program(Some("kitty")));

        let guarded = [
            (
                Some("ghostty"),
                Some("xterm-256color"),
                true,
                false,
                false,
                false,
            ),
            (
                Some("ghostty"),
                Some("tmux-256color"),
                false,
                false,
                false,
                false,
            ),
            (
                Some("ghostty"),
                Some("screen-256color"),
                false,
                false,
                false,
                false,
            ),
            (
                Some("ghostty"),
                Some("xterm-256color"),
                false,
                true,
                false,
                false,
            ),
            (
                Some("ghostty"),
                Some("xterm-256color"),
                false,
                false,
                true,
                false,
            ),
            (
                Some("ghostty"),
                Some("xterm-256color"),
                false,
                false,
                false,
                true,
            ),
        ];
        for (term_program, term, tmux, zellij, ssh_tty, ssh_connection) in guarded {
            assert!(!is_direct_local_terminal(
                term_program,
                term,
                tmux,
                zellij,
                ssh_tty,
                ssh_connection,
            ));
        }
    }

    #[test]
    fn auto_uses_kitty_for_an_ambiguous_direct_ghostty_result() {
        let picker = picker_with(FontSize::new(10, 20), ProtocolType::Halfblocks);
        let graphics = Graphics::resolve_query(
            GraphicsMode::Auto,
            Ok(picker),
            negotiation_context(true, true, Some(FontSize::new(14, 28))),
        )
        .unwrap();

        assert_eq!(graphics.kind(), GraphicsKind::Kitty);
        let font = graphics.picker().unwrap().font_size();
        assert_eq!((font.width, font.height), (14, 28));
    }

    #[test]
    fn auto_uses_kitty_after_a_direct_ghostty_query_error() {
        let graphics = Graphics::resolve_query(
            GraphicsMode::Auto,
            Err("timed out".to_owned()),
            negotiation_context(true, true, Some(FontSize::new(13, 27))),
        )
        .unwrap();

        assert_eq!(graphics.kind(), GraphicsKind::Kitty);
        let font = graphics.picker().unwrap().font_size();
        assert_eq!((font.width, font.height), (13, 27));
    }

    #[test]
    fn auto_keeps_a_positive_protocol_response_in_ghostty() {
        let picker = picker_with(FontSize::new(12, 24), ProtocolType::Sixel);
        let graphics = Graphics::resolve_query(
            GraphicsMode::Auto,
            Ok(picker),
            negotiation_context(true, true, Some(FontSize::new(13, 27))),
        )
        .unwrap();

        assert_eq!(graphics.kind(), GraphicsKind::Sixel);
    }

    #[test]
    fn explicit_kitty_forces_an_ambiguous_direct_result() {
        let graphics = Graphics::resolve_query(
            GraphicsMode::Kitty,
            Err("timed out".to_owned()),
            negotiation_context(true, false, Some(FontSize::new(15, 30))),
        )
        .unwrap();

        assert_eq!(graphics.kind(), GraphicsKind::Kitty);
        let font = graphics.picker().unwrap().font_size();
        assert_eq!((font.width, font.height), (15, 30));
    }

    #[test]
    fn explicit_kitty_refines_synthesized_halfblocks_in_a_direct_session() {
        let picker = picker_with(FontSize::new(10, 20), ProtocolType::Halfblocks);
        let graphics = Graphics::resolve_query(
            GraphicsMode::Kitty,
            Ok(picker),
            negotiation_context(true, false, Some(FontSize::new(16, 32))),
        )
        .unwrap();

        assert_eq!(graphics.kind(), GraphicsKind::Kitty);
        let font = graphics.picker().unwrap().font_size();
        assert_eq!((font.width, font.height), (16, 32));
    }

    #[test]
    fn explicit_kitty_does_not_force_through_a_wrapper() {
        let error = Graphics::resolve_query(
            GraphicsMode::Kitty,
            Err("timed out".to_owned()),
            negotiation_context(false, false, Some(FontSize::new(15, 30))),
        )
        .expect_err("an unconfirmed protocol must not be forced through a wrapper");

        assert!(
            error
                .to_string()
                .contains("unsafe through a multiplexer or SSH")
        );
    }

    #[test]
    fn restoration_sequences_are_complete_and_ordered() {
        let state = SYNCHRONIZED_UPDATE
            | ENHANCED_KEYBOARD
            | MOUSE_CAPTURE
            | CURSOR_HIDDEN
            | ALTERNATE_SCREEN;
        let mut output = Vec::new();
        restore_ansi_modes(&mut output, state).unwrap();
        let output = String::from_utf8(output).unwrap();

        let synchronized = output.find("\x1b[?2026l").unwrap();
        let keyboard = output.find("\x1b[<1u").unwrap();
        let mouse = output.find("\x1b[?1000l").unwrap();
        let cursor = output.find("\x1b[?25h").unwrap();
        let alternate = output.find("\x1b[?1049l").unwrap();

        assert!(synchronized < keyboard);
        assert!(keyboard < mouse);
        assert!(mouse < cursor);
        assert!(cursor < alternate);
    }
}
