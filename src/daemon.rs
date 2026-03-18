//! `pty-host` — a lightweight PTY shepherd daemon.
//!
//! Each invocation manages exactly one PTY session. Zed spawns one
//! `pty-host` per terminal. The process:
//!
//! 1. Creates a PTY and spawns the requested shell
//! 2. Listens on a Unix domain socket for client connections
//! 3. Proxies I/O between the PTY master fd and the connected client
//! 4. Survives client disconnects (keeps the shell alive)
//! 5. Exits when the shell exits and no client is connected
//!
//! # Usage
//!
//! ```text
//! pty-host --session <UUID> [OPTIONS]
//!
//! Options:
//!   --session <UUID>       Session ID (determines socket path)
//!   --shell <PATH>         Shell program (default: $SHELL or /bin/sh)
//!   --arg <ARG>            Shell argument (repeatable)
//!   --cwd <DIR>            Working directory
//!   --env <KEY=VALUE>      Environment variable (repeatable)
//!   --cols <N>             Initial columns (default: 80)
//!   --rows <N>             Initial rows (default: 24)
//!   --scrollback <BYTES>   Scrollback buffer size in bytes (default: 65536)
//!   --socket <PATH>        Override socket path (default: $TMPDIR/zed-pty/<session>.sock)
//!   --list                 List active sessions and exit
//!   --kill <UUID>          Kill a session by sending SIGHUP to its host process
//!   --attach <UUID>        Attach to a running session as a dumb terminal client
//! ```

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::os::unix::io::AsRawFd;

use crate::protocol::{
    self, ClientMessage, HostMessage, WindowSize,
};
use crate::session::{Session, SessionConfig};

/// Set up logging to both stderr and a per-session log file.
///
/// Log files are written to `$TMPDIR/zed-pty/logs/<session-uuid>.log`.
/// If the session UUID isn't known yet (e.g. `--list`), falls back to
/// stderr-only logging.
fn init_logging(args: &[String]) {
    use std::fs;

    // Try to extract the session UUID so we can name the log file.
    let session_id = args.windows(2).find_map(|pair| {
        if pair[0] == "--session" {
            Some(pair[1].as_str())
        } else {
            None
        }
    });

    let log_dir = std::env::temp_dir().join("zed-pty").join("logs");
    let _ = fs::create_dir_all(&log_dir);

    let log_file = session_id.and_then(|id| {
        let path = log_dir.join(format!("{id}.log"));
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|f| (f, path))
    });

    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    );
    builder.format_timestamp_millis();

    if let Some((file, path)) = log_file {
        use std::sync::Mutex;
        let file = Arc::new(Mutex::new(file));
        let file2 = file.clone();

        // Write to both stderr and the log file.
        builder.format(move |buf, record| {
            use std::io::Write as _;
            let line = format!(
                "[{} {} {}] {}\n",
                buf.timestamp_millis(),
                record.level(),
                record.target(),
                record.args(),
            );
            // Best-effort write to log file; never fail logging.
            if let Ok(mut f) = file.lock() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
            buf.write_all(line.as_bytes())
        });

        builder.init();

        // Install a panic hook that writes to the log file before aborting.
        let panic_file = file2;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = format!("[PANIC] {info}\n");
            eprintln!("{msg}");
            if let Ok(mut f) = panic_file.lock() {
                let _ = f.write_all(msg.as_bytes());
                let _ = f.flush();
            }
            prev(info);
        }));

        log::info!("Log file: {}", path.display());
    } else {
        builder.init();
    }
}


pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    init_logging(&args);

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        process::exit(0);
    }

    if args.iter().any(|a| a == "--list") {
        cmd_list();
        process::exit(0);
    }

    if let Some(pos) = args.iter().position(|a| a == "--kill") {
        let id = args.get(pos + 1).unwrap_or_else(|| {
            eprintln!("error: --kill requires a session UUID");
            process::exit(1);
        });
        cmd_kill(id);
        process::exit(0);
    }

    if let Some(pos) = args.iter().position(|a| a == "--attach") {
        let id = args.get(pos + 1).unwrap_or_else(|| {
            eprintln!("error: --attach requires a session UUID");
            process::exit(1);
        });
        cmd_attach(id);
        process::exit(0);
    }

    let config = match parse_session_args(&args[1..]) {
        Ok(config) => config,
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!();
            print_usage();
            process::exit(1);
        }
    };

    log::info!(
        "Starting pty-host: shell={}, socket={}",
        config.program,
        config.socket_path.display(),
    );

    let mut session = match Session::spawn(&config) {
        Ok(s) => s,
        Err(e) => {
            log::error!("Failed to spawn session: {e:#}");
            process::exit(1);
        }
    };

    // Print the socket path to stdout so the parent (Zed) can read it.
    println!("{}", config.socket_path.display());

    if let Err(e) = session.run() {
        log::error!("Session error: {e:#}");
        process::exit(1);
    }

    log::info!("Session ended cleanly");
}

// ---------------------------------------------------------------------------
// --attach: dumb terminal client
// ---------------------------------------------------------------------------

/// Attach to a running session as an interactive terminal client.
///
/// Puts stdin into raw mode, sends keystrokes as DATA messages, and prints
/// received DATA messages to stdout. Handles terminal resize via SIGWINCH.
/// Detaches cleanly on disconnect or when the child exits.
fn cmd_attach(session_id_str: &str) {
    let id = match uuid::Uuid::parse_str(session_id_str) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: invalid session UUID '{session_id_str}': {e}");
            process::exit(1);
        }
    };

    let socket_path = crate::socket_path_for_session(&id);
    if !socket_path.exists() {
        eprintln!(
            "error: no session socket at {}",
            socket_path.display()
        );
        process::exit(1);
    }

    let stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "error: could not connect to {}: {e}",
                socket_path.display()
            );
            process::exit(1);
        }
    };

    eprintln!("Attached to session {id}. Press Ctrl-] to detach.");

    // Put stdin into raw mode so we get individual keystrokes.
    let orig_termios = match enable_raw_mode() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not enable raw mode: {e}");
            process::exit(1);
        }
    };

    // Send initial RESIZE based on the current terminal size.
    if let Some(size) = get_terminal_size() {
        let mut writer = &stream;
        let _ = protocol::send_client_message(
            &mut writer,
            &ClientMessage::Resize(size),
        );
    }

    // Flag to signal threads to stop.
    let running = Arc::new(AtomicBool::new(true));

    // Install SIGWINCH handler to forward terminal resizes.
    let resize_stream = stream.try_clone().expect("clone socket for resize");
    let resize_running = running.clone();
    install_sigwinch_handler(resize_stream, resize_running);

    // Save stdin fd so we can interrupt the blocking read from another thread.
    let stdin_fd = io::stdin().as_raw_fd();

    // Reader thread: host → stdout.
    // When this thread exits (disconnect, eviction, child exit), it sets
    // `running` to false and interrupts the writer's blocking stdin read
    // by sending it SIGWINCH (harmless no-op signal that interrupts read).
    let reader_stream = stream.try_clone().expect("clone socket for reader");
    let reader_running = running.clone();
    let writer_thread_id = unsafe { libc::pthread_self() };
    let reader_handle = std::thread::spawn(move || {
        reader_loop(reader_stream, &reader_running);
        // Signal the writer thread to wake up from its blocking stdin read.
        // SIGUSR1 will interrupt the read() syscall with EINTR.
        reader_running.store(false, Ordering::Relaxed);
        unsafe {
            libc::pthread_kill(writer_thread_id, libc::SIGUSR1);
        }
    });

    // Install a no-op SIGUSR1 handler so the signal just interrupts read()
    // instead of killing the process.
    unsafe {
        libc::signal(libc::SIGUSR1, noop_signal_handler as *const () as libc::sighandler_t);
    }

    // Writer loop (this thread): stdin → host.
    // We use the DETACH_BYTE (Ctrl-]) as the escape to detach.
    const DETACH_BYTE: u8 = 0x1d; // Ctrl-]
    let mut writer_stream = stream;
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 4096];

    while running.load(Ordering::Relaxed) {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,        // EOF on stdin
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // Check if we were interrupted because the reader died.
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };

        // Check if reader died while we were reading.
        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Check for the detach escape character.
        if buf[..n].contains(&DETACH_BYTE) {
            // Send DETACH and exit.
            let _ = protocol::send_client_message(
                &mut writer_stream,
                &ClientMessage::Detach,
            );
            running.store(false, Ordering::Relaxed);
            break;
        }

        // Forward input to the host.
        if protocol::send_client_message(
            &mut writer_stream,
            &ClientMessage::Data(buf[..n].to_vec()),
        )
        .is_err()
        {
            break;
        }
    }

    running.store(false, Ordering::Relaxed);
    let _ = reader_handle.join();

    // Restore terminal mode.
    restore_terminal_mode(&orig_termios);

    let _ = stdin_fd; // used above for documentation; fd borrowed via as_raw_fd
    eprintln!("\r\nDetached from session {id}.");
}

