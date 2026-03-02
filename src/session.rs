//! A single PTY session managed by `pty-host`.
//!
//! Each `Session` owns:
//! - A PTY master fd (with a shell child on the slave side)
//! - A headless alacritty `Term` that tracks grid state
//! - A Unix domain socket listener for client connections
//! - A proxy loop that shuttles bytes between the master fd and the connected client
//!
//! The session survives client disconnects. When Zed restarts and reconnects to
//! the same socket, the headless terminal's grid is serialised to ANSI escape
//! sequences and replayed to the new client.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use polling::{Event, Events, PollMode, Poller};

use crate::headless::HeadlessTerminal;
use crate::protocol::{
    ClientMessage, DecodeError, HostMessage, WindowSize, HEADER_SIZE, MAX_PAYLOAD_SIZE,
};

// Poller event keys.
const KEY_PTY: usize = 0;
const KEY_CLIENT: usize = 1;
const KEY_SIGCHLD: usize = 2;
const KEY_LISTENER: usize = 3;

/// Read buffer size — same as alacritty's `READ_BUFFER_SIZE`.
const READ_BUF_SIZE: usize = 0x10_0000;

/// Configuration for spawning a new session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Path to the shell program.
    pub program: String,
    /// Arguments to the shell.
    pub args: Vec<String>,
    /// Environment variables for the shell.
    pub env: Vec<(String, String)>,
    /// Working directory for the shell.
    pub working_directory: Option<PathBuf>,
    /// Initial terminal size.
    pub initial_size: WindowSize,
    /// Path for the Unix domain socket.
    pub socket_path: PathBuf,
    /// Maximum number of scrollback lines for the headless terminal emulator.
    pub max_scrollback_lines: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            program: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
            args: Vec::new(),
            env: Vec::new(),
            working_directory: None,
            initial_size: WindowSize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            socket_path: PathBuf::new(),
            max_scrollback_lines: 10_000,
        }
    }
}

/// A running PTY session.
pub struct Session {
    /// The PTY master fd.
    master_fd: RawFd,
    /// Owned fd so it gets closed on drop.
    _master_file: OwnedFd,
    /// The shell child process.
    child: Child,
    /// Socket listener for client connections.
    listener: UnixListener,
    /// Path to the socket file (for cleanup).
    socket_path: PathBuf,
    /// Currently connected client, if any.
    client: Option<UnixStream>,
    /// SIGCHLD notification pipe (read end).
    sigchld_pipe: std::os::unix::net::UnixStream,
    /// SIGCHLD signal-hook registration ID (for cleanup).
    sigchld_id: signal_hook::SigId,
    /// Headless terminal emulator for state tracking and grid serialisation.
    headless: HeadlessTerminal,
    /// Whether the child has exited.
    child_exited: bool,
    /// Raw exit status if the child has exited.
    exit_status: Option<i32>,
}

impl Session {
    /// Create a new session: allocate a PTY, spawn the shell, bind the socket.
    pub fn spawn(config: &SessionConfig) -> anyhow::Result<Self> {
        // Create PTY pair.
        let pty = rustix_openpty::openpty(
            None,
            Some(&to_winsize(&config.initial_size)),
        )?;
        let master = pty.controller;
        let slave = pty.user;

        let master_raw = master.as_raw_fd();

        // Set master to non-blocking.
        set_nonblocking(master_raw)?;

        // UTF-8 on the master.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix_openpty::rustix::termios::{self, InputModes, OptionalActions};
            if let Ok(mut attrs) = termios::tcgetattr(&master) {
                attrs.input_modes.set(InputModes::IUTF8, true);
                let _ = termios::tcsetattr(&master, OptionalActions::Now, &attrs);
            }
        }

        // Build the child command, wrapping for login shell behaviour.
        let mut cmd = build_shell_command(&config.program, &config.args);

        // Wire slave fd as stdin/stdout/stderr.
        let slave_clone1 = slave.try_clone()?;
        let slave_clone2 = slave.try_clone()?;
        cmd.stdin(slave);
        cmd.stdout(slave_clone1);
        cmd.stderr(slave_clone2);

