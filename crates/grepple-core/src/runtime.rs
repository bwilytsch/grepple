use std::{
    ffi::CString,
    fs::OpenOptions,
    io::{Read, Write},
    mem,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use serde_json::json;
use ulid::Ulid;

use crate::{
    classify::classify_command,
    error::{GreppleError, Result},
    model::{
        AttachSessionRequest, GitContext, SCHEMA_VERSION, SessionMetadata, SessionProvider,
        SessionStatus, StartSessionRequest, StopSessionRequest,
    },
    storage::SessionStore,
};

#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    pub default_grace_ms: u64,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            default_grace_ms: 1_500,
        }
    }
}

pub fn start_managed_session(
    store: &SessionStore,
    req: StartSessionRequest,
) -> Result<SessionMetadata> {
    let session_id = store.allocate_session_id();
    let (stdout_path, stderr_path, combined_path) = store.create_session_files(&session_id)?;

    let now = Utc::now();
    let cwd = req.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string()
    });

    let auto_name = build_default_name(req.name.clone(), &cwd, None, Some(&req.command));
    let slug = format!(
        "{}-{}",
        auto_name.replace(' ', "-"),
        &session_id[..4].to_lowercase()
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&req.command);
    cmd.current_dir(&cwd);

    for (key, value) in &req.env {
        cmd.env(key, value);
    }
    apply_color_env_defaults(&mut cmd, &req.env);

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(GreppleError::Io)?;
    let pid = child.id() as i32;
    // Best-effort process-group isolation (for Ctrl+C propagation via negative pid kill).
    unsafe {
        let _ = libc::setpgid(pid, pid);
    }

    let stdout_reader = child
        .stdout
        .take()
        .ok_or_else(|| GreppleError::Tool("failed to capture child stdout".to_string()))?;
    let stderr_reader = child
        .stderr
        .take()
        .ok_or_else(|| GreppleError::Tool("failed to capture child stderr".to_string()))?;

    let combined_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&combined_path)?;
    let combined_writer = Arc::new(Mutex::new(combined_file));
    let stdout_pump = spawn_stream_pump(
        stdout_reader,
        stdout_path.clone(),
        Arc::clone(&combined_writer),
        Some(ActivityTracker {
            store: store.clone(),
            session_id: session_id.clone(),
        }),
        if req.foreground {
            Some(MirrorTarget::Stdout)
        } else {
            None
        },
    );
    let stderr_pump = spawn_stream_pump(
        stderr_reader,
        stderr_path.clone(),
        Arc::clone(&combined_writer),
        Some(ActivityTracker {
            store: store.clone(),
            session_id: session_id.clone(),
        }),
        if req.foreground {
            Some(MirrorTarget::Stderr)
        } else {
            None
        },
    );

    // Append deterministic start markers.
    let mut stdout_marker = OpenOptions::new().append(true).open(&stdout_path)?;
    let mut stderr_marker = OpenOptions::new().append(true).open(&stderr_path)?;
    let mut combined_marker = OpenOptions::new().append(true).open(&combined_path)?;
    let marker = format!(
        "[{}] grepple session started: {} (pid={})\n",
        now.to_rfc3339(),
        req.command,
        pid
    );
    stdout_marker.write_all(marker.as_bytes())?;
    stderr_marker.write_all(marker.as_bytes())?;
    combined_marker.write_all(marker.as_bytes())?;

    let labels = classify_command(&req.command);
    let mut meta = SessionMetadata {
        schema_version: SCHEMA_VERSION,
        session_id: session_id.clone(),
        session_slug: slug,
        display_name: auto_name,
        status: SessionStatus::Running,
        provider: SessionProvider::Managed,
        cwd: Some(cwd.clone()),
        command: Some(req.command),
        pid: Some(pid),
        exit_code: None,
        created_at: now,
        updated_at: now,
        last_activity_at: now,
        stopped_at: None,
        summary_last_line: None,
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        combined_path: combined_path.display().to_string(),
        git_context: capture_git_context(Path::new(&cwd)),
        provider_ref: None,
        labels,
    };

    meta.summary_last_line = store.update_summary_from_combined(&session_id)?;

    store.write_meta(&meta)?;
    store.append_event(
        &session_id,
        "session_started",
        json!({
            "pid": pid,
            "provider": "managed",
            "cwd": cwd,
        }),
    )?;

    if req.foreground {
        let _sigint_guard = SigintForwardGuard::install()?;
        let status = wait_foreground_with_sigint_forwarding(&mut child, pid)?;
        let _ = stdout_pump.join();
        let _ = stderr_pump.join();
        restore_terminal_state();
        let meta = mark_session_exited(store, &session_id, status)?;
        return Ok(meta);
    }

    let store_for_wait = store.clone();
    let wait_session_id = session_id.clone();
    let stdout_wait = stdout_pump;
    let stderr_wait = stderr_pump;
    thread::spawn(move || {
        if let Ok(status) = child.wait() {
            let _ = stdout_wait.join();
            let _ = stderr_wait.join();
            let _ = mark_session_exited(&store_for_wait, &wait_session_id, status);
        }
    });

    Ok(meta)
}

