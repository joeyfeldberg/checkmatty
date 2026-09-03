//! Non-blocking supervision for external UCI chess engines.

use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    io::{self, BufRead, BufReader, BufWriter, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::chess::ChessMove;

const SHUTDOWN_GRACE: Duration = Duration::from_millis(150);
const FORCE_REAP_GRACE: Duration = Duration::from_millis(150);
const WORKER_JOIN_GRACE: Duration = Duration::from_millis(500);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_SEARCH_WATCHDOG: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy)]
struct ProtocolTimeouts {
    handshake: Duration,
    ready: Duration,
    node_search: Duration,
    search_grace: Duration,
    stop: Duration,
}

impl Default for ProtocolTimeouts {
    fn default() -> Self {
        Self {
            // Maia may need to import its model before entering the UCI loop.
            handshake: Duration::from_secs(60),
            ready: Duration::from_secs(30),
            node_search: Duration::from_secs(30),
            search_grace: Duration::from_secs(15),
            stop: Duration::from_secs(3),
        }
    }
}

impl ProtocolTimeouts {
    fn search(self, limit: SearchLimit) -> Duration {
        match limit {
            SearchLimit::Nodes(_) => self.node_search,
            SearchLimit::MoveTime(duration) => duration
                .saturating_add(self.search_grace)
                .min(MAX_SEARCH_WATCHDOG),
        }
    }
}

/// A UCI option sent during engine startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UciOption {
    pub name: String,
    pub value: String,
}

impl UciOption {
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// How long the engine may consider one position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchLimit {
    Nodes(u64),
    MoveTime(Duration),
}

impl Default for SearchLimit {
    fn default() -> Self {
        Self::MoveTime(Duration::from_millis(500))
    }
}

/// Process and protocol settings for one external UCI opponent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    pub command: String,
    pub args: Vec<String>,
    pub environment: Vec<(OsString, OsString)>,
    pub options: Vec<UciOption>,
    pub search_limit: SearchLimit,
    pub display_name: String,
}

impl EngineConfig {
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        let command = command.into();
        let display_name = std::path::Path::new(&command)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("Engine")
            .to_owned();
        Self {
            command,
            args: Vec::new(),
            environment: Vec::new(),
            options: Vec::new(),
            search_limit: SearchLimit::default(),
            display_name,
        }
    }
}

/// A search tagged by the application's position revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub id: u64,
    pub initial_fen: String,
    pub moves: Vec<ChessMove>,
}

/// Events produced by the background engine supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    Starting,
    Ready {
        name: String,
    },
    Thinking {
        search_id: u64,
    },
    BestMove {
        search_id: u64,
        best_move: Option<ChessMove>,
    },
    ProtocolError {
        message: String,
    },
    Failed {
        message: String,
    },
}

/// A control request could not reach the engine supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the UCI engine supervisor is no longer running")]
pub struct EngineDisconnected;

/// UI-facing handle for a worker that exclusively owns the child process.
pub struct UciOpponent {
    commands: Sender<WorkerMessage>,
    events: Receiver<EngineEvent>,
    worker: Option<JoinHandle<()>>,
}

impl UciOpponent {
    /// Start the supervisor thread. Executable lookup and model loading happen
    /// on that thread, so this call does not stall terminal initialization.
    pub fn spawn(config: EngineConfig) -> io::Result<Self> {
        Self::spawn_with_protocol_timeouts(config, ProtocolTimeouts::default())
    }