        // Environment.
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        // Working directory.
        if let Some(ref cwd) = config.working_directory {
            cmd.current_dir(cwd);
        }

        let master_fd_to_close = master_raw;
        // Safety: pre_exec runs after fork, before exec.
        unsafe {
            cmd.pre_exec(move || {
                // New session.
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                // Set controlling terminal.
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // Reset signals.
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
                libc::signal(libc::SIGALRM, libc::SIG_DFL);
                // Close the master fd in the child (inherited from parent).
                libc::close(master_fd_to_close);
                Ok(())
            });
        }

        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn '{}': {}",
                config.program,
                e
            )
        })?;

        // Register SIGCHLD pipe.
        let (sigchld_sender, sigchld_receiver) = std::os::unix::net::UnixStream::pair()?;
        let sigchld_id = signal_hook::low_level::pipe::register(
            signal_hook::consts::SIGCHLD,
            sigchld_sender,
        )?;
        sigchld_receiver.set_nonblocking(true)?;

        // Remove stale socket file if it exists.
        let _ = fs::remove_file(&config.socket_path);

        // Ensure the parent directory exists (owner-only access).
        if let Some(parent) = config.socket_path.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        // Bind the listener. The parent directory is already 0700, so the
        // socket inherits protection regardless of the user's umask.
        let listener = UnixListener::bind(&config.socket_path)?;
        listener.set_nonblocking(true)?;

        log::info!(
            "Session started: pid={}, socket={}",
            child.id(),
            config.socket_path.display()
        );

        Ok(Session {
            master_fd: master_raw,
            _master_file: master,
            child,
            listener,
            socket_path: config.socket_path.clone(),
            client: None,
            sigchld_pipe: sigchld_receiver,
            sigchld_id: sigchld_id,
            headless: HeadlessTerminal::new(&config.initial_size, config.max_scrollback_lines),
            child_exited: false,
            exit_status: None,
        })
    }

    /// The child PID.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the session event loop. Blocks until the child exits AND no client
    /// is connected, or until a KILL message is received.
    pub fn run(&mut self) -> anyhow::Result<()> {
        let poller = Arc::new(Poller::new()?);

        // Register the master fd for reading (always) and the SIGCHLD pipe.
        unsafe {
            poller.add_with_mode(
                self.master_fd,
                Event::readable(KEY_PTY),
                PollMode::Level,
            )?;
            poller.add_with_mode(
                &self.sigchld_pipe,
                Event::readable(KEY_SIGCHLD),
                PollMode::Level,
            )?;
            poller.add_with_mode(
                &self.listener,
                Event::readable(KEY_LISTENER),
                PollMode::Level,
            )?;
        }

        let mut events = Events::with_capacity(NonZeroUsize::new(64).unwrap());
        let mut pty_read_buf = vec![0u8; READ_BUF_SIZE];
        let mut client_read_buf = vec![0u8; HEADER_SIZE + MAX_PAYLOAD_SIZE as usize];
        let mut client_read_pos: usize = 0;
        let mut pty_write_queue: Vec<u8> = Vec::new();
        let mut pty_wants_write = false;
        let mut client_write_queue: Vec<u8> = Vec::new();
        let mut pty_read_paused = false;

        'event_loop: loop {
            // If the child has exited and there's no client, we're done.
            if self.child_exited && self.client.is_none() {
                log::info!("Child exited and no client connected, shutting down");
                break 'event_loop;
            }

            let timeout = if self.child_exited {
                // If child exited but client is connected, give them a moment to
                // read the exit message, then shut down.
                Some(Duration::from_secs(5))
            } else {
                None
            };

            events.clear();
            match poller.wait(&mut events, timeout) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }

            // Timeout with exited child — force shutdown.
            if events.is_empty() && self.child_exited {
                log::info!("Timeout after child exit, shutting down");
                break 'event_loop;
            }

            for event in events.iter() {
                match event.key {
                    KEY_LISTENER => {
                        // Accept a new client connection.
                        match self.listener.accept() {
                            Ok((stream, _addr)) => {
                                // Disconnect previous client if any.
                                if let Some(old) = self.client.take() {
                                    // Notify the old client that it's being evicted
                                    // so its reader thread sees a clean message and
                                    // can shut down immediately.
                                    let mut old_writer = &old;
                                    let evict_msg = HostMessage::Error(
                                        "evicted: another client connected".to_string(),
                                    );
                                    let _ = crate::protocol::send_host_message(
                                        &mut old_writer,
                                        &evict_msg,
                                    );
                                    let _ = poller.delete(&old);
                                    log::info!("Evicted previous client");
                                    // `old` is dropped here → socket closed
                                }

                                // The accepted stream inherits non-blocking mode
                                // from the listener on macOS. Force it to blocking
                                // so the handshake writes below are guaranteed to
                                // complete fully.
                                stream.set_nonblocking(false)?;

                                // Send the handshake messages (READY + replay +
                                // REPLAY_DONE) while the socket is still in
                                // BLOCKING mode. This guarantees all bytes reach
                                // the client even if the data is larger than the
                                // kernel socket buffer. We only switch to
                                // non-blocking AFTER the handshake is complete.

                                // Send READY message.
                                let mut writer = &stream;
                                let current_size = self.headless.current_size();
                                let ready = HostMessage::Ready { version: crate::protocol::PROTOCOL_VERSION, pid: self.pid(), cols: current_size.cols, rows: current_size.rows };
                                if let Err(e) = crate::protocol::send_host_message(&mut writer, &ready) {
                                    log::warn!("Failed to send READY to new client: {e}");
                                }

                                // Serialize the headless terminal's full state
                                // (grids, cursor, modes, scroll region) and send
                                // it to the new client as a Snapshot message.
                                let snapshot = self.headless.snapshot();
                                log::info!(
                                    "Sending snapshot to client: {} bytes",
                                    snapshot.len()
                                );
                                if !snapshot.is_empty() {
                                    let snapshot_msg = HostMessage::Snapshot(snapshot);
                                    if let Err(e) = crate::protocol::send_host_message(
                                        &mut writer,
                                        &snapshot_msg,
                                    ) {
                                        log::warn!("Failed to send snapshot to client: {e}");
                                    }
                                }

                                // Signal that replay is complete so the client
                                // can stop draining and switch to live mode.
                                if let Err(e) = crate::protocol::send_host_message(
                                    &mut writer,
                                    &HostMessage::ReplayDone,
                                ) {
                                    log::warn!("Failed to send REPLAY_DONE to client: {e}");
                                }

                                // If child already exited, notify immediately.
                                if self.child_exited {
                                    let exit_msg = HostMessage::ChildExit {
                                        raw_status: self.exit_status,
                                    };
                                    if let Err(e) = crate::protocol::send_host_message(
                                        &mut writer,
                                        &exit_msg,
                                    ) {
                                        log::warn!("Failed to send CHILD_EXIT to client: {e}");
                                    }
                                }

                                // NOW switch to non-blocking for the event loop.
                                stream.set_nonblocking(true)?;

                                // Register the new client for reading.
                                unsafe {
                                    poller.add_with_mode(
                                        &stream,
                                        Event::readable(KEY_CLIENT),
                                        PollMode::Level,
                                    )?;
                                }

                                self.client = Some(stream);
                                client_read_pos = 0;
                                log::info!("Client connected");

                                // Send SIGWINCH to the foreground process group
                                // so full-screen apps redraw themselves after
                                // reconnection, following tmux's approach. The
                                // kernel only auto-delivers SIGWINCH when the
                                // size *changes* via TIOCSWINSZ, so we send it
                                // explicitly to cover the same-size case.
                                if !self.child_exited {
                                    let pgrp = unsafe { libc::tcgetpgrp(self.master_fd) };
                                    if pgrp > 0 {
                                        unsafe { libc::kill(-pgrp, libc::SIGWINCH) };
                                    }
                                }
                            }
                            Err(e) if would_block(&e) => {}
                            Err(e) => {
                                log::warn!("Accept error: {e}");
                            }
                        }
                    }

                    KEY_PTY => {
                        if event.readable && !self.child_exited {
                            // Read from PTY master.
                            match read_nonblocking(self.master_fd, &mut pty_read_buf) {
                                Ok(0) => {
                                    // EOF on master — child side hung up.
                                }
                                Ok(n) => {
                                    let data = &pty_read_buf[..n];

                                    // Feed through the headless terminal so grid
                                    // state is always up-to-date for replay.
                                    self.headless.process(data);

                                    // Queue for client if connected.
                                    if self.client.is_some() {
                                        let msg = HostMessage::Data(data.to_vec());
                                        client_write_queue.extend_from_slice(&msg.encode());

                                        // If the queue is getting large, pause PTY
                                        // reads so the child process backs up
                                        // naturally via kernel PTY flow control.
                                        const HIGH_WATER: usize = 1024 * 1024; // 1 MiB
                                        if client_write_queue.len() >= HIGH_WATER && !pty_read_paused {
                                            pty_read_paused = true;
                                            let interest = if pty_wants_write {
                                                Event::writable(KEY_PTY)
                                            } else {
                                                Event::none(KEY_PTY)
                                            };
                                            poller.modify_with_mode(
                                                unsafe { std::os::fd::BorrowedFd::borrow_raw(self.master_fd) },
                                                interest,
                                                PollMode::Level,
                                            )?;
                                        }

                                        // Ensure we're polling the client for writability.
                                        if let Some(ref client) = self.client {
                                            poller.modify_with_mode(
                                                client,
                                                Event::all(KEY_CLIENT),
                                                PollMode::Level,
                                            )?;
                                        }
                                    }
                                }
                                Err(e) if would_block(&e) => {}
                                Err(e) => {
                                    // EIO on Linux means slave hung up.
                                    #[cfg(target_os = "linux")]
                                    if e.raw_os_error() == Some(libc::EIO) {
                                        continue;
                                    }
                                    log::error!("PTY read error: {e}");
                                    break 'event_loop;
                                }
                            }
                        }

                        if event.writable && !pty_write_queue.is_empty() {
                            match write_nonblocking(self.master_fd, &pty_write_queue) {
                                Ok(n) => {
                                    pty_write_queue.drain(..n);
                                    if pty_write_queue.is_empty() && pty_wants_write {
                                        // No more data to write — switch back to
                                        // read-only (or none if paused).
                                        pty_wants_write = false;
                                        let interest = if pty_read_paused {
                                            Event::none(KEY_PTY)
                                        } else {
                                            Event::readable(KEY_PTY)
                                        };
                                        poller.modify_with_mode(
                                            unsafe { std::os::fd::BorrowedFd::borrow_raw(self.master_fd) },
                                            interest,
                                            PollMode::Level,
                                        )?;
                                    }
                                }
                                Err(e) if would_block(&e) => {}
                                Err(e) => {
                                    log::error!("PTY write error: {e}");
                                    break 'event_loop;
                                }
                            }
                        }
                    }

                    KEY_CLIENT => {
                        // Drain the outbound write queue when the socket is writable.
                        if event.writable && !client_write_queue.is_empty() {
                            if let Some(ref client) = self.client {
                                match Write::write(&mut &*client, &client_write_queue) {
                                    Ok(0) => {
                                        log::info!("Client write returned 0");
                                        self.disconnect_client(&poller);
                                        client_write_queue.clear();
                                    }
                                    Ok(n) => {
                                        client_write_queue.drain(..n);

                                        // Resume PTY reads if the queue has drained
                                        // below the low-water mark.
                                        const LOW_WATER: usize = 256 * 1024; // 256 KiB
                                        if pty_read_paused && client_write_queue.len() < LOW_WATER {
                                            pty_read_paused = false;
                                            let interest = if pty_wants_write {
                                                Event::all(KEY_PTY)
                                            } else {
                                                Event::readable(KEY_PTY)
                                            };
                                            poller.modify_with_mode(
                                                unsafe { std::os::fd::BorrowedFd::borrow_raw(self.master_fd) },
                                                interest,
                                                PollMode::Level,
                                            )?;
                                        }

                                        // If fully drained, stop polling for writability.
                                        if client_write_queue.is_empty() {
                                            poller.modify_with_mode(
                                                client,
                                                Event::readable(KEY_CLIENT),
                                                PollMode::Level,
                                            )?;
                                        }
                                    }
                                    Err(e) if would_block(&e) => {}
                                    Err(e) => {
                                        log::info!("Client write failed: {e}");
                                        self.disconnect_client(&poller);
                                        client_write_queue.clear();
                                    }
                                }
                            }
                        }

                        if !event.readable {
                            continue;
                        }

                        let Some(ref client) = self.client else {
                            continue;
                        };

                        // Read framed messages from the client.
                        match read_client_bytes(
                            client,
                            &mut client_read_buf,
                            &mut client_read_pos,
                        ) {
                            Ok(messages) => {
                                for msg in messages {
                                    match msg {
                                        ClientMessage::Data(bytes) => {
                                            if !bytes.is_empty() {
                                                pty_write_queue.extend_from_slice(&bytes);
                                                if !pty_wants_write {
                                                    pty_wants_write = true;
                                                    poller.modify_with_mode(
                                                        unsafe {
                                                            std::os::fd::BorrowedFd::borrow_raw(
                                                                self.master_fd,
                                                            )
                                                        },
                                                        Event::all(KEY_PTY),
                                                        PollMode::Level,
                                                    )?;
                                                }
                                            }
                                        }
                                        ClientMessage::Resize(size) => {
                                            pty_resize(self.master_fd, &size);
                                            self.headless.resize(&size);
                                        }
                                        ClientMessage::Detach => {
                                            log::info!("Client sent DETACH");
                                            self.disconnect_client(&poller);
                                        }
                                        ClientMessage::Kill => {
                                            log::info!("Client sent KILL");
                                            self.kill_child();
                                            self.disconnect_client(&poller);
                                            break 'event_loop;
                                        }
                                    }
                                }
                            }
                            Err(ReadClientError::Disconnected) => {
                                log::info!("Client disconnected");
                                self.disconnect_client(&poller);
                            }
                            Err(ReadClientError::Io(e)) => {
                                log::warn!("Client read error: {e}");
                                self.disconnect_client(&poller);
                            }
                            Err(ReadClientError::Protocol(e)) => {
                                log::warn!("Client protocol error: {e}");
                                self.disconnect_client(&poller);
                            }
                        }
                    }

                    KEY_SIGCHLD => {
                        // Drain the signal pipe.
                        let mut sig_buf = [0u8; 32];
                        let _ = self.sigchld_pipe.read(&mut sig_buf);

                        // Check the child.
                        match self.child.try_wait() {
                            Ok(Some(status)) => {
                                #[cfg(unix)]
                                let raw = {
                                    use std::os::unix::process::ExitStatusExt;
                                    status.into_raw()
                                };
                                #[cfg(not(unix))]
                                let raw = status.code().unwrap_or(-1);

                                log::info!("Child exited with raw status {raw}");
                                self.child_exited = true;
                                self.exit_status = Some(raw);

                                // Notify client if connected.
                                if let Some(ref client) = self.client {
                                    let msg = HostMessage::ChildExit {
                                        raw_status: Some(raw),
                                    };
                                    let mut writer = client;
                                    let _ = crate::protocol::send_host_message(
                                        &mut writer, &msg,
                                    );
                                }
                            }
                            Ok(None) => {
                                // Child still running — spurious SIGCHLD.
                            }
                            Err(e) => {
                                log::warn!("Error checking child status: {e}");
                            }
                        }
                    }

                    _ => {}
                }
            }

            // If the client disconnected during this iteration, clear the
            // outbound queue and resume PTY reads if they were paused.
            if self.client.is_none() && !client_write_queue.is_empty() {
                client_write_queue.clear();
            }
            if self.client.is_none() && pty_read_paused {
                pty_read_paused = false;
                let interest = if pty_wants_write {
                    Event::all(KEY_PTY)
                } else {
                    Event::readable(KEY_PTY)
                };
                poller.modify_with_mode(
                    unsafe { std::os::fd::BorrowedFd::borrow_raw(self.master_fd) },
                    interest,
                    PollMode::Level,
                )?;
            }
        }

        Ok(())
    }

    fn disconnect_client(&mut self, poller: &Poller) {
        if let Some(client) = self.client.take() {
            let _ = poller.delete(&client);
            // Drop closes the socket.
        }
    }

    fn kill_child(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGHUP);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Unregister signal handler.
        signal_hook::low_level::unregister(self.sigchld_id);

        // Clean up socket file.
        let _ = fs::remove_file(&self.socket_path);

        // If child is still running, be polite.
        if !self.child_exited {
            self.kill_child();
        }

        log::info!("Session cleaned up: {}", self.socket_path.display());
    }
}



