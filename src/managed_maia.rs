//! App-managed installation of the Maia-3 UCI engine.
//!
//! Installation is deliberately kept outside the terminal process: a worker
//! thread owns every child process, redirects its output to a log, and reports
//! coarse progress through [`ManagedMaiaEvent`]. A completed installation is
//! published with an atomic completion marker, so discovery never observes a
//! half-installed engine. The virtual environment is created at its final path
//! because Python console-script shebangs embed that absolute path.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Upstream source revision used by the managed installer.
pub const MAIA3_COMMIT: &str = "1e13597c42d4858b7cfd7cfdae01e297263364b2";
/// CPU-friendly Maia model installed by the managed preset.
pub const MAIA3_MODEL: &str = "maia3-5m";
/// Immutable Hugging Face snapshot used for the managed 5M model.
pub const MAIA3_MODEL_REVISION: &str = "b6559de2398d7140b985f28fd2c19fb5e47ddabe";

const MAIA3_ARCHIVE_URL: &str =
    "https://github.com/CSSLab/maia3/archive/1e13597c42d4858b7cfd7cfdae01e297263364b2.tar.gz";
const READY_MANIFEST: &str = "maia3-source=1e13597c42d4858b7cfd7cfdae01e297263364b2\nmaia3-model=b6559de2398d7140b985f28fd2c19fb5e47ddabe\n";
const PYTHON_VERSION: &str = "3.12";
const READY_MARKER: &str = ".chess-tui-ready";
const LOG_FILE: &str = "install.log";
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TERMINATE_GRACE: Duration = Duration::from_millis(250);
const DROP_JOIN_GRACE: Duration = Duration::from_secs(1);
const MAX_ERROR_LOG_BYTES: u64 = 12 * 1024;

static UNIQUE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Paths and process settings for a managed Maia installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedMaiaConfig {
    data_root: PathBuf,
    uv_program: OsString,
    extra_environment: Vec<(OsString, OsString)>,
}

impl ManagedMaiaConfig {
    /// Resolve the platform data directory and optional environment overrides.
    ///
    /// `CHESS_TUI_DATA_DIR` overrides the platform directory and
    /// `CHESS_TUI_UV` overrides the `uv` executable found through `PATH`.
    pub fn from_environment() -> io::Result<Self> {
        let data_root = match env::var_os("CHESS_TUI_DATA_DIR") {
            Some(path) if path.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CHESS_TUI_DATA_DIR cannot be empty",
                ));
            }
            Some(path) => absolute_path(PathBuf::from(path))?,
            None => default_data_root()?,
        };
        let uv_program = match env::var_os("CHESS_TUI_UV") {
            Some(program) if program.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CHESS_TUI_UV cannot be empty",
                ));
            }
            Some(program) => program,
            None => OsString::from("uv"),
        };
        Ok(Self {
            data_root,
            uv_program,
            extra_environment: Vec::new(),
        })
    }

    /// Use an explicit data root. This is useful for portable installations and
    /// keeps tests independent from the user's home directory.
    #[must_use]
    pub fn in_data_root(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            uv_program: OsString::from("uv"),
            extra_environment: Vec::new(),
        }
    }

    /// Override the `uv` executable without invoking a shell.
    #[must_use]
    pub fn with_uv_program(mut self, program: impl Into<OsString>) -> Self {
        self.uv_program = program.into();
        self
    }

    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Return a complete installed runtime, if this pinned version is ready.
    pub fn discover(&self) -> io::Result<Option<ManagedMaia>> {
        let installation = self.installation();
        if installation.is_ready()? {
            Ok(Some(installation))
        } else {
            Ok(None)
        }
    }

    fn installation(&self) -> ManagedMaia {
        let install_dir = self
            .data_root
            .join("engines")
            .join("maia3")
            .join(MAIA3_COMMIT);
        ManagedMaia::new(self.data_root.clone(), install_dir)
    }

    fn process_environment(&self) -> Vec<(OsString, OsString)> {
        let cache_root = self.data_root.join("cache");
        let mut vars = vec![
            path_env("UV_CACHE_DIR", cache_root.join("uv")),
            path_env(
                "UV_PYTHON_INSTALL_DIR",
                self.data_root.join("runtimes").join("python"),
            ),
            (OsString::from("UV_NO_MODIFY_PATH"), OsString::from("1")),
            path_env("XDG_CACHE_HOME", cache_root.join("xdg")),
            path_env("HF_HOME", cache_root.join("huggingface")),
            path_env(
                "HUGGINGFACE_HUB_CACHE",
                cache_root.join("huggingface").join("hub"),
            ),
            path_env("TORCH_HOME", cache_root.join("torch")),
            path_env("PIP_CACHE_DIR", cache_root.join("pip")),
            path_env("PYTHONPYCACHEPREFIX", cache_root.join("pycache")),
        ];
        vars.extend(self.extra_environment.iter().cloned());
        vars
    }
}