    fn spawn_with_protocol_timeouts(
        config: EngineConfig,
        timeouts: ProtocolTimeouts,
    ) -> io::Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let worker_tx = command_tx.clone();
        let worker = thread::Builder::new()
            .name("chess-uci-engine".to_owned())
            .spawn(move || run_worker(config, timeouts, command_rx, worker_tx, event_tx))?;
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            worker: Some(worker),
        })
    }

    pub fn search(&self, request: SearchRequest) -> Result<(), EngineDisconnected> {
        self.send(Control::Search(request))
    }

    pub fn cancel(&self) -> Result<(), EngineDisconnected> {
        self.send(Control::Cancel)
    }

    pub fn new_game(&self) -> Result<(), EngineDisconnected> {
        self.send(Control::NewGame)
    }

    pub fn try_recv(&self) -> Result<EngineEvent, TryRecvError> {
        self.events.try_recv()
    }

    fn send(&self, control: Control) -> Result<(), EngineDisconnected> {
        self.commands
            .send(WorkerMessage::Control(control))
            .map_err(|_| EngineDisconnected)
    }
}

impl Drop for UciOpponent {
    fn drop(&mut self) {
        let _ = self
            .commands
            .send(WorkerMessage::Control(Control::Shutdown));
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + WORKER_JOIN_GRACE;
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(WORKER_POLL_INTERVAL);
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
            // Dropping the handle detaches a worker delayed in process creation
            // or an operating-system call. Terminal teardown must stay bounded.
        }
    }
}

#[derive(Debug)]
enum Control {
    Search(SearchRequest),
    Cancel,
    NewGame,
    Shutdown,
}

#[derive(Debug)]
enum WorkerMessage {
    Control(Control),
    Output(Output),
}

#[derive(Debug)]
enum Output {
    Line(String),
    Eof,
    ReadError(String),
    WriteError(String),
}

#[derive(Debug)]
enum Phase {
    Handshake {
        deadline: Instant,
    },
    AwaitingReady {
        deadline: Instant,
    },
    Ready,
    Searching {
        request: SearchRequest,
        discard: bool,
        deadline: Instant,
    },
}

impl Phase {
    fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Handshake { deadline }
            | Self::AwaitingReady { deadline }
            | Self::Searching { deadline, .. } => Some(*deadline),
            Self::Ready => None,
        }
    }

    fn timeout_message(&self, engine_name: &str) -> String {
        match self {
            Self::Handshake { .. } => {
                format!("{engine_name} did not complete the UCI handshake in time")
            }
            Self::AwaitingReady { .. } => {
                format!("{engine_name} did not become ready in time")
            }
            Self::Searching { discard: true, .. } => {
                format!("{engine_name} did not stop its previous search in time")
            }
            Self::Searching { discard: false, .. } => {
                format!("{engine_name} did not return a move in time")
            }
            Self::Ready => format!("{engine_name} protocol watchdog expired"),
        }
    }
}

#[derive(Debug)]
struct AsyncStdin {
    commands: Sender<String>,
}

impl AsyncStdin {
    fn spawn(stdin: ChildStdin, messages: Sender<WorkerMessage>) -> io::Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let writer = thread::Builder::new()
            .name("chess-uci-stdin".to_owned())
            .spawn(move || run_stdin_writer(stdin, command_rx, messages))?;
        // The writer may be blocked in an operating-system pipe write. The
        // supervisor kills the child to release it but never waits on it.
        drop(writer);
        Ok(Self {
            commands: command_tx,
        })
    }

    fn send(&self, command: impl Into<String>) -> io::Result<()> {
        let command = command.into();
        validate_single_line(&command)?;
        self.commands.send(command).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the UCI engine stdin writer stopped",
            )
        })
    }
}

