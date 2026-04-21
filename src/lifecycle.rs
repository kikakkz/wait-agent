use crate::cli::{
    AttachCommand, DaemonCommand, DetachCommand, ListCommand, StatusCommand, WorkspaceCommand,
};
use crate::config::AppConfig;
use crate::pty::{PtyHandle, PtyManager, PtySize, SpawnRequest, PTY_EOF_ERRNO};
use crate::session::SessionAddress;
use crate::terminal::{
    ScreenSnapshot, TerminalEngine, TerminalError, TerminalRuntime, TerminalSize,
};
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::raw::c_int;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);
const CLIENT_TICK: Duration = Duration::from_millis(50);
const RESET_FRAME_CURSOR: &str = "\x1b[H";
const WORKSPACE_STATUS_ROWS: usize = 4;
const FRAME_START: &[u8] = b"\x1b[H";
const ROW_ONE_START: &[u8] = b"\x1b[1;1H";
const CLEAR_TO_LINE_END: &[u8] = b"\x1b[K";

extern "C" {
    fn setsid() -> c_int;
}

pub fn run_workspace_entry(
    config: AppConfig,
    command: WorkspaceCommand,
) -> Result<(), LifecycleError> {
    let runtime =
        config.runtime_for_workspace(command.node_id.as_deref(), command.connect.as_deref());
    let mut terminal = TerminalRuntime::stdio();
    let snapshot = terminal.snapshot()?;
    if !snapshot.input_is_tty || !snapshot.output_is_tty {
        return Err(LifecycleError::Terminal(TerminalError::NotTty(
            "workspace console".to_string(),
        )));
    }

    let workspace_dir = resolve_workspace_dir(None)?;
    let paths = WorkspacePaths::from_dir(&workspace_dir);
    ensure_daemon_running(&runtime, &paths, snapshot.size)?;
    if !wait_for_existing_daemon_ready(&paths, DAEMON_START_TIMEOUT, true) {
        return Err(LifecycleError::Protocol(format!(
            "waitagent daemon did not become ready at {}",
            paths.socket_path.display()
        )));
    }
    run_attach_client(&paths, snapshot.size)
}

pub fn run_daemon(config: AppConfig, command: DaemonCommand) -> Result<(), LifecycleError> {
    let runtime =
        config.runtime_for_workspace(command.node_id.as_deref(), command.connect.as_deref());
    let workspace_dir = resolve_workspace_dir(command.workspace_dir.as_deref())?;
    let paths = WorkspacePaths::from_dir(&workspace_dir);
    let size = TerminalSize {
        rows: command.rows.unwrap_or(24),
        cols: command.cols.unwrap_or(80),
        pixel_width: command.pixel_width.unwrap_or(0),
        pixel_height: command.pixel_height.unwrap_or(0),
    };

    DaemonRuntime::start(&runtime, workspace_dir, paths, size)?.run()
}

pub fn run_attach(command: AttachCommand) -> Result<(), LifecycleError> {
    let mut terminal = TerminalRuntime::stdio();
    let snapshot = terminal.snapshot()?;
    if !snapshot.input_is_tty || !snapshot.output_is_tty {
        return Err(LifecycleError::Terminal(TerminalError::NotTty(
            "attach console".to_string(),
        )));
    }

    let workspace_dir = resolve_workspace_dir(command.workspace_dir.as_deref())?;
    let paths = WorkspacePaths::from_dir(&workspace_dir);
    wait_for_existing_daemon_ready(&paths, DAEMON_START_TIMEOUT, true);
    run_attach_client(&paths, snapshot.size)
}