// ---------------------------------------------------------------------------
// Login shell command builder
// ---------------------------------------------------------------------------

/// Build a `Command` that spawns the shell as a login shell, mirroring
/// alacritty's `default_shell_command()` in `tty/unix.rs`.
///
/// On macOS this wraps the shell in `/usr/bin/login -flp ...` which handles
/// utmp/wtmp accounting (producing the `Last login:` banner) and sets up a
/// proper TTY session. On other Unix systems, we set argv[0] to `-shellname`
/// which is the POSIX convention for login shells.
///
/// If `args` is non-empty (i.e. the user explicitly configured shell arguments),
/// we skip the login wrapping and just run the shell directly — the user knows
/// what they're doing.
fn build_shell_command(shell: &str, args: &[String]) -> Command {
    if !args.is_empty() {
        let mut cmd = Command::new(shell);
        cmd.args(args);
        return cmd;
    }

    #[cfg(target_os = "macos")]
    {
        build_macos_login_command(shell)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let shell_name = shell.rsplit('/').next().unwrap_or(shell);
        let mut cmd = Command::new(shell);
        cmd.arg0(format!("-{shell_name}"));
        cmd
    }
}

#[cfg(target_os = "macos")]
fn build_macos_login_command(shell: &str) -> Command {
    let shell_name = shell.rsplit('/').next().unwrap_or(shell);

    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());

    // exec with argv[0] prefixed by '-' so the shell runs as a login shell.
    // `login -l` skips the argv[0] prefix, so we do it ourselves via exec -a.
    let exec = format!("exec -a -{shell_name} {shell}");

    // Check for .hushlogin in the user's home directory. `login` only checks
    // the current working directory, so we pass -q ourselves if needed.
    let has_hushlogin = Path::new(&home).join(".hushlogin").exists();

    // -f: bypass authentication (already logged in)
    // -l: don't cd to $HOME or prepend '-' to argv[0] (we handle both)
    // -p: preserve the environment
    // -q: suppress Last login banner (only if .hushlogin exists)
    let flags = if has_hushlogin { "-qflp" } else { "-flp" };

    let mut cmd = Command::new("/usr/bin/login");
    cmd.args([flags, &user, "/bin/zsh", "-fc", &exec]);
    cmd
}