fn run_worker(
    config: EngineConfig,
    timeouts: ProtocolTimeouts,
    messages: Receiver<WorkerMessage>,
    message_tx: Sender<WorkerMessage>,
    events: Sender<EngineEvent>,
) {
    let _ = events.send(EngineEvent::Starting);
    let child = spawn_engine(&config);
    let (mut child, child_stdin) = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = events.send(EngineEvent::Failed {
                message: format!("could not start {} ({error})", config.command),
            });
            return;
        }
    };

    let stdin = match AsyncStdin::spawn(child_stdin, message_tx.clone()) {
        Ok(stdin) => stdin,
        Err(error) => {
            let _ = events.send(EngineEvent::Failed {
                message: format!("could not start the UCI stdin writer ({error})"),
            });
            force_terminate_child(child);
            return;
        }
    };

    let stdout = child.stdout.take().expect("spawn_engine pipes stdout");
    spawn_stdout_reader(stdout, message_tx.clone());
    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_drain(stderr);
    }

    if let Err(error) = stdin.send("uci") {
        let _ = events.send(EngineEvent::Failed {
            message: format!("could not initialize engine ({error})"),
        });
        shutdown_child(child, &stdin, false);
        return;
    }

    let mut phase = Phase::Handshake {
        deadline: Instant::now() + timeouts.handshake,
    };
    let mut engine_name = config.display_name.clone();
    let mut pending_search: Option<SearchRequest> = None;
    let mut pending_new_game = false;

    loop {
        let message = match receive_worker_message(&messages, &phase) {
            WorkerReceive::Message(message) => message,
            WorkerReceive::TimedOut => {
                let _ = events.send(EngineEvent::Failed {
                    message: phase.timeout_message(&engine_name),
                });
                shutdown_child(child, &stdin, matches!(phase, Phase::Searching { .. }));
                return;
            }
            WorkerReceive::Disconnected => break,
        };
        let result = match message {
            WorkerMessage::Control(Control::Search(request)) => {
                pending_search = Some(request);
                match &mut phase {
                    Phase::Ready => start_pending_search(
                        &stdin,
                        &config,
                        timeouts,
                        &events,
                        &mut phase,
                        &mut pending_search,
                    ),
                    Phase::Searching {
                        discard, deadline, ..
                    } => request_stop(&stdin, discard, deadline, timeouts.stop),
                    Phase::Handshake { .. } | Phase::AwaitingReady { .. } => Ok(()),
                }
            }
            WorkerMessage::Control(Control::Cancel) => {
                pending_search = None;
                if let Phase::Searching {
                    discard, deadline, ..
                } = &mut phase
                {
                    request_stop(&stdin, discard, deadline, timeouts.stop)
                } else {
                    Ok(())
                }
            }
            WorkerMessage::Control(Control::NewGame) => {
                pending_search = None;
                pending_new_game = true;
                if let Phase::Searching {
                    discard, deadline, ..
                } = &mut phase
                {
                    request_stop(&stdin, discard, deadline, timeouts.stop)
                } else if matches!(phase, Phase::Ready) {
                    begin_new_game(&stdin, timeouts, &mut phase, &mut pending_new_game)
                } else {
                    Ok(())
                }
            }
            WorkerMessage::Control(Control::Shutdown) => {
                shutdown_child(child, &stdin, matches!(phase, Phase::Searching { .. }));
                return;
            }
            WorkerMessage::Output(Output::Line(line)) => handle_output_line(
                &line,
                &config,
                timeouts,
                &events,
                &stdin,
                &mut phase,
                &mut engine_name,
                &mut pending_search,
                &mut pending_new_game,
            ),
            WorkerMessage::Output(Output::Eof) => {
                let status = child.try_wait().ok().flatten().map_or_else(
                    || "closed its output".to_owned(),
                    |status| status.to_string(),
                );
                let _ = events.send(EngineEvent::Failed {
                    message: format!("{engine_name} exited ({status})"),
                });
                shutdown_child(child, &stdin, matches!(phase, Phase::Searching { .. }));
                return;
            }
            WorkerMessage::Output(Output::ReadError(error)) => {
                let _ = events.send(EngineEvent::Failed {
                    message: format!("could not read {engine_name} output ({error})"),
                });
                shutdown_child(child, &stdin, matches!(phase, Phase::Searching { .. }));
                return;
            }
            WorkerMessage::Output(Output::WriteError(error)) => {
                let _ = events.send(EngineEvent::Failed {
                    message: format!("could not write to {engine_name} ({error})"),
                });
                shutdown_child(child, &stdin, matches!(phase, Phase::Searching { .. }));
                return;
            }
        };

        if let Err(error) = result {
            let _ = events.send(EngineEvent::Failed {
                message: format!("could not communicate with {engine_name} ({error})"),
            });
            shutdown_child(child, &stdin, matches!(phase, Phase::Searching { .. }));
            return;
        }
    }

    shutdown_child(child, &stdin, matches!(phase, Phase::Searching { .. }));
}