pub fn attach_tmux_session(
    store: &SessionStore,
    req: AttachSessionRequest,
) -> Result<SessionMetadata> {
    let panes = list_tmux_panes()?;
    if panes.is_empty() {
        return Err(GreppleError::InvalidArgument(
            "no tmux panes found".to_string(),
        ));
    }

    let selected = if let Some(target) = req.target {
        panes
            .into_iter()
            .find(|pane| pane.pane_id == target || pane.label == target)
            .ok_or_else(|| {
                GreppleError::InvalidArgument(format!("tmux target not found: {target}"))
            })?
    } else if panes.len() == 1 {
        panes[0].clone()
    } else {
        let options = panes
            .iter()
            .map(|p| format!("{} ({})", p.label, p.pane_id))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(GreppleError::InvalidArgument(format!(
            "AMBIGUOUS_TARGET: multiple tmux panes detected, provide --target. candidates: {options}"
        )));
    };

    let session_id = store.allocate_session_id();
    let (stdout_path, stderr_path, combined_path) = store.create_session_files(&session_id)?;

    let output = Command::new("tmux")
        .arg("capture-pane")
        .arg("-p")
        .arg("-e")
        .arg("-J")
        .arg("-S")
        .arg("-500")
        .arg("-t")
        .arg(&selected.pane_id)
        .output()?;

    let captured = String::from_utf8_lossy(&output.stdout);
    std::fs::write(&combined_path, captured.as_bytes())?;
    std::fs::write(&stdout_path, captured.as_bytes())?;

    let cwd = selected.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string()
    });
    let now = Utc::now();
    let auto_name = build_default_name(req.name, &cwd, Some(&selected.label), None);

    let mut meta = SessionMetadata {
        schema_version: SCHEMA_VERSION,
        session_id: session_id.clone(),
        session_slug: format!(
            "{}-{}",
            auto_name.replace(' ', "-"),
            &session_id[..4].to_lowercase()
        ),
        display_name: auto_name,
        status: SessionStatus::Running,
        provider: SessionProvider::TmuxAttach,
        cwd: Some(cwd.clone()),
        command: Some(format!("tmux:{}", selected.label)),
        pid: None,
        exit_code: None,
        created_at: now,
        updated_at: now,
        last_activity_at: now,
        stopped_at: None,
        summary_last_line: store.update_summary_from_combined(&session_id)?,
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        combined_path: combined_path.display().to_string(),
        git_context: capture_git_context(Path::new(&cwd)),
        provider_ref: Some(selected.pane_id),
        labels: classify_command(&selected.command),
    };

    store.write_meta(&meta)?;
    store.append_event(
        &session_id,
        "session_attached",
        json!({
            "provider": "tmux_attach",
            "target": selected.label,
        }),
    )?;

    // Snapshot-only attach in v1: status remains running but has no process control handle.
    meta.updated_at = Utc::now();
    store.write_meta(&meta)?;

    Ok(meta)
}