/// A complete, locally installed Maia engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedMaia {
    data_root: PathBuf,
    install_dir: PathBuf,
    uci_command: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl ManagedMaia {
    fn new(data_root: PathBuf, install_dir: PathBuf) -> Self {
        let uci_command = console_script(&install_dir.join("venv"), "maia3-uci");
        let config = ManagedMaiaConfig::in_data_root(data_root.clone());
        Self {
            data_root,
            install_dir,
            uci_command,
            environment: config.process_environment(),
        }
    }

    /// Discover the pinned managed engine using platform defaults.
    pub fn discover() -> io::Result<Option<Self>> {
        ManagedMaiaConfig::from_environment()?.discover()
    }

    #[must_use]
    pub fn command(&self) -> &Path {
        &self.uci_command
    }

    /// Arguments that make the cached model usable offline and preserve move
    /// history for human-like predictions.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        vec![
            OsString::from("--model"),
            OsString::from(MAIA3_MODEL),
            OsString::from("--cache-dir"),
            self.model_cache_dir().into_os_string(),
            OsString::from("--revision"),
            OsString::from(MAIA3_MODEL_REVISION),
            OsString::from("--local-files-only"),
            OsString::from("--use-uci-history"),
        ]
    }

    /// Environment variables that keep Hugging Face, Torch, Python, pip, and
    /// uv caches under the chess-tui data root.
    #[must_use]
    pub fn environment(&self) -> &[(OsString, OsString)] {
        &self.environment
    }

    #[must_use]
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    #[must_use]
    pub fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    #[must_use]
    pub fn model_cache_dir(&self) -> PathBuf {
        self.data_root.join("cache").join("huggingface").join("hub")
    }

    fn is_ready(&self) -> io::Result<bool> {
        let marker = match fs::read_to_string(self.install_dir.join(READY_MARKER)) {
            Ok(marker) => marker,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(marker == READY_MANIFEST && executable_file(&self.uci_command)?)
    }
}

/// Coarse phases suitable for display inside a terminal UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedMaiaPhase {
    Preparing,
    Installing,
    CachingModel,
}

impl ManagedMaiaPhase {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Preparing => "Preparing private Python environment...",
            Self::Installing => "Installing Maia-3 and its dependencies...",
            Self::CachingModel => "Downloading the Maia-3 5M model...",
        }
    }
}

/// Events emitted by the background installer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagedMaiaEvent {
    Progress(ManagedMaiaPhase),
    Ready(ManagedMaia),
    Failed { message: String },
    Cancelled,
}

/// The installer worker has already stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the Maia installer is no longer running")]
pub struct ManagedMaiaInstallerDisconnected;

enum InstallerControl {
    Cancel,
}

/// Non-blocking handle for installing the pinned Maia engine.
pub struct ManagedMaiaInstaller {
    config: ManagedMaiaConfig,
    controls: Sender<InstallerControl>,
    events: Receiver<ManagedMaiaEvent>,
    worker: Option<JoinHandle<()>>,
}

impl ManagedMaiaInstaller {
    /// Start installation using platform defaults and environment overrides.
    pub fn start() -> io::Result<Self> {
        Self::start_with_config(ManagedMaiaConfig::from_environment()?)
    }