pub fn run_list(_command: ListCommand) -> Result<(), LifecycleError> {
    let runtime_dir = runtime_root_dir();
    if !runtime_dir.exists() {
        println!("no waitagent daemons running");
        return Ok(());
    }

    let mut daemons = Vec::new();
    for entry in fs::read_dir(&runtime_dir).map_err(|error| {
        LifecycleError::Io(
            format!(
                "failed to read waitagent runtime directory {}",
                runtime_dir.display()
            ),
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            LifecycleError::Io("failed to read waitagent runtime entry".to_string(), error)
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sock") {
            continue;
        }

        let Some(status) = query_status_by_socket(&path)? else {
            continue;
        };
        if let Some(parsed) = parse_status_fields(&status, path) {
            daemons.push(parsed);
        }
    }

    if daemons.is_empty() {
        println!("no waitagent daemons running");
        return Ok(());
    }

    daemons.sort_by(|left, right| left.workspace_dir.cmp(&right.workspace_dir));
    for daemon in daemons {
        println!(
            "{}: {} | node={} | ready={} | attached={} | pid={} | size={}x{} | socket={}",
            daemon.key,
            daemon.workspace_dir.display(),
            daemon.node_id,
            if daemon.ready { "yes" } else { "no" },
            daemon.attached_clients,
            daemon.child_pid,
            daemon.cols,
            daemon.rows,
            daemon.socket_path.display()
        );
    }

    Ok(())
}

pub fn run_status(command: StatusCommand) -> Result<(), LifecycleError> {
    let workspace_dir = resolve_workspace_dir(command.workspace_dir.as_deref())?;
    let paths = WorkspacePaths::from_dir(&workspace_dir);
    match UnixStream::connect(&paths.socket_path) {
        Ok(mut stream) => {
            write_frame(&mut stream, &Frame::StatusRequest)?;
            match read_frame(&mut stream)? {
                Frame::StatusResponse(text) | Frame::Ack(text) => {
                    println!("{text}");
                    Ok(())
                }
                Frame::Error(message) => {
                    println!("{message}");
                    Ok(())
                }
                other => Err(LifecycleError::Protocol(format!(
                    "unexpected status response: {:?}",
                    other
                ))),
            }
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::ConnectionRefused =>
        {
            println!(
                "waitagent daemon not running for {}\nsocket: {}",
                workspace_dir.display(),
                paths.socket_path.display()
            );
            Ok(())
        }
        Err(error) => Err(LifecycleError::Io(
            "failed to connect to daemon".to_string(),
            error,
        )),
    }
}

pub fn run_detach(command: DetachCommand) -> Result<(), LifecycleError> {
    let workspace_dir = resolve_workspace_dir(command.workspace_dir.as_deref())?;
    let paths = WorkspacePaths::from_dir(&workspace_dir);
    match UnixStream::connect(&paths.socket_path) {
        Ok(mut stream) => {
            write_frame(&mut stream, &Frame::DetachRequest)?;
            match read_frame(&mut stream)? {
                Frame::Ack(text) | Frame::StatusResponse(text) | Frame::Error(text) => {
                    println!("{text}");
                    Ok(())
                }
                other => Err(LifecycleError::Protocol(format!(
                    "unexpected detach response: {:?}",
                    other
                ))),
            }
        }
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::ConnectionRefused =>
        {
            println!(
                "waitagent daemon not running for {}\nsocket: {}",
                workspace_dir.display(),
                paths.socket_path.display()
            );
            Ok(())
        }
        Err(error) => Err(LifecycleError::Io(
            "failed to connect to daemon".to_string(),
            error,
        )),
    }
}

#[derive(Debug)]
struct WorkspacePaths {
    workspace_dir: PathBuf,
    socket_path: PathBuf,
}

impl WorkspacePaths {
    fn from_dir(workspace_dir: &Path) -> Self {
        let key = stable_workspace_key(workspace_dir);
        let base_dir = runtime_root_dir();
        let socket_path = base_dir.join(format!("{key}.sock"));
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            socket_path,
        }
    }
}

fn runtime_root_dir() -> PathBuf {
    env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("waitagent")
}

fn resolve_workspace_dir(value: Option<&str>) -> Result<PathBuf, LifecycleError> {
    let dir = match value {
        Some(path) => PathBuf::from(path),
        None => env::current_dir().map_err(|error| {
            LifecycleError::Io("failed to read current directory".to_string(), error)
        })?,
    };
    dir.canonicalize().map_err(|error| {
        LifecycleError::Io(
            "failed to canonicalize workspace directory".to_string(),
            error,
        )
    })
}

fn stable_workspace_key(path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    let normalized = path.to_string_lossy();
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn ensure_daemon_running(
    runtime: &AppConfig,
    paths: &WorkspacePaths,
    size: TerminalSize,
) -> Result<(), LifecycleError> {
    if daemon_accepts_connections(paths) {
        return Ok(());
    }

    if paths.socket_path.exists() {
        let _ = fs::remove_file(&paths.socket_path);
    }

    if let Some(parent) = paths.socket_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            LifecycleError::Io(
                "failed to create waitagent runtime directory".to_string(),
                error,
            )
        })?;
    }

    let current_exe = env::current_exe().map_err(|error| {
        LifecycleError::Io("failed to locate current executable".to_string(), error)
    })?;
    let mut command = Command::new(current_exe);
    command
        .arg("daemon")
        .arg("--workspace-dir")
        .arg(&paths.workspace_dir)
        .arg("--rows")
        .arg(size.rows.to_string())
        .arg("--cols")
        .arg(size.cols.to_string())
        .arg("--pixel-width")
        .arg(size.pixel_width.to_string())
        .arg("--pixel-height")
        .arg(size.pixel_height.to_string())
        .arg("--node-id")
        .arg(&runtime.node.node_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir(&paths.workspace_dir);

    if let Some(connect) = runtime.network.access_point.as_deref() {
        command.arg("--connect").arg(connect);
    }

    unsafe {
        command.pre_exec(|| {
            if setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    command.spawn().map_err(|error| {
        LifecycleError::Io("failed to spawn waitagent daemon".to_string(), error)
    })?;

    if wait_for_existing_daemon_socket(paths, DAEMON_START_TIMEOUT, true) {
        return Ok(());
    }

    Err(LifecycleError::Protocol(format!(
        "waitagent daemon did not become reachable at {}",
        paths.socket_path.display()
    )))
}

fn daemon_is_reachable(paths: &WorkspacePaths) -> bool {
    let Ok(mut stream) = UnixStream::connect(&paths.socket_path) else {
        return false;
    };

    if write_frame(&mut stream, &Frame::StatusRequest).is_err() {
        return false;
    }

    match read_frame(&mut stream) {
        Ok(Frame::StatusResponse(status)) | Ok(Frame::Ack(status)) => daemon_status_ready(&status),
        _ => false,
    }
}

fn daemon_accepts_connections(paths: &WorkspacePaths) -> bool {
    UnixStream::connect(&paths.socket_path).is_ok()
}

fn wait_for_existing_daemon_ready(
    paths: &WorkspacePaths,
    timeout: Duration,
    wait_for_socket_creation: bool,
) -> bool {
    let started_at = Instant::now();
    let mut saw_socket = paths.socket_path.exists();

    while started_at.elapsed() < timeout {
        if daemon_is_reachable(paths) {
            return true;
        }
        if paths.socket_path.exists() {
            saw_socket = true;
        } else if !saw_socket && !wait_for_socket_creation {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }

    false
}

fn wait_for_existing_daemon_socket(
    paths: &WorkspacePaths,
    timeout: Duration,
    wait_for_socket_creation: bool,
) -> bool {
    let started_at = Instant::now();
    let mut saw_socket = paths.socket_path.exists();

    while started_at.elapsed() < timeout {
        if daemon_accepts_connections(paths) {
            return true;
        }
        if paths.socket_path.exists() {
            saw_socket = true;
        } else if !saw_socket && !wait_for_socket_creation {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }

    false
}

fn daemon_status_ready(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("ready: "))
        .map(|value| value == "yes")
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct DaemonStatusRecord {
    key: String,
    workspace_dir: PathBuf,
    socket_path: PathBuf,
    node_id: String,
    child_pid: String,
    ready: bool,
    attached_clients: usize,
    rows: u16,
    cols: u16,
}

fn query_status_by_socket(socket_path: &Path) -> Result<Option<String>, LifecycleError> {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(stream) => stream,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.kind() == io::ErrorKind::ConnectionRefused =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(LifecycleError::Io(
                format!(
                    "failed to connect to waitagent daemon at {}",
                    socket_path.display()
                ),
                error,
            ));
        }
    };

    write_frame(&mut stream, &Frame::StatusRequest)?;
    match read_frame(&mut stream)? {
        Frame::StatusResponse(status) | Frame::Ack(status) => Ok(Some(status)),
        Frame::Error(message) => Err(LifecycleError::Protocol(message)),
        other => Err(LifecycleError::Protocol(format!(
            "unexpected status response from {}: {:?}",
            socket_path.display(),
            other
        ))),
    }
}

fn parse_status_fields(status: &str, socket_path: PathBuf) -> Option<DaemonStatusRecord> {
    let mut fields = HashMap::<String, String>::new();
    for line in status.lines() {
        let (key, value) = line.split_once(": ")?;
        fields.insert(key.to_string(), value.to_string());
    }

    let workspace_dir = PathBuf::from(fields.get("workspace")?.clone());
    let key = fields
        .get("key")
        .cloned()
        .unwrap_or_else(|| stable_workspace_key(&workspace_dir));
    let node_id = fields.get("node")?.clone();
    let child_pid = fields.get("child_pid")?.clone();
    let ready = fields.get("ready").map(|value| value == "yes")?;
    let attached_clients = fields.get("attached_clients")?.parse::<usize>().ok()?;
    let (rows, cols) = parse_screen_size(fields.get("screen_size")?)?;

    Some(DaemonStatusRecord {
        key,
        workspace_dir,
        socket_path,
        node_id,
        child_pid,
        ready,
        attached_clients,
        rows,
        cols,
    })
}

fn parse_screen_size(value: &str) -> Option<(u16, u16)> {
    let (rows, cols) = value.split_once('x')?;
    Some((rows.parse().ok()?, cols.parse().ok()?))
}

fn workspace_snapshot_ready(snapshot: &ScreenSnapshot) -> bool {
    workspace_snapshot_has_visible_workspace(snapshot) && workspace_snapshot_has_chrome(snapshot)
}

fn workspace_snapshot_has_visible_workspace(snapshot: &ScreenSnapshot) -> bool {
    let work_rows = snapshot.lines.len().saturating_sub(WORKSPACE_STATUS_ROWS);
    snapshot.lines[..work_rows]
        .iter()
        .map(|line| visible_workspace_main_text(line))
        .any(line_has_visible_workspace_content)
}

fn workspace_snapshot_has_chrome(snapshot: &ScreenSnapshot) -> bool {
    let mut saw_keys = false;
    let mut saw_status = false;

    for line in &snapshot.lines {
        let visible = visible_workspace_main_text(line).trim();
        if visible.starts_with("keys:") {
            saw_keys = true;
        } else if visible.starts_with("WaitAgent |") {
            saw_status = true;
        }
    }

    saw_keys && saw_status
}

fn looks_like_full_frame(bytes: &[u8]) -> bool {
    let Some(frame_start) = full_frame_start_offset(bytes) else {
        return false;
    };

    bytes[frame_start..]
        .windows(ROW_ONE_START.len())
        .any(|window| window == ROW_ONE_START)
}

fn full_frame_start_offset(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(FRAME_START.len())
        .position(|window| window == FRAME_START)
}

#[cfg(test)]
fn frame_has_visible_first_line(bytes: &[u8]) -> bool {
    let Some(start) = bytes
        .windows(ROW_ONE_START.len())
        .position(|window| window == ROW_ONE_START)
    else {
        return false;
    };
    let content_start = start + ROW_ONE_START.len();
    let content_end = bytes[content_start..]
        .windows(CLEAR_TO_LINE_END.len())
        .position(|window| window == CLEAR_TO_LINE_END)
        .map(|offset| content_start + offset)
        .unwrap_or(bytes.len());
    ansi_visible_text(visible_workspace_main_bytes(
        &bytes[content_start..content_end],
    ))
    .chars()
    .any(|ch| !ch.is_whitespace())
}

fn visible_workspace_main_text(line: &str) -> &str {
    line.split_once('┃').map(|(main, _)| main).unwrap_or(line)
}

fn visible_workspace_main_bytes(bytes: &[u8]) -> &[u8] {
    let divider = "┃".as_bytes();
    bytes
        .windows(divider.len())
        .position(|window| window == divider)
        .map(|index| &bytes[..index])
        .unwrap_or(bytes)
}

fn attach_frame_has_visible_workspace(bytes: &[u8]) -> bool {
    if looks_like_full_frame(bytes) {
        return full_frame_has_visible_workspace(bytes);
    }

    ansi_visible_text(visible_workspace_main_bytes(bytes))
        .chars()
        .any(|ch| !ch.is_whitespace())
}

fn full_frame_has_visible_workspace(bytes: &[u8]) -> bool {
    let mut saw_visible_workspace = false;

    for visible in full_frame_main_lines(bytes) {
        if line_has_visible_workspace_content(&visible) {
            saw_visible_workspace = true;
            break;
        }
    }

    saw_visible_workspace
}

fn full_frame_has_chrome(bytes: &[u8]) -> bool {
    let mut saw_keys = false;
    let mut saw_status = false;

    for visible in full_frame_main_lines(bytes) {
        let trimmed = visible.trim();
        if trimmed.starts_with("keys:") {
            saw_keys = true;
        } else if trimmed.starts_with("WaitAgent |") {
            saw_status = true;
        }
    }

    saw_keys && saw_status
}

fn full_frame_main_lines(bytes: &[u8]) -> Vec<String> {
    let mut index = 0;
    let mut lines = Vec::new();

    while index < bytes.len() {
        let Some(row_start_offset) = bytes[index..]
            .windows(2)
            .position(|window| window == b"\x1b[")
        else {
            break;
        };
        index += row_start_offset + 2;

        let row_digits_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if row_digits_start == index || index + 2 >= bytes.len() {
            continue;
        }
        if bytes[index] != b';' || bytes[index + 1] != b'1' || bytes[index + 2] != b'H' {
            continue;
        }
        index += 3;

        let content_start = index;
        let Some(clear_offset) = bytes[index..]
            .windows(CLEAR_TO_LINE_END.len())
            .position(|window| window == CLEAR_TO_LINE_END)
        else {
            break;
        };
        let content_end = index + clear_offset;
        index = content_end + CLEAR_TO_LINE_END.len();

        lines.push(ansi_visible_text(visible_workspace_main_bytes(
            &bytes[content_start..content_end],
        )));
    }

    lines
}

fn line_has_visible_workspace_content(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with("keys:")
        && !trimmed.starts_with("WaitAgent |")
        && !trimmed.chars().all(|ch| ch == '━')
}

fn ansi_visible_text(bytes: &[u8]) -> String {
    let mut visible = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                if index + 1 >= bytes.len() {
                    break;
                }
                match bytes[index + 1] {
                    b'[' => {
                        index += 2;
                        while index < bytes.len() {
                            let byte = bytes[index];
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    b']' => {
                        index += 2;
                        while index < bytes.len() {
                            match bytes[index] {
                                0x07 => {
                                    index += 1;
                                    break;
                                }
                                0x1b if index + 1 < bytes.len() && bytes[index + 1] == b'\\' => {
                                    index += 2;
                                    break;
                                }
                                _ => index += 1,
                            }
                        }
                    }
                    _ => index += 2,
                }
            }
            byte => {
                visible.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&visible).into_owned()
}

fn run_attach_client(paths: &WorkspacePaths, size: TerminalSize) -> Result<(), LifecycleError> {
    let mut stream = UnixStream::connect(&paths.socket_path).map_err(|error| {
        LifecycleError::Io(
            format!(
                "failed to connect to waitagent daemon at {}",
                paths.socket_path.display()
            ),
            error,
        )
    })?;
    write_frame(&mut stream, &Frame::Attach(size.into()))?;

    let terminal = TerminalRuntime::stdio();
    let _alternate_screen = terminal.enter_alternate_screen()?;
    let _raw_mode = terminal.enter_raw_mode()?;

    let (tx, rx) = mpsc::channel();
    spawn_attach_stdin_reader(tx.clone());
    spawn_attach_socket_reader(
        stream.try_clone().map_err(|error| {
            LifecycleError::Io("failed to clone daemon socket".to_string(), error)
        })?,
        tx,
    );

    let mut terminal_runtime = TerminalRuntime::stdio();
    let mut writer = stream;
    let mut stdout = io::stdout().lock();
    let mut first_message_seen = false;
    let mut workspace_visible = false;
    let mut requested_startup_refresh = false;

    loop {
        match rx.recv_timeout(CLIENT_TICK) {
            Ok(AttachClientEvent::Input(bytes)) => {
                write_frame(&mut writer, &Frame::Input(bytes))?;
            }
            Ok(AttachClientEvent::Socket(frame)) => match frame {
                Frame::Ack(_) => {
                    first_message_seen = true;
                }
                Frame::Snapshot(bytes) | Frame::Output(bytes) => {
                    first_message_seen = true;
                    if !workspace_visible {
                        if !attach_frame_has_visible_workspace(&bytes) {
                            if !requested_startup_refresh {
                                request_attach_startup_refresh(&mut writer, size)?;
                                requested_startup_refresh = true;
                            }
                            continue;
                        }
                        workspace_visible = true;
                    }
                    stdout.write_all(&bytes).map_err(|error| {
                        LifecycleError::Io("failed to write attach output".to_string(), error)
                    })?;
                    stdout.flush().map_err(|error| {
                        LifecycleError::Io("failed to flush attach output".to_string(), error)
                    })?;
                }
                Frame::Error(message) => {
                    return Err(LifecycleError::Protocol(message));
                }
                Frame::StatusResponse(_) => {}
                _ => {
                    return Err(LifecycleError::Protocol(format!(
                        "unexpected attach frame: {:?}",
                        frame
                    )));
                }
            },
            Ok(AttachClientEvent::SocketClosed) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if first_message_seen && !workspace_visible && !requested_startup_refresh {
                    request_attach_startup_refresh(&mut writer, size)?;
                    requested_startup_refresh = true;
                }
                if first_message_seen {
                    if let Some(resized) = terminal_runtime.capture_resize()? {
                        write_frame(&mut writer, &Frame::Resize(resized.into()))?;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

fn request_attach_startup_refresh(
    writer: &mut UnixStream,
    size: TerminalSize,
) -> Result<(), LifecycleError> {
    let mut bumped = size;
    if bumped.rows < u16::MAX {
        bumped.rows += 1;
    } else if bumped.cols < u16::MAX {
        bumped.cols += 1;
    }
    write_frame(writer, &Frame::Resize(bumped.into()))?;
    write_frame(writer, &Frame::Resize(size.into()))?;
    write_frame(writer, &Frame::SnapshotRequest)
}

struct DaemonRuntime {
    runtime: AppConfig,
    workspace_dir: PathBuf,
    paths: WorkspacePaths,
    pty: PtyHandle,
    engine: TerminalEngine,
    workspace_ready: bool,
    latest_frame_bytes: Option<Vec<u8>>,
    attached_clients: HashMap<u64, AttachedClient>,
    next_client_id: u64,
    rx: Receiver<DaemonEvent>,
    tx: Sender<DaemonEvent>,
    initial_size: TerminalSize,
}

impl DaemonRuntime {
    fn start(
        runtime: &AppConfig,
        workspace_dir: PathBuf,
        paths: WorkspacePaths,
        size: TerminalSize,
    ) -> Result<Self, LifecycleError> {
        if let Some(parent) = paths.socket_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                LifecycleError::Io(
                    "failed to create waitagent runtime directory".to_string(),
                    error,
                )
            })?;
        }
        if paths.socket_path.exists() {
            let _ = fs::remove_file(&paths.socket_path);
        }

        let listener = UnixListener::bind(&paths.socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to bind waitagent daemon socket at {}",
                    paths.socket_path.display()
                ),
                error,
            )
        })?;

        let mut pty_manager = PtyManager::new();
        let current_exe = env::current_exe().map_err(|error| {
            LifecycleError::Io("failed to locate current executable".to_string(), error)
        })?;
        let mut args = vec![
            "__workspace-internal".to_string(),
            "--node-id".to_string(),
            runtime.node.node_id.clone(),
        ];
        if let Some(connect) = runtime.network.access_point.as_deref() {
            args.push("--connect".to_string());
            args.push(connect.to_string());
        }

        let pty = pty_manager.spawn(
            SessionAddress::new("local", "workspace-runtime"),
            SpawnRequest {
                program: current_exe.to_string_lossy().into_owned(),
                args,
                size: size.into(),
            },
        )?;

        let engine = TerminalEngine::new(size);
        let (tx, rx) = mpsc::channel();

        spawn_daemon_listener(listener, tx.clone());
        spawn_daemon_pty_reader(
            pty.try_clone_reader().map_err(LifecycleError::Pty)?,
            tx.clone(),
        );

        Ok(Self {
            runtime: runtime.clone(),
            workspace_dir,
            paths,
            pty,
            engine,
            workspace_ready: false,
            latest_frame_bytes: None,
            attached_clients: HashMap::new(),
            next_client_id: 0,
            rx,
            tx,
            initial_size: size,
        })
    }

    fn run(mut self) -> Result<(), LifecycleError> {
        let _socket_guard = SocketGuard {
            path: self.paths.socket_path.clone(),
        };

        while let Ok(event) = self.rx.recv() {
            match event {
                DaemonEvent::PtyOutput(bytes) => {
                    self.engine.feed(&bytes);
                    if looks_like_full_frame(&bytes) {
                        let frame_ready = full_frame_has_visible_workspace(&bytes)
                            && full_frame_has_chrome(&bytes);
                        self.workspace_ready |= frame_ready;
                        if frame_ready {
                            self.latest_frame_bytes = Some(bytes.clone());
                        }
                    } else {
                        let snapshot_ready = workspace_snapshot_ready(&self.engine.snapshot());
                        self.workspace_ready |= snapshot_ready;
                    }
                    self.broadcast_frame(Frame::Output(bytes));
                }
                DaemonEvent::PtyClosed => {
                    self.attached_clients.clear();
                    break;
                }
                DaemonEvent::Incoming(stream) => {
                    self.handle_incoming(stream)?;
                }
                DaemonEvent::ClientFrame(client_id, frame) => {
                    if !self.attached_clients.contains_key(&client_id) {
                        continue;
                    }
                    match frame {
                        Frame::Input(bytes) => {
                            self.pty.write_all(&bytes)?;
                        }
                        Frame::Resize(size) => {
                            self.pty.resize(size)?;
                            self.engine.resize(size.into());
                        }
                        Frame::SnapshotRequest => {
                            if let Some(client) = self.attached_clients.get_mut(&client_id) {
                                let snapshot_bytes =
                                    self.latest_frame_bytes.clone().unwrap_or_else(|| {
                                        render_snapshot_bytes(&self.engine.snapshot()).into_bytes()
                                    });
                                write_frame(&mut client.stream, &Frame::Snapshot(snapshot_bytes))?;
                            }
                        }
                        _ => {}
                    }
                }
                DaemonEvent::ClientDisconnected(client_id) => {
                    self.attached_clients.remove(&client_id);
                }
            }
        }

        Ok(())
    }

    fn handle_incoming(&mut self, mut stream: UnixStream) -> Result<(), LifecycleError> {
        let frame = match read_frame(&mut stream) {
            Ok(frame) => frame,
            Err(LifecycleError::Io(_, error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        match frame {
            Frame::Attach(size) => self.attach_client(stream, size),
            Frame::StatusRequest => {
                let response = self.render_status();
                write_frame(&mut stream, &Frame::StatusResponse(response))?;
                Ok(())
            }
            Frame::DetachRequest => {
                let detached = self.detach_all_clients("detached by external request");
                let message = if detached > 0 {
                    format!("detached {detached} attached client(s)")
                } else {
                    "no attached client".to_string()
                };
                write_frame(&mut stream, &Frame::Ack(message))?;
                Ok(())
            }
            other => {
                write_frame(
                    &mut stream,
                    &Frame::Error(format!("unexpected initial daemon frame: {:?}", other)),
                )?;
                Ok(())
            }
        }
    }

    fn attach_client(
        &mut self,
        mut stream: UnixStream,
        size: PtySize,
    ) -> Result<(), LifecycleError> {
        self.pty.resize(size)?;
        self.engine.resize(size.into());

        self.next_client_id += 1;
        let client_id = self.next_client_id;
        write_frame(&mut stream, &Frame::Ack("attached".to_string()))?;
        let snapshot_bytes = self
            .latest_frame_bytes
            .clone()
            .unwrap_or_else(|| render_snapshot_bytes(&self.engine.snapshot()).into_bytes());
        write_frame(&mut stream, &Frame::Snapshot(snapshot_bytes))?;

        let reader = stream.try_clone().map_err(|error| {
            LifecycleError::Io("failed to clone attached client socket".to_string(), error)
        })?;
        spawn_daemon_client_reader(client_id, reader, self.tx.clone());
        self.attached_clients
            .insert(client_id, AttachedClient { stream });
        Ok(())
    }

    fn broadcast_frame(&mut self, frame: Frame) {
        let mut disconnected = Vec::new();
        for (&client_id, client) in &mut self.attached_clients {
            if write_frame(&mut client.stream, &frame).is_err() {
                disconnected.push(client_id);
            }
        }
        for client_id in disconnected {
            self.attached_clients.remove(&client_id);
        }
    }

    fn detach_all_clients(&mut self, reason: &str) -> usize {
        let mut detached = 0;
        for (_, mut client) in self.attached_clients.drain() {
            let _ = write_frame(&mut client.stream, &Frame::Error(reason.to_string()));
            let _ = client.stream.shutdown(Shutdown::Both);
            detached += 1;
        }
        detached
    }

    fn render_status(&self) -> String {
        let snapshot = self.engine.snapshot();
        let child_pid = self
            .pty
            .process_id()
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!(
            "workspace: {}\nsocket: {}\nkey: {}\nnode: {}\nchild_pid: {}\nready: {}\nattached_clients: {}\nscreen_size: {}x{}\ninitial_size: {}x{}\nalternate_screen: {}",
            self.workspace_dir.display(),
            self.paths.socket_path.display(),
            stable_workspace_key(&self.workspace_dir),
            self.runtime.node.node_id,
            child_pid,
            if self.workspace_ready { "yes" } else { "no" },
            self.attached_clients.len(),
            snapshot.size.rows,
            snapshot.size.cols,
            self.initial_size.rows,
            self.initial_size.cols,
            if snapshot.alternate_screen { "yes" } else { "no" },
        )
    }
}

struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct AttachedClient {
    stream: UnixStream,
}

#[derive(Debug)]
enum DaemonEvent {
    PtyOutput(Vec<u8>),
    PtyClosed,
    Incoming(UnixStream),
    ClientFrame(u64, Frame),
    ClientDisconnected(u64),
}

#[derive(Debug)]
enum AttachClientEvent {
    Input(Vec<u8>),
    Socket(Frame),
    SocketClosed,
}

fn spawn_daemon_listener(listener: UnixListener, tx: Sender<DaemonEvent>) {
    thread::spawn(move || loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if tx.send(DaemonEvent::Incoming(stream)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });
}

fn spawn_daemon_pty_reader(mut reader: File, tx: Sender<DaemonEvent>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(DaemonEvent::PtyClosed);
                    break;
                }
                Ok(count) => {
                    if tx
                        .send(DaemonEvent::PtyOutput(buffer[..count].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(PTY_EOF_ERRNO) => {
                    let _ = tx.send(DaemonEvent::PtyClosed);
                    break;
                }
                Err(_) => {
                    let _ = tx.send(DaemonEvent::PtyClosed);
                    break;
                }
            }
        }
    });
}

fn spawn_daemon_client_reader(client_id: u64, mut stream: UnixStream, tx: Sender<DaemonEvent>) {
    thread::spawn(move || loop {
        match read_frame(&mut stream) {
            Ok(frame) => {
                if tx.send(DaemonEvent::ClientFrame(client_id, frame)).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = tx.send(DaemonEvent::ClientDisconnected(client_id));
                break;
            }
        }
    });
}

fn spawn_attach_stdin_reader(tx: Sender<AttachClientEvent>) {
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        let mut buffer = [0_u8; 4096];
        loop {
            match handle.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if tx
                        .send(AttachClientEvent::Input(buffer[..count].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_attach_socket_reader(mut stream: UnixStream, tx: Sender<AttachClientEvent>) {
    thread::spawn(move || loop {
        match read_frame(&mut stream) {
            Ok(frame) => {
                if tx.send(AttachClientEvent::Socket(frame)).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = tx.send(AttachClientEvent::SocketClosed);
                break;
            }
        }
    });
}

#[derive(Debug, Clone)]
enum Frame {
    Attach(PtySize),
    Input(Vec<u8>),
    Resize(PtySize),
    SnapshotRequest,
    StatusRequest,
    DetachRequest,
    Ack(String),
    Snapshot(Vec<u8>),
    Output(Vec<u8>),
    StatusResponse(String),
    Error(String),
}

const FRAME_ATTACH: u8 = 1;
const FRAME_INPUT: u8 = 2;
const FRAME_RESIZE: u8 = 3;
const FRAME_SNAPSHOT_REQUEST: u8 = 4;
const FRAME_STATUS_REQUEST: u8 = 5;
const FRAME_DETACH_REQUEST: u8 = 6;
const FRAME_ACK: u8 = 101;
const FRAME_SNAPSHOT: u8 = 102;
const FRAME_OUTPUT: u8 = 103;
const FRAME_STATUS_RESPONSE: u8 = 104;
const FRAME_ERROR: u8 = 105;

fn write_frame(stream: &mut UnixStream, frame: &Frame) -> Result<(), LifecycleError> {
    let (tag, payload) = match frame {
        Frame::Attach(size) => (FRAME_ATTACH, encode_size(*size)),
        Frame::Input(bytes) => (FRAME_INPUT, bytes.clone()),
        Frame::Resize(size) => (FRAME_RESIZE, encode_size(*size)),
        Frame::SnapshotRequest => (FRAME_SNAPSHOT_REQUEST, Vec::new()),
        Frame::StatusRequest => (FRAME_STATUS_REQUEST, Vec::new()),
        Frame::DetachRequest => (FRAME_DETACH_REQUEST, Vec::new()),
        Frame::Ack(text) => (FRAME_ACK, text.as_bytes().to_vec()),
        Frame::Snapshot(bytes) => (FRAME_SNAPSHOT, bytes.clone()),
        Frame::Output(bytes) => (FRAME_OUTPUT, bytes.clone()),
        Frame::StatusResponse(text) => (FRAME_STATUS_RESPONSE, text.as_bytes().to_vec()),
        Frame::Error(text) => (FRAME_ERROR, text.as_bytes().to_vec()),
    };

    let mut header = [0_u8; 5];
    header[0] = tag;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header).map_err(|error| {
        LifecycleError::Io("failed to write daemon frame header".to_string(), error)
    })?;
    if !payload.is_empty() {
        stream.write_all(&payload).map_err(|error| {
            LifecycleError::Io("failed to write daemon frame payload".to_string(), error)
        })?;
    }
    stream
        .flush()
        .map_err(|error| LifecycleError::Io("failed to flush daemon frame".to_string(), error))?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<Frame, LifecycleError> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header).map_err(|error| {
        LifecycleError::Io("failed to read daemon frame header".to_string(), error)
    })?;
    let tag = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0_u8; len];
    if len > 0 {
        stream.read_exact(&mut payload).map_err(|error| {
            LifecycleError::Io("failed to read daemon frame payload".to_string(), error)
        })?;
    }

    match tag {
        FRAME_ATTACH => Ok(Frame::Attach(decode_size(&payload)?)),
        FRAME_INPUT => Ok(Frame::Input(payload)),
        FRAME_RESIZE => Ok(Frame::Resize(decode_size(&payload)?)),
        FRAME_SNAPSHOT_REQUEST => Ok(Frame::SnapshotRequest),
        FRAME_STATUS_REQUEST => Ok(Frame::StatusRequest),
        FRAME_DETACH_REQUEST => Ok(Frame::DetachRequest),
        FRAME_ACK => Ok(Frame::Ack(String::from_utf8(payload).map_err(|_| {
            LifecycleError::Protocol("invalid utf-8 in daemon ack".to_string())
        })?)),
        FRAME_SNAPSHOT => Ok(Frame::Snapshot(payload)),
        FRAME_OUTPUT => Ok(Frame::Output(payload)),
        FRAME_STATUS_RESPONSE => Ok(Frame::StatusResponse(String::from_utf8(payload).map_err(
            |_| LifecycleError::Protocol("invalid utf-8 in daemon status response".to_string()),
        )?)),
        FRAME_ERROR => Ok(Frame::Error(String::from_utf8(payload).map_err(|_| {
            LifecycleError::Protocol("invalid utf-8 in daemon error".to_string())
        })?)),
        other => Err(LifecycleError::Protocol(format!(
            "unknown daemon frame tag: {other}"
        ))),
    }
}

fn encode_size(size: PtySize) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&size.rows.to_be_bytes());
    payload.extend_from_slice(&size.cols.to_be_bytes());
    payload.extend_from_slice(&size.pixel_width.to_be_bytes());
    payload.extend_from_slice(&size.pixel_height.to_be_bytes());
    payload
}

fn decode_size(bytes: &[u8]) -> Result<PtySize, LifecycleError> {
    if bytes.len() != 8 {
        return Err(LifecycleError::Protocol(format!(
            "invalid size payload length: {}",
            bytes.len()
        )));
    }
    Ok(PtySize {
        rows: u16::from_be_bytes([bytes[0], bytes[1]]),
        cols: u16::from_be_bytes([bytes[2], bytes[3]]),
        pixel_width: u16::from_be_bytes([bytes[4], bytes[5]]),
        pixel_height: u16::from_be_bytes([bytes[6], bytes[7]]),
    })
}

fn render_snapshot_bytes(snapshot: &ScreenSnapshot) -> String {
    let mut buffer = String::from(RESET_FRAME_CURSOR);
    for (index, line) in snapshot.styled_lines.iter().enumerate() {
        let row = index.saturating_add(1);
        buffer.push_str(&format!("\x1b[{row};1H{line}\x1b[0m\x1b[K"));
    }

    for row in snapshot.styled_lines.len().saturating_add(1)..=snapshot.size.rows as usize {
        buffer.push_str(&format!("\x1b[{row};1H\x1b[K"));
    }

    let cursor_row = snapshot.cursor_row.saturating_add(1);
    let cursor_col = snapshot.cursor_col.saturating_add(1);
    let cursor_visibility = if snapshot.cursor_visible {
        "\x1b[?25h"
    } else {
        "\x1b[?25l"
    };
    let scroll_region = if snapshot.scroll_top == 0
        && snapshot.scroll_bottom.saturating_add(1) == snapshot.size.rows
    {
        "\x1b[r".to_string()
    } else {
        format!(
            "\x1b[{};{}r",
            snapshot.scroll_top.saturating_add(1),
            snapshot.scroll_bottom.saturating_add(1)
        )
    };
    buffer.push_str(&format!(
        "{scroll_region}\x1b[{cursor_row};{cursor_col}H{}{cursor_visibility}",
        snapshot.active_style_ansi
    ));
    buffer
}

#[derive(Debug)]
pub enum LifecycleError {
    Io(String, io::Error),
    Protocol(String),
    Pty(crate::pty::PtyError),
    Terminal(TerminalError),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(context, error) => write!(f, "{context}: {error}"),
            Self::Protocol(message) => write!(f, "{message}"),
            Self::Pty(error) => write!(f, "{error}"),
            Self::Terminal(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<crate::pty::PtyError> for LifecycleError {
    fn from(value: crate::pty::PtyError) -> Self {
        Self::Pty(value)
    }
}

impl From<TerminalError> for LifecycleError {
    fn from(value: TerminalError) -> Self {
        Self::Terminal(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        attach_frame_has_visible_workspace, daemon_status_ready, frame_has_visible_first_line,
        looks_like_full_frame, parse_status_fields, workspace_snapshot_ready,
    };
    use crate::terminal::{ScreenSnapshot, TerminalSize};
    use std::path::PathBuf;

    fn snapshot(lines: &[&str], cursor_row: u16, cursor_col: u16) -> ScreenSnapshot {
        let size = TerminalSize {
            rows: lines.len() as u16,
            cols: lines.iter().map(|line| line.len()).max().unwrap_or(0) as u16,
            pixel_width: 0,
            pixel_height: 0,
        };
        ScreenSnapshot {
            size,
            lines: lines.iter().map(|line| line.to_string()).collect(),
            styled_lines: lines.iter().map(|line| line.to_string()).collect(),
            active_style_ansi: "\x1b[0m".to_string(),
            scrollback: Vec::new(),
            styled_scrollback: Vec::new(),
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            window_title: None,
            cursor_row,
            cursor_col,
            cursor_visible: true,
            alternate_screen: true,
        }
    }

    #[test]
    fn daemon_status_requires_ready_yes() {
        assert!(daemon_status_ready(
            "workspace: /tmp/demo\nready: yes\nattached: no"
        ));
        assert!(!daemon_status_ready("workspace: /tmp/demo\nattached: no"));
        assert!(!daemon_status_ready(
            "workspace: /tmp/demo\nready: no\nattached: no"
        ));
    }

    #[test]
    fn detects_visible_first_line_in_full_frame_bytes() {
        let bytes = b"\x1b[H\x1b[1;1Hprompt$ \x1b[K\x1b[2;1H\x1b[K";
        assert!(looks_like_full_frame(bytes));
        assert!(frame_has_visible_first_line(bytes));
    }

    #[test]
    fn ignores_blank_first_line_in_full_frame_bytes() {
        let bytes = b"\x1b[H\x1b[1;1H\x1b[K\x1b[2;1H\x1b[K";
        assert!(looks_like_full_frame(bytes));
        assert!(!frame_has_visible_first_line(bytes));
    }

    #[test]
    fn ignores_sidebar_only_first_line_in_full_frame_bytes() {
        let bytes =
            "\x1b[H\x1b[1;1H                                                   ┃ Sessions\x1b[K"
                .as_bytes();
        assert!(looks_like_full_frame(bytes));
        assert!(!frame_has_visible_first_line(bytes));
        assert!(!attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn attach_frame_detects_visible_workspace_text_in_non_frame_output() {
        assert!(attach_frame_has_visible_workspace(b"prompt$ "));
        assert!(!attach_frame_has_visible_workspace(
            "   ┃ Sessions".as_bytes()
        ));
    }

    #[test]
    fn attach_frame_detects_visible_workspace_text_below_first_row() {
        let bytes = b"\x1b[H\x1b[1;1H                                                   \x1b[K\x1b[2;1Hprompt$ \x1b[K\x1b[3;1Hkeys: demo\x1b[K";
        assert!(attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn full_frame_detection_accepts_leading_terminal_mode_prefixes() {
        let bytes = b"\x1b[?1049h\x1b[H\x1b[?2026h\x1b[H\x1b[1;1Hprompt$ \x1b[K";
        assert!(looks_like_full_frame(bytes));
        assert!(frame_has_visible_first_line(bytes));
        assert!(attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn sidebar_only_diff_is_not_treated_as_full_frame() {
        let bytes = "\x1b[?2026h\x1b[H\x1b[3;52H┃\x1b[3;53H> bash@local 🔊I\x1b[K".as_bytes();
        assert!(!looks_like_full_frame(bytes));
        assert!(!attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn parses_daemon_status_fields() {
        let status = "\
workspace: /tmp/demo\n\
socket: /run/user/1000/waitagent/demo.sock\n\
key: abc123\n\
node: local\n\
child_pid: 4242\n\
ready: yes\n\
attached_clients: 2\n\
screen_size: 24x80\n\
initial_size: 24x80\n\
alternate_screen: yes";
        let parsed =
            parse_status_fields(status, PathBuf::from("/run/user/1000/waitagent/demo.sock"))
                .expect("status should parse");
        assert_eq!(parsed.key, "abc123");
        assert_eq!(parsed.workspace_dir, PathBuf::from("/tmp/demo"));
        assert_eq!(parsed.attached_clients, 2);
        assert_eq!(parsed.rows, 24);
        assert_eq!(parsed.cols, 80);
        assert!(parsed.ready);
    }

    #[test]
    fn workspace_snapshot_ready_when_cursor_has_moved() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &["", "", "", "━━━━━━━━", "keys: demo", "WaitAgent | bash",],
            0,
            12,
        )));
    }

    #[test]
    fn workspace_snapshot_ready_when_work_area_has_content() {
        assert!(workspace_snapshot_ready(&snapshot(
            &[
                "prompt line",
                "",
                "",
                "━━━━━━━━",
                "keys: demo",
                "WaitAgent | bash",
            ],
            0,
            0,
        )));
    }

    #[test]
    fn workspace_snapshot_not_ready_for_footer_only_frame() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &["", "", "", "━━━━━━━━", "keys: demo", "WaitAgent | bash",],
            0,
            0,
        )));
    }

    #[test]
    fn workspace_snapshot_not_ready_when_prompt_is_present_without_chrome() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &["prompt line", "", "", "", "", ""],
            0,
            0,
        )));
    }

    #[test]
    fn attach_frame_ignores_footer_only_snapshot_without_prompt() {
        let bytes = concat!(
            "\x1b[H",
            "\x1b[1;1H                                                   ┃ ← back  ↑↓ move  enter swi \x1b[K",
            "\x1b[2;1H                                                   ┃> bash@local             🔊I\x1b[K",
            "\x1b[20;1H━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┃                            \x1b[K",
            "\x1b[21;1Hkeys: ^W cmd  ^B/^F switch  ^N new  ^L picker  ^X c┃                            \x1b[K",
            "\x1b[22;1HWaitAgent | bash | local/session-1                 ┃bash@local | INPUT | unknow \x1b[K",
            "\x1b[23;1HWaitAgent | bash | local/session-1                      active | 1 waiting | 1/1\x1b[K",
            "\x1b[24;1H                                                                                \x1b[K",
        )
        .as_bytes();

        assert!(!attach_frame_has_visible_workspace(bytes));
    }

    #[test]
    fn workspace_snapshot_not_ready_for_sidebar_only_work_rows() {
        assert!(!workspace_snapshot_ready(&snapshot(
            &[
                "                                                   ┃ Sessions",
                "",
                "divider",
                "keys",
                "status"
            ],
            0,
            0,
        )));
    }
}