pub fn stop_session(
    store: &SessionStore,
    req: StopSessionRequest,
    opts: &RuntimeOptions,
) -> Result<SessionMetadata> {
    let mut meta = store.read_meta(&req.session_id)?;

    if meta.provider != SessionProvider::Managed {
        meta.status = SessionStatus::Stopped;
        meta.stopped_at = Some(Utc::now());
        meta.updated_at = Utc::now();
        store.write_meta(&meta)?;
        store.append_event(
            &meta.session_id,
            "session_stopped",
            json!({"reason": "non-managed"}),
        )?;
        return Ok(meta);
    }

    let pid = meta
        .pid
        .ok_or_else(|| GreppleError::InvalidArgument("session does not have pid".to_string()))?;

    let grace_ms = if req.grace_ms == 0 {
        opts.default_grace_ms
    } else {
        req.grace_ms
    };

    let _ = signal_process_group(pid, libc::SIGINT);
    thread::sleep(Duration::from_millis(grace_ms));
    if is_process_alive(pid) {
        let _ = signal_process_group(pid, libc::SIGTERM);
        thread::sleep(Duration::from_millis(300));
    }
    if is_process_alive(pid) {
        let _ = signal_process_group(pid, libc::SIGKILL);
    }

    meta.status = SessionStatus::Stopped;
    meta.stopped_at = Some(Utc::now());
    meta.updated_at = Utc::now();
    meta.last_activity_at = Utc::now();
    meta.summary_last_line = store.update_summary_from_combined(&meta.session_id)?;
    store.write_meta(&meta)?;
    store.append_event(
        &meta.session_id,
        "session_stopped",
        json!({"pid": pid, "grace_ms": grace_ms}),
    )?;

    Ok(meta)
}

pub fn refresh_status(store: &SessionStore, session_id: &str) -> Result<SessionMetadata> {
    let mut meta = store.read_meta(session_id)?;
    if meta.provider == SessionProvider::Managed
        && matches!(
            meta.status,
            SessionStatus::Starting | SessionStatus::Running
        )
    {
        if let Some(pid) = meta.pid {
            if !is_process_alive(pid) {
                meta.status = SessionStatus::Stopped;
                meta.stopped_at = Some(Utc::now());
                meta.updated_at = Utc::now();
                meta.last_activity_at = Utc::now();
                store.write_meta(&meta)?;
            }
        }
    }

    Ok(meta)
}

pub fn list_tmux_panes() -> Result<Vec<TmuxPane>> {
    let output = Command::new("tmux")
        .arg("list-panes")
        .arg("-a")
        .arg("-F")
        .arg("#{session_name}:#{window_index}.#{pane_index}\t#{pane_id}\t#{pane_current_command}\t#{pane_current_path}")
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut panes = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() < 3 {
            continue;
        }
        panes.push(TmuxPane {
            label: parts[0].to_string(),
            pane_id: parts[1].to_string(),
            command: parts[2].to_string(),
            cwd: parts.get(3).map(|v| (*v).to_string()),
        });
    }
    Ok(panes)
}

#[derive(Debug, Clone)]
pub struct TmuxPane {
    pub label: String,
    pub pane_id: String,
    pub command: String,
    pub cwd: Option<String>,
}

pub fn build_default_name(
    explicit: Option<String>,
    cwd: &str,
    tmux_label: Option<&str>,
    command: Option<&str>,
) -> String {
    if let Some(name) = explicit {
        return with_suffix(name);
    }

    if let Some(git) = capture_git_context(Path::new(cwd)) {
        let repo = Path::new(&git.repo_root)
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("repo");
        return with_suffix(format!("{}:{}", repo, git.branch));
    }

    if let Some(label) = tmux_label {
        return with_suffix(format!("tmux:{}", label));
    }

    if let Some(cmd) = command {
        let compact = cmd.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
        return with_suffix(compact);
    }

    with_suffix("grepple-session".to_string())
}

fn with_suffix(base: String) -> String {
    let suffix = Ulid::new().to_string();
    format!("{}#{}", base, &suffix[..4].to_lowercase())
}