enum WorkerReceive {
    Message(WorkerMessage),
    TimedOut,
    Disconnected,
}

fn receive_worker_message(messages: &Receiver<WorkerMessage>, phase: &Phase) -> WorkerReceive {
    let Some(deadline) = phase.deadline() else {
        return match messages.recv() {
            Ok(message) => WorkerReceive::Message(message),
            Err(_) => WorkerReceive::Disconnected,
        };
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return WorkerReceive::TimedOut;
    }
    match messages.recv_timeout(remaining) {
        Ok(message) => WorkerReceive::Message(message),
        Err(RecvTimeoutError::Timeout) => WorkerReceive::TimedOut,
        Err(RecvTimeoutError::Disconnected) => WorkerReceive::Disconnected,
    }
}

fn request_stop(
    stdin: &AsyncStdin,
    discard: &mut bool,
    deadline: &mut Instant,
    timeout: Duration,
) -> io::Result<()> {
    if *discard {
        return Ok(());
    }
    *discard = true;
    *deadline = Instant::now() + timeout;
    stdin.send("stop")
}

fn run_stdin_writer(
    stdin: ChildStdin,
    commands: Receiver<String>,
    messages: Sender<WorkerMessage>,
) {
    let mut writer = BufWriter::new(stdin);
    while let Ok(command) = commands.recv() {
        if let Err(error) = write_command(&mut writer, &command) {
            let _ = messages.send(WorkerMessage::Output(Output::WriteError(error.to_string())));
            return;
        }
    }
}

fn spawn_engine(config: &EngineConfig) -> io::Result<(Child, ChildStdin)> {
    validate_single_line(&config.command)?;
    for argument in &config.args {
        validate_single_line(argument)?;
    }
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .envs(config.environment.iter().cloned());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped child has stdin");
    Ok((child, stdin))
}

fn spawn_stdout_reader(stdout: impl io::Read + Send + 'static, messages: Sender<WorkerMessage>) {
    let _ = thread::Builder::new()
        .name("chess-uci-stdout".to_owned())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if interesting_output(&line)
                            && messages
                                .send(WorkerMessage::Output(Output::Line(line)))
                                .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = messages
                            .send(WorkerMessage::Output(Output::ReadError(error.to_string())));
                        return;
                    }
                }
            }
            let _ = messages.send(WorkerMessage::Output(Output::Eof));
        });
}