    /// Start installation with explicit paths. No network or process work is
    /// performed on the caller's thread.
    pub fn start_with_config(config: ManagedMaiaConfig) -> io::Result<Self> {
        let (control_tx, control_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let worker_config = config.clone();
        let worker = thread::Builder::new()
            .name("chess-maia-installer".to_owned())
            .spawn(move || install_worker(worker_config, control_rx, &event_tx))?;
        Ok(Self {
            config,
            controls: control_tx,
            events: event_rx,
            worker: Some(worker),
        })
    }

    pub fn try_recv(&self) -> Result<ManagedMaiaEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub fn cancel(&self) -> Result<(), ManagedMaiaInstallerDisconnected> {
        self.controls
            .send(InstallerControl::Cancel)
            .map_err(|_| ManagedMaiaInstallerDisconnected)
    }

    /// Consume a failed/cancelled installer and start again with the same
    /// configuration. An incomplete version directory is removed by the worker.
    pub fn retry(mut self) -> io::Result<Self> {
        let config = self.config.clone();
        self.stop_and_join();
        Self::start_with_config(config)
    }

    fn stop_and_join(&mut self) {
        let _ = self.controls.send(InstallerControl::Cancel);
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + DROP_JOIN_GRACE;
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(POLL_INTERVAL);
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for ManagedMaiaInstaller {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn install_worker(
    config: ManagedMaiaConfig,
    controls: Receiver<InstallerControl>,
    events: &Sender<ManagedMaiaEvent>,
) {
    match install(&config, &controls, events) {
        Ok(InstallOutcome::Ready(installation)) => {
            let _ = events.send(ManagedMaiaEvent::Ready(installation));
        }
        Ok(InstallOutcome::Cancelled) => {
            let _ = events.send(ManagedMaiaEvent::Cancelled);
        }
        Err(error) => {
            let _ = events.send(ManagedMaiaEvent::Failed {
                message: error.to_string(),
            });
        }
    }
}

enum InstallOutcome {
    Ready(ManagedMaia),
    Cancelled,
}

fn install(
    config: &ManagedMaiaConfig,
    controls: &Receiver<InstallerControl>,
    events: &Sender<ManagedMaiaEvent>,
) -> io::Result<InstallOutcome> {
    if let Some(installation) = config.discover()? {
        return Ok(InstallOutcome::Ready(installation));
    }
    if cancelled(controls) {
        return Ok(InstallOutcome::Cancelled);
    }

    let installation = config.installation();
    let install_parent = installation
        .install_dir
        .parent()
        .expect("the versioned install directory always has a parent");
    fs::create_dir_all(install_parent)?;
    prepare_cache_directories(config)?;

    // A final directory without the marker can only be an interrupted or old
    // incompatible installation. It is safe to replace inside our pinned root.
    if installation.install_dir.exists() {
        fs::remove_dir_all(&installation.install_dir)?;
    }

    fs::create_dir(&installation.install_dir)?;
    let mut incomplete = IncompleteInstallGuard::new(installation.install_dir.clone());
    let log_path = installation.install_dir.join(LOG_FILE);
    let venv = installation.install_dir.join("venv");
    let process_env = config.process_environment();

    emit_progress(events, ManagedMaiaPhase::Preparing);
    let mut venv_command = Command::new(&config.uv_program);
    venv_command
        .args([
            OsStr::new("venv"),
            OsStr::new("--python"),
            OsStr::new(PYTHON_VERSION),
        ])
        .arg(&venv);
    if run_logged_command(
        &mut venv_command,
        &process_env,
        &log_path,
        controls,
        "create the private Python environment",
    )? == CommandOutcome::Cancelled
    {
        return Ok(InstallOutcome::Cancelled);
    }

    emit_progress(events, ManagedMaiaPhase::Installing);
    let mut install_command = Command::new(&config.uv_program);
    install_command
        .args([
            OsStr::new("pip"),
            OsStr::new("install"),
            OsStr::new("--python"),
        ])
        .arg(&venv)
        .arg(MAIA3_ARCHIVE_URL);
    if run_logged_command(
        &mut install_command,
        &process_env,
        &log_path,
        controls,
        "install Maia-3",
    )? == CommandOutcome::Cancelled
    {
        return Ok(InstallOutcome::Cancelled);
    }

    let cache_command_path = console_script(&venv, "maia3-cache");
    if !executable_file(&cache_command_path)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Maia-3 installed without its cache command at {}",
                cache_command_path.display()
            ),
        ));
    }