pub fn capture_git_context(cwd: &Path) -> Option<GitContext> {
    let worktree_root = normalize_path(git_output(cwd, &["rev-parse", "--show-toplevel"])?);
    let repo_root = git_common_dir_to_repo_root(cwd)
        .map(normalize_path)
        .unwrap_or_else(|| worktree_root.clone());
    let branch = git_output(cwd, &["branch", "--show-current"])
        .or_else(|| git_output(cwd, &["rev-parse", "--short", "HEAD"]))?;
    let head_sha = git_output(cwd, &["rev-parse", "HEAD"])?;

    Some(GitContext {
        repo_root,
        worktree_root,
        branch,
        head_sha,
        captured_at: Utc::now(),
    })
}

fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if out.is_empty() { None } else { Some(out) }
}

fn git_common_dir_to_repo_root(cwd: &Path) -> Option<String> {
    let common_dir = git_output(cwd, &["rev-parse", "--git-common-dir"])?;
    let common_path = PathBuf::from(&common_dir);
    let common_path = if common_path.is_absolute() {
        common_path
    } else {
        cwd.join(common_path)
    };
    let canonical = std::fs::canonicalize(common_path).ok()?;
    if canonical.file_name().and_then(|v| v.to_str()) == Some(".git") {
        return canonical.parent().map(|p| p.display().to_string());
    }
    Some(canonical.display().to_string())
}

fn normalize_path(path: String) -> String {
    std::fs::canonicalize(&path)
        .map(|p| p.display().to_string())
        .unwrap_or(path)
}

fn touch_session_activity(store: &SessionStore, session_id: &str) -> Result<()> {
    let mut meta = store.read_meta(session_id)?;
    if matches!(
        meta.status,
        SessionStatus::Running | SessionStatus::Starting
    ) {
        let now = Utc::now();
        meta.updated_at = now;
        meta.last_activity_at = now;
        store.write_meta(&meta)?;
    }
    Ok(())
}

fn signal_process_group(pid: i32, signal: i32) -> Result<()> {
    // Negative pid means process group kill.
    let rc = unsafe { libc::kill(-pid, signal) };
    if rc == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err.into())
}

fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }

    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

pub fn process_group_name(pid: i32) -> Option<String> {
    let c_path = CString::new(format!("/proc/{pid}")).ok()?;
    if c_path.as_bytes().is_empty() {
        return None;
    }
    Some(format!("pgid:{pid}"))
}

#[derive(Clone)]
struct ActivityTracker {
    store: SessionStore,
    session_id: String,
}

fn spawn_stream_pump<R: Read + Send + 'static>(
    mut reader: R,
    stream_path: PathBuf,
    combined: Arc<Mutex<std::fs::File>>,
    activity: Option<ActivityTracker>,
    mirror: Option<MirrorTarget>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stream_file = match OpenOptions::new().append(true).open(stream_path) {
            Ok(file) => file,
            Err(_) => return,
        };

        let mut buffer = [0_u8; 8192];
        let mut last_touch = Instant::now() - Duration::from_secs(2);
        loop {
            let read_n = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };

            let chunk = &buffer[..read_n];
            if stream_file.write_all(chunk).is_err() {
                break;
            }

            if let Some(target) = mirror {
                match target {
                    MirrorTarget::Stdout => {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(chunk);
                        let _ = out.flush();
                    }
                    MirrorTarget::Stderr => {
                        let mut out = std::io::stderr();
                        let _ = out.write_all(chunk);
                        let _ = out.flush();
                    }
                }
            }

            if let Ok(mut combined_file) = combined.lock() {
                if combined_file.write_all(chunk).is_err() {
                    break;
                }
            } else {
                break;
            }

            if let Some(activity) = &activity {
                if last_touch.elapsed() >= Duration::from_secs(1) {
                    let _ = touch_session_activity(&activity.store, &activity.session_id);
                    last_touch = Instant::now();
                }
            }
        }
    })
}