fn spawn_stderr_drain(stderr: impl io::Read + Send + 'static) {
    let _ = thread::Builder::new()
        .name("chess-uci-stderr".to_owned())
        .spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if line.is_err() {
                    break;
                }
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn handle_output_line(
    line: &str,
    config: &EngineConfig,
    timeouts: ProtocolTimeouts,
    events: &Sender<EngineEvent>,
    stdin: &AsyncStdin,
    phase: &mut Phase,
    engine_name: &mut String,
    pending_search: &mut Option<SearchRequest>,
    pending_new_game: &mut bool,
) -> io::Result<()> {
    if let Some(name) = parse_engine_name(line) {
        name.clone_into(engine_name);
    }

    if line.trim() == "uciok" && matches!(phase, Phase::Handshake { .. }) {
        for option in &config.options {
            stdin.send(format_uci_option(option)?)?;
        }
        stdin.send("isready")?;
        *phase = Phase::AwaitingReady {
            deadline: Instant::now() + timeouts.ready,
        };
        return Ok(());
    }

    if line.trim() == "readyok" && matches!(phase, Phase::AwaitingReady { .. }) {
        *phase = Phase::Ready;
        let _ = events.send(EngineEvent::Ready {
            name: engine_name.clone(),
        });
        if *pending_new_game {
            begin_new_game(stdin, timeouts, phase, pending_new_game)?;
        } else {
            start_pending_search(stdin, config, timeouts, events, phase, pending_search)?;
        }
        return Ok(());
    }

    if line.trim_start().starts_with("bestmove ") {
        let Phase::Searching {
            request, discard, ..
        } = std::mem::replace(phase, Phase::Ready)
        else {
            let _ = events.send(EngineEvent::ProtocolError {
                message: format!("unexpected engine response: {line}"),
            });
            return Ok(());
        };
        if !discard {
            match parse_bestmove(line) {
                Ok(best_move) => {
                    let _ = events.send(EngineEvent::BestMove {
                        search_id: request.id,
                        best_move,
                    });
                }
                Err(message) => {
                    let _ = events.send(EngineEvent::ProtocolError { message });
                }
            }
        }
        if *pending_new_game {
            begin_new_game(stdin, timeouts, phase, pending_new_game)?;
        } else {
            start_pending_search(stdin, config, timeouts, events, phase, pending_search)?;
        }
    }
    Ok(())
}

fn start_pending_search(
    stdin: &AsyncStdin,
    config: &EngineConfig,
    timeouts: ProtocolTimeouts,
    events: &Sender<EngineEvent>,
    phase: &mut Phase,
    pending: &mut Option<SearchRequest>,
) -> io::Result<()> {
    if !matches!(phase, Phase::Ready) {
        return Ok(());
    }
    let Some(request) = pending.take() else {
        return Ok(());
    };
    stdin.send(format_position(&request)?)?;
    stdin.send(format_search_limit(config.search_limit))?;
    let _ = events.send(EngineEvent::Thinking {
        search_id: request.id,
    });
    *phase = Phase::Searching {
        request,
        discard: false,
        deadline: Instant::now() + timeouts.search(config.search_limit),
    };
    Ok(())
}

fn begin_new_game(
    stdin: &AsyncStdin,
    timeouts: ProtocolTimeouts,
    phase: &mut Phase,
    pending_new_game: &mut bool,
) -> io::Result<()> {
    if !matches!(phase, Phase::Ready) {
        return Ok(());
    }
    *pending_new_game = false;
    stdin.send("ucinewgame")?;
    stdin.send("isready")?;
    *phase = Phase::AwaitingReady {
        deadline: Instant::now() + timeouts.ready,
    };
    Ok(())
}

fn write_command(writer: &mut impl Write, command: &str) -> io::Result<()> {
    validate_single_line(command)?;
    writer.write_all(command.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn validate_single_line(value: &str) -> io::Result<()> {
    if value.contains(['\r', '\n']) || value.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UCI values must be non-empty single lines",
        ))
    } else {
        Ok(())
    }
}

fn format_uci_option(option: &UciOption) -> io::Result<String> {
    validate_single_line(&option.name)?;
    validate_single_line(&option.value)?;
    Ok(format!(
        "setoption name {} value {}",
        option.name, option.value
    ))
}

fn format_position(request: &SearchRequest) -> io::Result<String> {
    validate_single_line(&request.initial_fen)?;
    let mut command = format!("position fen {}", request.initial_fen);
    if !request.moves.is_empty() {
        command.push_str(" moves");
        for mv in &request.moves {
            write!(command, " {mv}").expect("writing to a String is infallible");
        }
    }
    Ok(command)
}

fn format_search_limit(limit: SearchLimit) -> String {
    match limit {
        SearchLimit::Nodes(nodes) => format!("go nodes {}", nodes.max(1)),
        SearchLimit::MoveTime(duration) => {
            let milliseconds = duration.as_millis().max(1);
            format!("go movetime {milliseconds}")
        }
    }
}

fn interesting_output(line: &str) -> bool {
    let line = line.trim_start();
    line == "uciok"
        || line == "readyok"
        || line.starts_with("id name ")
        || line.starts_with("bestmove ")
}