    emit_progress(events, ManagedMaiaPhase::CachingModel);
    let mut cache_command = Command::new(&cache_command_path);
    cache_command
        .args([
            OsStr::new("--model"),
            OsStr::new(MAIA3_MODEL),
            OsStr::new("--cache-dir"),
        ])
        .arg(installation.model_cache_dir())
        .args([OsStr::new("--revision"), OsStr::new(MAIA3_MODEL_REVISION)]);
    if run_logged_command(
        &mut cache_command,
        &process_env,
        &log_path,
        controls,
        "cache the Maia-3 model",
    )? == CommandOutcome::Cancelled
    {
        return Ok(InstallOutcome::Cancelled);
    }

    let staged_uci = console_script(&venv, "maia3-uci");
    if !executable_file(&staged_uci)? {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Maia-3 installed without its UCI command at {}",
                staged_uci.display()
            ),
        ));
    }
    publish_ready_marker(&installation.install_dir)?;
    incomplete.disarm();

    config.discover()?.map_or_else(
        || {
            Err(io::Error::other(
                "Maia-3 installation disappeared while publishing readiness",
            ))
        },
        |ready| Ok(InstallOutcome::Ready(ready)),
    )
}

fn emit_progress(events: &Sender<ManagedMaiaEvent>, phase: ManagedMaiaPhase) {
    let _ = events.send(ManagedMaiaEvent::Progress(phase));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandOutcome {
    Completed,
    Cancelled,
}

fn run_logged_command(
    command: &mut Command,
    environment: &[(OsString, OsString)],
    log_path: &Path,
    controls: &Receiver<InstallerControl>,
    operation: &str,
) -> io::Result<CommandOutcome> {
    if cancelled(controls) {
        return Ok(CommandOutcome::Cancelled);
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln_to_log(&log, &format!("\n-- {operation} --\n"))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .envs(environment.iter().cloned())
        .env_remove("VIRTUAL_ENV")
        .env_remove("CONDA_PREFIX");
    configure_process_group(command);

    let display_program = command.get_program().to_string_lossy().into_owned();
    let mut child = command.spawn().map_err(|error| {
        let message = if error.kind() == io::ErrorKind::NotFound
            && matches!(
                operation,
                "create the private Python environment" | "install Maia-3"
            )
        {
            format!(
                "Could not find uv ({display_program}). Install uv or set CHESS_TUI_UV to its executable, then retry."
            )
        } else {
            format!("could not start {display_program} to {operation}: {error}")
        };
        io::Error::new(error.kind(), message)
    })?;
    loop {
        if cancelled(controls) {
            terminate_child_tree(&mut child);
            return Ok(CommandOutcome::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => return command_result(operation, status, log_path),
            Ok(None) => {}
            Err(error) => {
                terminate_child_tree(&mut child);
                return Err(error);
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn command_result(
    operation: &str,
    status: ExitStatus,
    log_path: &Path,
) -> io::Result<CommandOutcome> {
    if status.success() {
        return Ok(CommandOutcome::Completed);
    }
    let details = read_sanitized_log_tail(log_path).unwrap_or_default();
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!("\n\nInstaller output:\n{details}")
    };
    Err(io::Error::other(format!(
        "Could not {operation} ({status}).{suffix}"
    )))
}

fn cancelled(controls: &Receiver<InstallerControl>) -> bool {
    match controls.try_recv() {
        Ok(InstallerControl::Cancel) | Err(TryRecvError::Disconnected) => true,
        Err(TryRecvError::Empty) => false,
    }
}

fn prepare_cache_directories(config: &ManagedMaiaConfig) -> io::Result<()> {
    for (_, value) in config.process_environment() {
        let path = Path::new(&value);
        if path.is_absolute() {
            fs::create_dir_all(path)?;
        }
    }
    Ok(())
}

fn publish_ready_marker(install_dir: &Path) -> io::Result<()> {
    let temporary = install_dir.join(format!(
        ".ready-{}-{}.tmp",
        std::process::id(),
        UNIQUE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = File::create(&temporary)?;
    {
        use std::io::Write as _;
        file.write_all(READY_MANIFEST.as_bytes())?;
    }
    file.sync_all()?;
    fs::rename(temporary, install_dir.join(READY_MARKER))
}

struct IncompleteInstallGuard {
    path: PathBuf,
    armed: bool,
}

impl IncompleteInstallGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for IncompleteInstallGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn read_sanitized_log_tail(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(MAX_ERROR_LOG_BYTES)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let cleaned: String = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect();
    Ok(cleaned.trim().to_owned())
}

fn writeln_to_log(mut log: &File, text: &str) -> io::Result<()> {
    use std::io::Write as _;
    log.write_all(text.as_bytes())
}

fn path_env(name: &str, path: PathBuf) -> (OsString, OsString) {
    (OsString::from(name), path.into_os_string())
}

fn console_script(venv: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        venv.join("Scripts").join(format!("{name}.exe"))
    }
    #[cfg(not(windows))]
    {
        venv.join("bin").join(name)
    }
}

fn executable_file(path: &Path) -> io::Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        Ok(true)
    }
}

fn absolute_path(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn default_data_root() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("LOCALAPPDATA")
            .or_else(|| env::var_os("APPDATA"))
            .map(PathBuf::from)
            .map(|root| root.join("chess-tui"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is not set"))
    }
    #[cfg(target_os = "macos")]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| {
                home.join("Library")
                    .join("Application Support")
                    .join("chess-tui")
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = env::var_os("XDG_DATA_HOME").filter(|root| !root.is_empty()) {
            return Ok(PathBuf::from(root).join("chess-tui"));
        }
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".local").join("share").join("chess-tui"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed Maia installation is unsupported on this platform",
        ))
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_child_tree(child: &mut Child) {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };

    let Some(group) = i32::try_from(child.id()).ok().map(Pid::from_raw) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let _ = killpg(group, Signal::SIGTERM);
    if wait_for_child(child, TERMINATE_GRACE) {
        return;
    }
    let _ = killpg(group, Signal::SIGKILL);
    let _ = wait_for_child(child, TERMINATE_GRACE);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate_child_tree(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(any(unix, windows)))]
fn terminate_child_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) | Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc::RecvTimeoutError, time::Duration};

    fn temp_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "chess-managed-maia-{label}-{}-{}",
            std::process::id(),
            UNIQUE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn runtime_paths_and_environment_are_private_and_versioned() {
        let root = temp_dir("paths");
        let config = ManagedMaiaConfig::in_data_root(&root);
        let runtime = config.installation();
        assert!(runtime.install_dir().ends_with(MAIA3_COMMIT));
        assert!(runtime.command().starts_with(runtime.install_dir()));
        assert_eq!(
            runtime.args(),
            vec![
                OsString::from("--model"),
                OsString::from(MAIA3_MODEL),
                OsString::from("--cache-dir"),
                runtime.model_cache_dir().into_os_string(),
                OsString::from("--revision"),
                OsString::from(MAIA3_MODEL_REVISION),
                OsString::from("--local-files-only"),
                OsString::from("--use-uci-history"),
            ]
        );
        for (_, value) in runtime.environment() {
            if value != "1" {
                assert!(Path::new(value).starts_with(&root));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn fake_uv(root: &Path, fail_marker: Option<&Path>, slow_marker: Option<&Path>) -> PathBuf {
        let uv = root.join("fake uv");
        let fail = fail_marker
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let slow = slow_marker
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let script = format!(
            r#"#!/bin/sh
set -eu
printf '%s|%s|%s\n' "$*" "$UV_CACHE_DIR" "$HF_HOME" >> "{log}"
if [ "$1" = "venv" ]; then
  if [ -n "{slow}" ] && [ -f "{slow}" ]; then sleep 30; fi
  venv="$4"
  mkdir -p "$venv/bin"
  printf '#!/bin/sh\necho "$*" >> "{log}"\necho "$UV_CACHE_DIR" >> "{log}"\necho "$HF_HOME" >> "{log}"\nexit 0\n' > "$venv/bin/maia3-cache"
  printf '#!/bin/sh\nexit 0\n' > "$venv/bin/maia3-uci"
  chmod +x "$venv/bin/maia3-cache" "$venv/bin/maia3-uci"
elif [ "$1" = "pip" ] && [ -n "{fail}" ] && [ -f "{fail}" ]; then
  rm -f "{fail}"
  printf 'simulated package failure\n' >&2
  exit 42
fi
"#,
            log = root.join("commands.log").display(),
            fail = fail,
            slow = slow,
        );
        write_executable(&uv, &script);
        uv
    }

    fn recv_terminal_event(installer: &ManagedMaiaInstaller) -> ManagedMaiaEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match installer.events.recv_timeout(Duration::from_millis(100)) {
                Ok(
                    event @ (ManagedMaiaEvent::Ready(_)
                    | ManagedMaiaEvent::Failed { .. }
                    | ManagedMaiaEvent::Cancelled),
                ) => return event,
                Ok(ManagedMaiaEvent::Progress(_)) => {}
                Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(error) => panic!("installer stopped without a terminal event: {error}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn fake_uv_install_publishes_readiness_and_uses_pinned_private_resources() {
        let root = temp_dir("success with spaces");
        let uv = fake_uv(&root, None, None);
        let config = ManagedMaiaConfig::in_data_root(&root).with_uv_program(uv);
        let installer = ManagedMaiaInstaller::start_with_config(config.clone()).unwrap();
        let ManagedMaiaEvent::Ready(runtime) = recv_terminal_event(&installer) else {
            panic!("expected ready event")
        };
        assert_eq!(config.discover().unwrap(), Some(runtime.clone()));
        assert!(runtime.command().is_file());
        assert!(runtime.install_dir().join(READY_MARKER).is_file());
        assert_eq!(
            fs::read_dir(runtime.install_dir().parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with(".ready-") && name.ends_with(".tmp")
                })
                .count(),
            0
        );
        let log = fs::read_to_string(root.join("commands.log")).unwrap();
        assert!(log.contains("venv --python 3.12"));
        assert!(log.contains(MAIA3_ARCHIVE_URL));
        assert!(log.contains("--model maia3-5m --cache-dir"));
        assert!(log.contains(MAIA3_MODEL_REVISION));
        assert!(log.contains(&root.join("cache/uv").display().to_string()));
        assert!(log.contains(&root.join("cache/huggingface").display().to_string()));
        drop(installer);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_install_cleans_incomplete_version_and_can_retry() {
        let root = temp_dir("retry");
        let marker = root.join("fail-once");
        fs::write(&marker, "fail").unwrap();
        let uv = fake_uv(&root, Some(&marker), None);
        let config = ManagedMaiaConfig::in_data_root(&root).with_uv_program(uv);
        let installer = ManagedMaiaInstaller::start_with_config(config.clone()).unwrap();
        let ManagedMaiaEvent::Failed { message } = recv_terminal_event(&installer) else {
            panic!("expected failed event")
        };
        assert!(message.contains("simulated package failure"));
        assert!(config.discover().unwrap().is_none());

        let retried = installer.retry().unwrap();
        assert!(matches!(
            recv_terminal_event(&retried),
            ManagedMaiaEvent::Ready(_)
        ));
        drop(retried);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn missing_uv_reports_the_single_action_needed_to_retry() {
        let root = temp_dir("missing-uv");
        let config =
            ManagedMaiaConfig::in_data_root(&root).with_uv_program(root.join("uv-does-not-exist"));
        let installer = ManagedMaiaInstaller::start_with_config(config.clone()).unwrap();
        let ManagedMaiaEvent::Failed { message } = recv_terminal_event(&installer) else {
            panic!("expected failed event")
        };

        assert!(message.contains("Install uv or set CHESS_TUI_UV"));
        assert!(config.discover().unwrap().is_none());
        drop(installer);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_the_process_group_and_returns_promptly() {
        let root = temp_dir("cancel");
        let marker = root.join("slow");
        fs::write(&marker, "slow").unwrap();
        let uv = fake_uv(&root, None, Some(&marker));
        let config = ManagedMaiaConfig::in_data_root(&root).with_uv_program(uv);
        let installer = ManagedMaiaInstaller::start_with_config(config.clone()).unwrap();
        assert_eq!(
            installer
                .events
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            ManagedMaiaEvent::Progress(ManagedMaiaPhase::Preparing)
        );
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        installer.cancel().unwrap();
        assert_eq!(recv_terminal_event(&installer), ManagedMaiaEvent::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(config.discover().unwrap().is_none());
        drop(installer);
        fs::remove_dir_all(root).unwrap();
    }
}