fn mark_session_exited(
    store: &SessionStore,
    session_id: &str,
    status: ExitStatus,
) -> Result<SessionMetadata> {
    let mut meta = store.read_meta(session_id)?;
    meta.exit_code = exit_code_from_status(status);
    if matches!(
        meta.status,
        SessionStatus::Running | SessionStatus::Starting
    ) {
        meta.status = SessionStatus::Stopped;
        meta.stopped_at = Some(Utc::now());
        meta.updated_at = Utc::now();
        meta.last_activity_at = Utc::now();
        meta.summary_last_line = store.update_summary_from_combined(session_id)?;
        store.write_meta(&meta)?;
        store.append_event(
            session_id,
            "session_exited",
            json!({
                "exit_code": meta.exit_code,
                "success": status.success(),
            }),
        )?;
    }
    Ok(meta)
}

fn wait_foreground_with_sigint_forwarding(
    child: &mut std::process::Child,
    pid: i32,
) -> Result<ExitStatus> {
    loop {
        if SIGINT_RECEIVED.swap(false, Ordering::SeqCst) {
            let _ = signal_process_group(pid, libc::SIGINT);
        }

        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }

        thread::sleep(Duration::from_millis(25));
    }
}

fn restore_terminal_state() {
    // Best-effort terminal reset for interrupted foreground commands.
    let mut out = std::io::stderr();
    let _ = out.write_all(b"\x1b[0m\x1b[?25h");
    let _ = out.flush();
}

fn exit_code_from_status(status: ExitStatus) -> Option<i32> {
    status.code().or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|signal| 128 + signal)
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

#[derive(Clone, Copy)]
enum MirrorTarget {
    Stdout,
    Stderr,
}

static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn sigint_flag_handler(_: i32) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

struct SigintForwardGuard {
    previous: libc::sigaction,
}

impl SigintForwardGuard {
    fn install() -> Result<Self> {
        SIGINT_RECEIVED.store(false, Ordering::SeqCst);

        let mut previous = mem::MaybeUninit::<libc::sigaction>::uninit();
        let mut action = unsafe { mem::zeroed::<libc::sigaction>() };

        action.sa_sigaction = sigint_flag_handler as usize;
        action.sa_flags = 0;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGINT, &action, previous.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                previous: previous.assume_init(),
            })
        }
    }
}

impl Drop for SigintForwardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::sigaction(libc::SIGINT, &self.previous, ptr::null_mut());
        }
    }
}

fn apply_color_env_defaults(cmd: &mut Command, user_env: &[(String, String)]) {
    if has_env_key(user_env, "NO_COLOR") {
        return;
    }

    // Managed sessions run over pipes, so many CLIs default to no color.
    // Also clear inherited NO_COLOR unless the caller explicitly set it.
    cmd.env_remove("NO_COLOR");

    if !should_force_color(user_env) {
        return;
    }

    set_env_if_absent(cmd, user_env, "CLICOLOR", "1");
    set_env_if_absent(cmd, user_env, "CLICOLOR_FORCE", "1");
    set_env_if_absent(cmd, user_env, "FORCE_COLOR", "1");
    set_env_if_absent(cmd, user_env, "TERM", "xterm-256color");
    set_env_if_absent(cmd, user_env, "COLORTERM", "truecolor");
}

fn set_env_if_absent(cmd: &mut Command, user_env: &[(String, String)], key: &str, value: &str) {
    if user_env.iter().any(|(k, _)| k == key) {
        return;
    }
    if std::env::var_os(key).is_some() {
        return;
    }
    cmd.env(key, value);
}

fn should_force_color(user_env: &[(String, String)]) -> bool {
    if env_value(user_env, "TERM")
        .as_deref()
        .is_some_and(|term| term.eq_ignore_ascii_case("dumb"))
    {
        return false;
    }

    if env_value(user_env, "CLICOLOR").as_deref() == Some("0")
        || env_value(user_env, "CLICOLOR_FORCE").as_deref() == Some("0")
        || env_value(user_env, "FORCE_COLOR").as_deref() == Some("0")
    {
        return false;
    }

    true
}

fn has_env_key(user_env: &[(String, String)], key: &str) -> bool {
    user_env.iter().any(|(k, _)| k == key)
}

fn env_value(user_env: &[(String, String)], key: &str) -> Option<String> {
    user_env
        .iter()
        .rev()
        .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None })
        .or_else(|| std::env::var(key).ok())
}