fn parse_engine_name(line: &str) -> Option<String> {
    line.trim()
        .strip_prefix("id name ")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn parse_bestmove(line: &str) -> Result<Option<ChessMove>, String> {
    let token = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("engine omitted its move: {line}"))?;
    if matches!(token, "(none)" | "0000") {
        return Ok(None);
    }
    token
        .parse()
        .map(Some)
        .map_err(|_| format!("engine returned an invalid move: {token}"))
}

fn shutdown_child(mut child: Child, stdin: &AsyncStdin, searching: bool) {
    if searching {
        let _ = stdin.send("stop");
    }
    let _ = stdin.send("quit");
    if poll_child_until(&mut child, Instant::now() + SHUTDOWN_GRACE) {
        return;
    }
    force_kill(&mut child);
    finish_reaping(child);
}

fn force_terminate_child(mut child: Child) {
    force_kill(&mut child);
    finish_reaping(child);
}

fn poll_child_until(child: &mut Child, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(WORKER_POLL_INTERVAL),
            Err(_) => break,
        }
    }
    false
}

fn finish_reaping(mut child: Child) {
    if poll_child_until(&mut child, Instant::now() + FORCE_REAP_GRACE) {
        return;
    }
    // Waiting for a killed process can itself wedge in an operating-system
    // state. A detached reaper avoids a zombie without extending terminal Drop.
    drop(
        thread::Builder::new()
            .name("chess-uci-reaper".to_owned())
            .spawn(move || {
                let _ = child.wait();
            }),
    );
}