// ---------------------------------------------------------------------------
// Client read helpers
// ---------------------------------------------------------------------------

enum ReadClientError {
    Disconnected,
    Io(io::Error),
    Protocol(DecodeError),
}

/// Read available bytes from the client socket into the accumulation buffer,
/// then extract as many complete messages as possible.
fn read_client_bytes(
    client: &UnixStream,
    buf: &mut [u8],
    pos: &mut usize,
) -> Result<Vec<ClientMessage>, ReadClientError> {
    // Read whatever's available.
    let mut stream = client;
    let n = match stream.read(&mut buf[*pos..]) {
        Ok(0) => return Err(ReadClientError::Disconnected),
        Ok(n) => n,
        Err(e) if would_block(&e) => return Ok(Vec::new()),
        Err(e) if e.kind() == ErrorKind::ConnectionReset => {
            return Err(ReadClientError::Disconnected)
        }
        Err(e) => return Err(ReadClientError::Io(e)),
    };
    *pos += n;

    // Extract complete messages.
    let mut messages = Vec::new();
    let mut consumed = 0;

    while consumed + HEADER_SIZE <= *pos {
        let header = &buf[consumed..consumed + HEADER_SIZE];
        let payload_len =
            u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;

        if payload_len > MAX_PAYLOAD_SIZE as usize {
            return Err(ReadClientError::Protocol(DecodeError::PayloadTooLarge(
                payload_len as u32,
            )));
        }

        let frame_len = HEADER_SIZE + payload_len;
        if consumed + frame_len > *pos {
            break; // Incomplete frame, wait for more data.
        }

        let frame = &buf[consumed..consumed + frame_len];
        let mut cursor = std::io::Cursor::new(frame);
        match ClientMessage::decode_from(&mut cursor) {
            Ok(msg) => messages.push(msg),
            Err(e) => return Err(ReadClientError::Protocol(e)),
        }
        consumed += frame_len;
    }

    // Compact: move unconsumed bytes to the front.
    if consumed > 0 {
        buf.copy_within(consumed..*pos, 0);
        *pos -= consumed;
    }

    Ok(messages)
}