/// Read messages from the host and write DATA payloads to stdout.
fn reader_loop(stream: UnixStream, running: &AtomicBool) {
    let mut reader = stream;
    let mut stdout = io::stdout().lock();

    while running.load(Ordering::Relaxed) {
        let msg = match HostMessage::decode_from(&mut reader) {
            Ok(msg) => msg,
            Err(protocol::DecodeError::UnexpectedEof) => break,
            Err(protocol::DecodeError::Io(ref e))
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                continue;
            }
            Err(protocol::DecodeError::PayloadTooLarge(len)) => {
                // The header was consumed but the payload bytes are still
                // in the stream.  Drain them so we can continue reading
                // subsequent messages (e.g. ReplayDone after an oversized
                // Snapshot from an old host that doesn't chunk).
                let _ = writeln!(
                    io::stderr(),
                    "[pty-host: skipping oversized frame, {len} bytes]"
                );
                if io::copy(
                    &mut (&mut reader).take(len as u64),
                    &mut io::sink(),
                ).is_err() {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };

        match msg {
            HostMessage::Data(bytes) => {
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
            }
            HostMessage::Ready { pid, .. } => {
                // Silently consume — the READY message is informational.
                // We could print it to stderr for debugging:
                let _ = writeln!(io::stderr(), "[pty-host: ready, pid={pid}]");
            }
            HostMessage::ChildExit { raw_status } => {
                let code = raw_status.unwrap_or(-1);
                let _ = writeln!(
                    io::stderr(),
                    "\r\n[pty-host: child exited, raw_status={code}]"
                );
                running.store(false, Ordering::Relaxed);
                break;
            }
            HostMessage::Error(msg) => {
                let _ = writeln!(io::stderr(), "\r\n[pty-host error: {msg}]");
                running.store(false, Ordering::Relaxed);
                break;
            }
            HostMessage::Snapshot(_) => {
                // Snapshot was already consumed during connect — ignore.
            }
            HostMessage::ReplayDone => {
                // Replay was already consumed during connect — ignore.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Raw mode / terminal helpers
// ---------------------------------------------------------------------------

/// Enable raw mode on stdin. Returns the original termios for restoration.
fn enable_raw_mode() -> io::Result<libc::termios> {
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut orig) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut raw = orig;
    // Input: no SIGINT/SIGQUIT, no CR→NL, no parity, no strip, no flow control
    raw.c_iflag &= !(libc::BRKINT
        | libc::ICRNL
        | libc::INPCK
        | libc::ISTRIP
        | libc::IXON);
    // Output: disable post-processing
    raw.c_oflag &= !libc::OPOST;
    // Control: 8-bit chars
    raw.c_cflag |= libc::CS8;
    // Local: no echo, no canonical mode, no signals, no extended
    raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
    // Read returns after 1 byte, no timeout
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;

    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(orig)
}

/// Restore terminal to the given termios settings.
fn restore_terminal_mode(orig: &libc::termios) {
    unsafe {
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, orig);
    }
}

/// Query the current terminal window size via ioctl.
fn get_terminal_size() -> Option<WindowSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some(WindowSize {
            cols: ws.ws_col,
            rows: ws.ws_row,
            // Estimate cell size — not critical for the attach client.
            cell_width: if ws.ws_xpixel > 0 {
                ws.ws_xpixel / ws.ws_col
            } else {
                8
            },
            cell_height: if ws.ws_ypixel > 0 {
                ws.ws_ypixel / ws.ws_row
            } else {
                16
            },
        })
    } else {
        None
    }
}

/// No-op signal handler used to make SIGUSR1 interrupt blocking reads
/// without killing the process.
extern "C" fn noop_signal_handler(_sig: libc::c_int) {}

/// Install a SIGWINCH handler that sends RESIZE messages when the terminal
/// is resized. Runs in a background thread using signal-hook's pipe mechanism.
fn install_sigwinch_handler(stream: UnixStream, running: Arc<AtomicBool>) {
    use signal_hook::consts::SIGWINCH;

    let (sender, receiver) = match std::os::unix::net::UnixStream::pair() {
        Ok(pair) => pair,
        Err(_) => return,
    };

    if signal_hook::low_level::pipe::register(SIGWINCH, sender).is_err() {
        return;
    }

    receiver.set_nonblocking(false).ok();

    std::thread::spawn(move || {
        let mut sig_buf = [0u8; 32];
        let mut writer = &stream;

        while running.load(Ordering::Relaxed) {
            // Block until SIGWINCH arrives.
            match (&receiver).read(&mut sig_buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }

            if let Some(size) = get_terminal_size() {
                let _ = protocol::send_client_message(
                    &mut writer,
                    &ClientMessage::Resize(size),
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Other commands
// ---------------------------------------------------------------------------

fn print_usage() {
    eprintln!(
        "\
pty-host — lightweight PTY shepherd daemon

Usage:
  pty-host --session <UUID> [OPTIONS]
  pty-host --attach <UUID>
  pty-host --list
  pty-host --kill <UUID>

Session options:
  --session <UUID>       Session ID (determines socket path)
  --shell <PATH>         Shell program (default: $SHELL or /bin/sh)
  --arg <ARG>            Shell argument (repeatable)
  --cwd <DIR>            Working directory
  --env <KEY=VALUE>      Environment variable (repeatable)
  --cols <N>             Initial columns (default: 80)
  --rows <N>             Initial rows (default: 24)
  --scrollback <BYTES>   Scrollback buffer size in bytes (default: 65536)
  --socket <PATH>        Override socket path

Management:
  --attach <UUID>        Attach to a running session (Ctrl-] to detach)
  --list                 List active sessions and exit
  --kill <UUID>          Kill a session by ID
  -h, --help             Show this help message"
    );
}

fn parse_session_args(args: &[String]) -> Result<SessionConfig, String> {
    let mut config = SessionConfig::default();
    let mut session_id: Option<uuid::Uuid> = None;
    let mut socket_override: Option<String> = None;
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                let val = next_arg(args, &mut i, "--session")?;
                session_id = Some(
                    uuid::Uuid::parse_str(&val)
                        .map_err(|e| format!("invalid session UUID '{val}': {e}"))?,
                );
            }
            "--shell" => {
                config.program = next_arg(args, &mut i, "--shell")?;
            }
            "--arg" => {
                config.args.push(next_arg(args, &mut i, "--arg")?);
            }
            "--cwd" => {
                config.working_directory = Some(next_arg(args, &mut i, "--cwd")?.into());
            }
            "--env" => {
                let val = next_arg(args, &mut i, "--env")?;
                let (key, value) = val
                    .split_once('=')
                    .ok_or_else(|| format!("--env value must be KEY=VALUE, got '{val}'"))?;
                config.env.push((key.to_string(), value.to_string()));
            }
            "--cols" => {
                let val = next_arg(args, &mut i, "--cols")?;
                cols = val
                    .parse()
                    .map_err(|_| format!("invalid --cols value: '{val}'"))?;
            }
            "--rows" => {
                let val = next_arg(args, &mut i, "--rows")?;
                rows = val
                    .parse()
                    .map_err(|_| format!("invalid --rows value: '{val}'"))?;
            }
            "--scrollback" => {
                let val = next_arg(args, &mut i, "--scrollback")?;
                config.max_scrollback_lines = val
                    .parse()
                    .map_err(|_| format!("invalid --scrollback value: '{val}'"))?;
            }
            "--socket" => {
                socket_override = Some(next_arg(args, &mut i, "--socket")?);
            }
            other => {
                return Err(format!("unknown argument: '{other}'"));
            }
        }
        i += 1;
    }

    let session_id = session_id.ok_or("--session <UUID> is required")?;

    config.socket_path = match socket_override {
        Some(path) => path.into(),
        None => crate::socket_path_for_session(&session_id),
    };

    config.initial_size = WindowSize {
        cols,
        rows,
        cell_width: 8,
        cell_height: 16,
    };

    Ok(config)
}

fn next_arg(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn cmd_list() {
    let sessions = crate::discover_sessions();
    if sessions.is_empty() {
        println!("No active sessions.");
        return;
    }

    // Gather process info for all pty-host processes in one pass.
    let proc_info = gather_pty_host_process_info();

    for (id, path) in &sessions {
        if !path.exists() {
            println!("stale	{id}		");
            continue;
        }

        let id_str = id.to_string();
        match proc_info.get(&id_str) {
            Some(info) => {
                let state = if info.has_client { "attached" } else { "detached" };
                let started = info.started.as_deref().unwrap_or("");
                println!("{state}	{id}	{started}	{}", info.cwd.as_deref().unwrap_or(""));
            }
            None => {
                // Socket exists but no process found — zombie socket.
                println!("stale	{id}		");
            }
        }
    }
}

/// Per-session info gathered from the OS process table and lsof.
struct PtyHostProcInfo {
    /// Working directory from --cwd argument.
    cwd: Option<String>,
    /// Whether a client is currently connected (accepted socket exists).
    has_client: bool,
    /// Process start time (from ps lstart), e.g. "2026-03-16 21:08".
    started: Option<String>,
}

/// Query the OS for all running pty-host processes and determine their
/// cwd and client-attached state.
///
/// Returns a map from session UUID string to process info.
///
/// **How client detection works:** each pty-host process has a Unix
/// listener socket (the `.sock` file). When a client connects, `accept()`
/// creates a second fd referencing the same path. So:
/// - 1 `.sock` fd → no client (detached)
/// - 2 `.sock` fds → client connected (attached)
///
/// We use `lsof -U` to count `.sock` references per PID.
fn gather_pty_host_process_info() -> std::collections::HashMap<String, PtyHostProcInfo> {
    use std::collections::HashMap;

    let mut result: HashMap<String, PtyHostProcInfo> = HashMap::new();

    // Step 1: Get all pty-host PIDs, their session UUIDs, and --cwd values
    // from `ps`.
    let ps_output = match std::process::Command::new("ps")
        .args(["-eo", "pid,lstart,args"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return result,
    };
    let ps_text = String::from_utf8_lossy(&ps_output.stdout);

    // Map PID → (session_id, cwd, started)
    let mut pid_info: HashMap<String, (String, Option<String>, Option<String>)> = HashMap::new();

    for line in ps_text.lines() {
        let line = line.trim();
        if !line.contains("pty-host") || !line.contains("--session") {
            continue;
        }
        // Format: PID LSTART(5 fields) ARGS...
        // e.g.: "1234 Mon 16 Mar 21:08:00 2026 /path/to/pty-host --session ..."
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 7 {
            continue;
        }
        let pid = fields[0].to_string();
        // lstart is 5 fields: DOW DD MON HH:MM:SS YYYY
        // Reformat to ISO 8601: YYYY-MM-DDTHH:MM:SS
        let mon = match fields[3] {
            "Jan" => "01", "Feb" => "02", "Mar" => "03", "Apr" => "04",
            "May" => "05", "Jun" => "06", "Jul" => "07", "Aug" => "08",
            "Sep" => "09", "Oct" => "10", "Nov" => "11", "Dec" => "12",
            _ => "00",
        };
        let started = format!(
            "{}-{}-{:0>2}T{}",
            fields[5], mon, fields[2], fields[4]
        );

        // Extract --session UUID
        let session_id = fields[6..]
            .windows(2)
            .find(|w| w[0] == "--session")
            .map(|w| w[1].to_string());

        // Extract --cwd path
        let cwd = fields[6..]
            .windows(2)
            .find(|w| w[0] == "--cwd")
            .map(|w| w[1].to_string());

        if let Some(sid) = session_id {
            pid_info.insert(pid, (sid, cwd, Some(started)));
        }
    }

    if pid_info.is_empty() {
        return result;
    }

    // Step 2: Use lsof to count .sock fd references per PID.
    // We pass all PIDs at once to avoid N lsof invocations.
    let pids_csv: String = pid_info
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");

    let lsof_output = match std::process::Command::new("lsof")
        .args(["-p", &pids_csv, "-U", "-F", "pn"])
        .output()
    {
        Ok(o) => o,
        Err(_) => {
            // lsof unavailable — fall back to showing everything as "detached".
            for (_, (sid, cwd, started)) in &pid_info {
                result.insert(
                    sid.clone(),
                    PtyHostProcInfo {
                        cwd: cwd.clone(),
                        has_client: false,
                        started: started.clone(),
                    },
                );
            }
            return result;
        }
    };
    let lsof_text = String::from_utf8_lossy(&lsof_output.stdout);

    // lsof -F pn output format:
    //   p<PID>        — process header
    //   n<NAME>       — fd name (socket path or ->0x...)
    // Count how many times each PID has a line containing ".sock".
    let mut sock_counts: HashMap<String, usize> = HashMap::new();
    let mut current_pid = String::new();

    for line in lsof_text.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            current_pid = pid.to_string();
        } else if let Some(name) = line.strip_prefix('n') {
            if name.contains(".sock") && !current_pid.is_empty() {
                *sock_counts.entry(current_pid.clone()).or_default() += 1;
            }
        }
    }

    // Step 3: Assemble results.
    for (pid, (sid, cwd, started)) in &pid_info {
        let sock_count = sock_counts.get(pid).copied().unwrap_or(0);
        // 1 .sock fd = listener only (detached)
        // 2+ .sock fds = listener + accepted client (attached)
        result.insert(
            sid.clone(),
            PtyHostProcInfo {
                cwd: cwd.clone(),
                has_client: sock_count >= 2,
                started: started.clone(),
            },
        );
    }

    result
}

fn cmd_kill(session_id_str: &str) {
    let id = match uuid::Uuid::parse_str(session_id_str) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: invalid session UUID '{session_id_str}': {e}");
            process::exit(1);
        }
    };

    let socket_path = crate::socket_path_for_session(&id);
    if !socket_path.exists() {
        eprintln!(
            "error: no session socket found at {}",
            socket_path.display()
        );
        process::exit(1);
    }

    // Connect and send a KILL message.
    match std::os::unix::net::UnixStream::connect(&socket_path) {
        Ok(mut stream) => {
            let msg = crate::protocol::ClientMessage::Kill;
            if let Err(e) = crate::protocol::send_client_message(&mut stream, &msg) {
                eprintln!("error: failed to send KILL: {e}");
                process::exit(1);
            }
            println!("Sent KILL to session {id}");
        }
        Err(e) => {
            eprintln!(
                "error: could not connect to {}: {e}",
                socket_path.display()
            );
            // Try to clean up the stale socket.
            let _ = std::fs::remove_file(&socket_path);
            eprintln!("Removed stale socket file.");
            process::exit(1);
        }
    }
}