#[cfg(unix)]
fn force_kill(child: &mut Child) {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };

    if let Ok(process_group) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn force_kill(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const START_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

    #[cfg(unix)]
    fn fake_engine(script: &str) -> EngineConfig {
        let mut config = EngineConfig::new("/bin/sh");
        config.args = vec!["-c".to_owned(), script.to_owned()];
        config.search_limit = SearchLimit::Nodes(1);
        config
    }

    #[cfg(unix)]
    fn test_timeouts() -> ProtocolTimeouts {
        ProtocolTimeouts {
            handshake: Duration::from_millis(150),
            ready: Duration::from_millis(150),
            node_search: Duration::from_millis(500),
            search_grace: Duration::from_millis(150),
            stop: Duration::from_millis(150),
        }
    }

    #[cfg(unix)]
    fn search_request(id: u64, moves: &[&str]) -> SearchRequest {
        SearchRequest {
            id,
            initial_fen: START_FEN.to_owned(),
            moves: moves
                .iter()
                .map(|mv| mv.parse().expect("test moves are coordinate notation"))
                .collect(),
        }
    }

    #[cfg(unix)]
    fn collect_until(
        engine: &UciOpponent,
        timeout: Duration,
        complete: impl Fn(&EngineEvent) -> bool,
    ) -> Vec<EngineEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        while Instant::now() < deadline {
            match engine.try_recv() {
                Ok(event) => {
                    let done = complete(&event);
                    events.push(event);
                    if done {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => break,
            }
        }
        events
    }

    #[test]
    fn formats_history_options_and_search_limits_as_uci() {
        let request = SearchRequest {
            id: 9,
            initial_fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_owned(),
            moves: vec!["e2e4".parse().unwrap(), "e7e5".parse().unwrap()],
        };

        assert_eq!(
            format_position(&request).unwrap(),
            "position fen rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1 moves e2e4 e7e5"
        );
        assert_eq!(
            format_uci_option(&UciOption::new("Elo", "1500")).unwrap(),
            "setoption name Elo value 1500"
        );
        assert_eq!(format_search_limit(SearchLimit::Nodes(1)), "go nodes 1");
        assert_eq!(
            format_search_limit(SearchLimit::MoveTime(Duration::from_millis(275))),
            "go movetime 275"
        );
    }

    #[test]
    fn parses_engine_identity_moves_and_terminal_no_move_responses() {
        assert_eq!(
            parse_engine_name("id name Maia-3 5M"),
            Some("Maia-3 5M".to_owned())
        );
        assert_eq!(
            parse_bestmove("bestmove e7e5 ponder g1f3"),
            Ok(Some("e7e5".parse().unwrap()))
        );
        assert_eq!(parse_bestmove("bestmove (none)"), Ok(None));
        assert_eq!(parse_bestmove("bestmove 0000"), Ok(None));
        assert!(parse_bestmove("bestmove rabbit").is_err());
    }

    #[test]
    fn rejects_multiline_protocol_values() {
        assert!(format_uci_option(&UciOption::new("Elo\nquit", "1500")).is_err());
        let request = SearchRequest {
            id: 1,
            initial_fen: "start\nquit".to_owned(),
            moves: Vec::new(),
        };
        assert!(format_position(&request).is_err());
    }

    #[test]
    fn stdout_reader_ignores_unbounded_analysis_noise() {
        assert!(interesting_output("uciok"));
        assert!(interesting_output("id name Human Engine"));
        assert!(interesting_output("bestmove a7a8q"));
        assert!(!interesting_output(
            "info depth 30 nodes 123456 score cp 12"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn supervises_a_real_uci_process_and_returns_a_tagged_move() {
        let script = r#"while IFS= read -r line; do case "$line" in uci) echo "id name $CHESS_TUI_TEST_ENGINE_NAME"; echo "uciok" ;; isready) echo "readyok" ;; go*) echo "info depth 1 nodes 1"; echo "bestmove e7e5" ;; quit) exit 0 ;; esac; done"#;
        let mut config = EngineConfig::new("/bin/sh");
        config.args = vec!["-c".to_owned(), script.to_owned()];
        config.environment.push((
            OsString::from("CHESS_TUI_TEST_ENGINE_NAME"),
            OsString::from("Friendly Fake"),
        ));
        config.options = vec![UciOption::new("Elo", "1500")];
        config.search_limit = SearchLimit::Nodes(1);
        let engine = UciOpponent::spawn(config).unwrap();
        engine
            .search(SearchRequest {
                id: 77,
                initial_fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_owned(),
                moves: vec!["e2e4".parse().unwrap()],
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            match engine.try_recv() {
                Ok(event) => {
                    let complete = matches!(
                        event,
                        EngineEvent::BestMove {
                            search_id: 77,
                            best_move: Some(_)
                        }
                    );
                    events.push(event);
                    if complete {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => break,
            }
        }

        assert!(
            events.contains(&EngineEvent::Ready {
                name: "Friendly Fake".to_owned()
            }),
            "events: {events:?}"
        );
        assert!(events.contains(&EngineEvent::Thinking { search_id: 77 }));
        assert!(events.contains(&EngineEvent::BestMove {
            search_id: 77,
            best_move: Some("e7e5".parse().unwrap())
        }));
    }

    #[cfg(unix)]
    #[test]
    fn reaps_an_engine_that_closes_stdout_without_exiting() {
        let mut config = EngineConfig::new("/bin/sh");
        config.args = vec!["-c".to_owned(), "exec 1>&-; exec sleep 5".to_owned()];
        let engine = UciOpponent::spawn(config).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match engine.try_recv() {
                Ok(EngineEvent::Failed { .. }) => break,
                Ok(_) | Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                event => panic!("engine did not report its closed output: {event:?}"),
            }
        }

        let started = Instant::now();
        drop(engine);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "dropping the stopped supervisor took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn drop_stays_bounded_when_the_engine_stops_reading_a_full_stdin_pipe() {
        let script = r#"IFS= read -r line; echo uciok; echo readyok; exec sleep 5"#;
        let mut config = fake_engine(script);
        config.options = vec![UciOption::new("Huge", "x".repeat(2 * 1024 * 1024))];
        let engine = UciOpponent::spawn_with_protocol_timeouts(config, test_timeouts()).unwrap();
        let events = collect_until(&engine, Duration::from_secs(2), |event| {
            matches!(event, EngineEvent::Ready { .. })
        });
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Ready { .. })),
            "fake engine never reached the blocked-write phase: {events:?}"
        );

        let started = Instant::now();
        drop(engine);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "dropping a supervisor with blocked stdin took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn handshake_watchdog_fails_and_reaps_an_unresponsive_engine() {
        let config = fake_engine("exec sleep 5");
        let started = Instant::now();
        let engine = UciOpponent::spawn_with_protocol_timeouts(config, test_timeouts()).unwrap();
        let events = collect_until(&engine, Duration::from_secs(2), |event| {
            matches!(event, EngineEvent::Failed { .. })
        });

        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Failed { message } if message.contains("handshake")
            )),
            "watchdog events: {events:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let drop_started = Instant::now();
        drop(engine);
        assert!(drop_started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_search_discards_the_stopped_search_move() {
        let script = r#"seen=0; while IFS= read -r line; do case "$line" in uci) echo 'id name Replacement Fake'; echo uciok ;; isready) echo readyok ;; go*) if [ "$seen" -eq 0 ]; then seen=1; else echo 'bestmove g8f6'; fi ;; stop) echo 'bestmove e7e5' ;; quit) exit 0 ;; esac; done"#;
        let engine =
            UciOpponent::spawn_with_protocol_timeouts(fake_engine(script), test_timeouts())
                .unwrap();
        engine
            .search(search_request(1, &["e2e4"]))
            .expect("first search reaches the worker");
        let mut events = collect_until(&engine, Duration::from_secs(2), |event| {
            matches!(event, EngineEvent::Thinking { search_id: 1 })
        });
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Thinking { search_id: 1 }))
        );

        engine
            .search(search_request(2, &["d2d4"]))
            .expect("replacement search reaches the worker");
        events.extend(collect_until(&engine, Duration::from_secs(2), |event| {
            matches!(event, EngineEvent::BestMove { search_id: 2, .. })
        }));

        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::BestMove {
                    search_id: 2,
                    best_move: Some(mv),
                } if *mv == "g8f6".parse().unwrap()
            )),
            "replacement events: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::BestMove { search_id: 1, .. })),
            "stopped search leaked a move: {events:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancel_discards_the_stopped_search_move() {
        let script = r#"while IFS= read -r line; do case "$line" in uci) echo uciok ;; isready) echo readyok ;; go*) : ;; stop) echo 'bestmove e7e5' ;; quit) exit 0 ;; esac; done"#;
        let engine =
            UciOpponent::spawn_with_protocol_timeouts(fake_engine(script), test_timeouts())
                .unwrap();
        engine
            .search(search_request(8, &["e2e4"]))
            .expect("search reaches the worker");
        let mut events = collect_until(&engine, Duration::from_secs(2), |event| {
            matches!(event, EngineEvent::Thinking { search_id: 8 })
        });
        engine.cancel().expect("cancel reaches the worker");
        events.extend(collect_until(&engine, Duration::from_millis(300), |_| {
            false
        }));

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::BestMove { search_id: 8, .. })),
            "cancelled search leaked a move: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Failed { .. })),
            "cancelled engine failed unexpectedly: {events:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_watchdog_fails_when_replacement_cannot_cancel_the_old_search() {
        let script = r#"while IFS= read -r line; do case "$line" in uci) echo uciok ;; isready) echo readyok ;; go*|stop) : ;; quit) exit 0 ;; esac; done"#;
        let engine =
            UciOpponent::spawn_with_protocol_timeouts(fake_engine(script), test_timeouts())
                .unwrap();
        engine.search(search_request(3, &["e2e4"])).unwrap();
        let mut events = collect_until(&engine, Duration::from_secs(2), |event| {
            matches!(event, EngineEvent::Thinking { search_id: 3 })
        });
        engine.search(search_request(4, &["d2d4"])).unwrap();
        events.extend(collect_until(&engine, Duration::from_secs(2), |event| {
            matches!(event, EngineEvent::Failed { .. })
        }));

        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Failed { message } if message.contains("stop its previous search")
            )),
            "stop watchdog events: {events:?}"
        );
    }
}