// ---------------------------------------------------------------------------
// Low-level I/O helpers
// ---------------------------------------------------------------------------

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_nonblocking(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_nonblocking(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn would_block(e: &io::Error) -> bool {
    e.kind() == ErrorKind::WouldBlock
}

fn pty_resize(master_fd: RawFd, size: &WindowSize) {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.cols * size.cell_width,
        ws_ypixel: size.rows * size.cell_height,
    };
    unsafe {
        libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws as *const _);
    }
}

fn to_winsize(
    size: &WindowSize,
) -> rustix_openpty::rustix::termios::Winsize {
    rustix_openpty::rustix::termios::Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.cols * size.cell_width,
        ws_ypixel: size.rows * size.cell_height,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_spawn_and_exit() {
        // Spawn a session that immediately exits.
        // Use a short path to stay under the 108-byte SUN_LEN limit for
        // Unix domain sockets. $TMPDIR on macOS is already long
        // (/var/folders/xx/…/T/), so we keep the subdirectory name minimal.
        let short_id = &uuid::Uuid::new_v4().to_string()[..8];
        let dir = std::env::temp_dir().join(format!("pht-{short_id}"));
        let _ = fs::create_dir_all(&dir);
        let socket_path = dir.join("s.sock");
        let socket_path_clone = socket_path.clone();

        let config = SessionConfig {
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo hello && exit 0".to_string()],
            socket_path: socket_path.clone(),
            max_scrollback_lines: 100,
            ..Default::default()
        };

        let mut session = Session::spawn(&config).expect("session should spawn");
        assert!(session.pid() > 0);
        assert!(socket_path.exists());

        // Run the session in a background thread.
        let handle = std::thread::spawn(move || {
            session.run().ok();
        });

        // Connect a client briefly, then disconnect so the session can shut
        // down once the child exits. The shell command exits immediately, so
        // the session should see: child exit + no client → done.
        {
            let _client = UnixStream::connect(&socket_path_clone).ok();
            // Small delay to let the READY + DATA messages flow.
            std::thread::sleep(Duration::from_millis(100));
            // _client drops here → session sees disconnect
        }

        // Join with a timeout to prevent hanging the test suite.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if handle.is_finished() {
                handle.join().expect("session thread panicked");
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("session did not shut down within 5 seconds");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Socket should be cleaned up.
        assert!(!socket_path.exists());

        // Clean up temp dir.
        let _ = fs::remove_dir_all(&dir);
    }
}