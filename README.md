# pty_host

A lightweight PTY shepherd daemon that holds terminal sessions across client
restarts. When the client quits and relaunches, the shell processes are still
alive — `pty-host` held the PTY master fd the whole time.

## The Problem

Many terminal-hosting applications have no in-process hot-reload. Every
"reload" is a full process restart:

1. The old process dies — all file descriptors close
2. Every shell receives SIGHUP and terminates

Terminal scrollback, running processes, environment state — all gone.

## The Solution

Move PTY ownership out of the client into a tiny external process. The shell
doesn't care who's reading its output. It only gets SIGHUP when the master fd
closes. If something else holds that fd, the shell lives forever.

```
Before (in-process PTY):

  ┌──────────────────────────────────┐
  │        client process            │
  │                                  │
  │  EventLoop thread                │
  │    owns Pty { file: master_fd }  │
  │                                  │
  │  Terminal::drop()                │
  │    → Msg::Shutdown               │
  │    → Pty::drop() → SIGHUP        │──── shell dies
  └──────────────────────────────────┘

After (PTY host):

  ┌────────────────────┐         ┌──────────────────────────┐
  │  client process    │  Unix   │       pty-host           │
  │                    │ socket  │                          │
  │  EventLoop thread  │◄═══════►│  holds master_fd         │
  │    reads/writes    │  framed │  proxies master ↔ socket │
  │    the socket      │  msgs   │  survives client restart │
  │                    │         │                          │
  │  on restart:       │         │  shell never sees SIGHUP │
  │    reconnect to    │         │                          │
  │    existing socket │         │                          │
  └────────────────────┘         └──────────┬───────────────┘
                                            │
                                            │ master fd (owned)
                                            ▼
                                       ┌──────────┐
                                       │  shell   │
                                       │ (zsh,    │
                                       │  fish..) │
                                       └──────────┘
```

## Architecture

**One process per session.** Each `pty-host` invocation manages exactly one
PTY. The client spawns one per terminal tab. This keeps things simple — no
shared state, no multiplexing, no control socket. If one host crashes, only
one terminal is affected.

### Session lifecycle

#### 1. Terminal creation (new session)

The client spawns the host:

```sh
pty-host \
  --session 550e8400-e29b-41d4-a716-446655440000 \
  --shell /bin/zsh \
  --cwd /Users/me/project \
  --cols 120 --rows 40
```

If `--shell` is omitted, the host uses `$SHELL` (falling back to `/bin/sh`).

The host:
1. Calls `openpty()` to create a master/slave PTY pair
2. Spawns the shell on the slave side (`setsid`, `TIOCSCTTY`, etc.)
3. Creates a Unix socket at `$TMPDIR/pty-host/<session-id>.sock`
4. Prints the socket path to stdout (so the parent can read it)
5. Enters its event loop, waiting for a client to connect

The client:
1. Reads the socket path from the host's stdout
2. Connects to the socket
3. Receives a `READY { version, pid }` message
4. Plugs the socket into alacritty's `EventLoop` (which is already
   generic over `EventedPty`)
5. Persists the `session_id` for later reconnection

#### 2. Normal operation

```
User types "ls\n"
  → Client sends ClientMessage::Data(b"ls\r")
  → Host writes b"ls\r" to master fd
  → Shell executes ls, writes output to slave
  → Host reads output from master fd
  → Host sends HostMessage::Data(output) to client
  → Client's alacritty parser renders the output
```

Resize works the same way — the client sends
`ClientMessage::Resize { cols, rows, .. }` and the host calls
`ioctl(TIOCSWINSZ)` on the master fd.

#### 3. Client restart (the whole point)

When the client is about to quit:
1. Client sends `ClientMessage::Detach` on each terminal's socket
2. The host receives it, drops the client connection, but keeps running
3. The shell doesn't notice — master fd is still open
4. Client exits. The host keeps running.

When the client relaunches:
1. Reads `session_id` from persistent storage
2. Checks if `$TMPDIR/pty-host/<session-id>.sock` exists
3. Connects to the socket
4. Host sends `READY { version, pid }` followed by grid replay data
   (zero or more `DATA` messages, then a `REPLAY_DONE` sentinel)
5. Terminal is back — same shell, same state, same running processes

If the socket doesn't exist (host crashed, system rebooted), the client
falls back to creating a new terminal the normal way.

#### 4. Shell exits

When the shell exits (user types `exit`, process killed, etc.):
1. Host detects child exit via SIGCHLD
2. Host sends `HostMessage::ChildExit { raw_status }` to the client
3. If a client is connected, the host waits up to 5 seconds for it to
   disconnect, then exits
4. If no client is connected, the host exits immediately
5. Socket file is cleaned up on drop

#### 5. Kill

Client sends `ClientMessage::Kill`:
1. Host sends SIGHUP to the shell (the standard "terminal closed" signal)
2. The shell runs its trap handlers, saves history, and exits
3. Host cleans up and exits

## Wire Protocol

Simple length-prefixed binary framing, optimised for the hot path (`DATA`
messages):

```
┌──────────┬────────────────┬─────────────────────────┐
│ type: u8 │ length: u32 LE │ payload: [u8; length]    │
└──────────┴────────────────┴─────────────────────────┘
```

5 bytes of overhead per message. Max payload: 16 MiB.

### Client → Host messages

| Tag    | Type     | Payload                                            |
|--------|----------|----------------------------------------------------|
| `0x01` | DATA     | Raw bytes (terminal input)                         |
| `0x02` | RESIZE   | cols: u16, rows: u16, cell_w: u16, cell_h: u16    |
| `0x03` | DETACH   | (empty) — disconnect, keep session alive           |
| `0x04` | KILL     | (empty) — terminate the shell                      |

### Host → Client messages

| Tag    | Type        | Payload                                         |
|--------|-------------|-------------------------------------------------|
| `0x81` | DATA        | Raw bytes (terminal output)                     |
| `0x82` | CHILD_EXIT  | has: u8, raw_status: i32                        |
| `0x83` | READY       | version: u32, pid: u32                          |
| `0x84` | ERROR       | UTF-8 error message                             |
| `0x85` | REPLAY_DONE | (empty) — end of grid replay, switch to live    |

Client tags use `0x01–0x7F`, host tags use `0x81–0xFF`. An unknown tag is a
protocol error that disconnects the client.

## Grid State Recovery

When a client reconnects, the terminal grid is empty — the old `Term<T>`
struct was destroyed with the old process. The host maintains a **headless
terminal emulator** (`HeadlessTerminal`) that processes every byte of PTY
output through an alacritty `Term` instance, keeping a shadow copy of the
grid state.

On reconnect, the host captures a binary snapshot of the terminal state via
`HeadlessTerminal::snapshot()`:
- Serialises both grid buffers (primary and alternate screen), cursor
  position, terminal mode flags, and scroll region into a `TermState` struct
- Encodes the struct with bincode for compact binary transfer

The serialised bytes are sent as one or more `DATA` messages, followed by a
`REPLAY_DONE` sentinel. The client deserialises the `TermState` and calls
`term.restore(state)` to reconstruct the grid before entering the live event
loop.

This recovers the full grid contents, colours, text attributes, cursor
position, terminal modes (mouse mode, alternate screen, bracketed paste),
and scroll region.

## Socket Location

```
$TMPDIR/pty-host/
├── 550e8400-e29b-41d4-a716-446655440000.sock
├── 6ba7b810-9dad-11d1-80b4-00c04fd430c8.sock
└── ...
```

`$TMPDIR` is the platform's secure temporary directory (per-user on macOS).
Each session gets a UUID. The mapping from application state to `session_id`
is left to the client.

## CLI

```
pty-host --session <UUID> [OPTIONS]

Create and manage a PTY session.

Options:
  --session <UUID>       Session ID (determines socket path)
  --shell <PATH>         Shell program (default: $SHELL or /bin/sh)
  --arg <ARG>            Shell argument (repeatable)
  --cwd <DIR>            Working directory
  --env <KEY=VALUE>      Environment variable (repeatable)
  --cols <N>             Initial columns (default: 80)
  --rows <N>             Initial rows (default: 24)
  --scrollback <LINES>   Maximum scrollback lines (default: 10000)
  --socket <PATH>        Override socket path

Management:
  --list                 List active sessions and exit
  --kill <UUID>          Kill a session by ID
  --attach <UUID>        Attach to a session as a dumb terminal client
  -h, --help             Show help
```

### Examples

```sh
# Start a new session with the system default shell
pty-host --session $(uuidgen) --cwd ~/project

# Start a session with a specific shell
pty-host --session $(uuidgen) --shell /bin/zsh

# List active sessions
pty-host --list

# Kill a session
pty-host --kill 550e8400-e29b-41d4-a716-446655440000

# Attach interactively (for debugging)
pty-host --attach 550e8400-e29b-41d4-a716-446655440000
```

## Client Integration

### Persistence

The client should persist `session_id` so it can reconnect after a restart.
For example, a SQLite-backed client might add a column:

```sql
ALTER TABLE terminals ADD COLUMN session_id TEXT;
```

On save: store the `session_id`.
On restore: attempt reconnect; fall back to new terminal if gone.

### Alacritty EventLoop

The alacritty fork's `EventLoop` is already generic:

```rust
pub struct EventLoop<T: tty::EventedPty, U: EventListener> {
    pty: T,  // ← generic over any EventedPty
    // ...
}
```

`PtyHostClient` implements `EventedPty` for the Unix socket connection.
Internally it uses a deframing thread to bridge the framed protocol to
alacritty's raw byte read/write expectations:

- A background thread reads framed messages from the socket
- `DATA` payloads are written to an internal pipe (registered with the poller)
- `CHILD_EXIT` signals are delivered via a separate pipe
- Writes go through a `FramingWriter` that encodes raw bytes as `DATA` frames

**No changes to the alacritty fork's event loop are needed** — just a new
`impl EventedPty`.

## Crate Structure

```
pty-host/
├── Cargo.toml
├── README.md
├── LICENSE
└── src/
    ├── lib.rs             # Public API: socket paths, session discovery, spawn_host()
    ├── main.rs            # Binary entry point
    ├── daemon.rs          # CLI parsing, session launch, attach mode
    ├── protocol.rs        # Wire protocol: message types, encode/decode, framing
    ├── session.rs         # Session: PTY creation, shell spawn, I/O proxy loop
    ├── client.rs          # Client: PtyHostClient (EventedPty impl), deframing thread
    └── headless.rs        # Headless terminal: grid state tracking, serde snapshot
```

The crate is both a library (shared types for a client to import) and a
binary (`pty-host`). Dependencies are minimal — same PTY/IO stack as
alacritty (`polling`, `rustix-openpty`, `signal-hook`), plus `uuid` for
session IDs and `anyhow`/`log`/`env_logger` for error handling and logging.

## What This Doesn't Solve

- **System reboot**: The host process dies too. This survives client restarts,
  not system reboots.
- **Remote terminals**: SSH terminals have their own reconnection story
  (mosh, SSH multiplexing). The PTY host is for local terminals.
- **Task terminals**: Fire-and-forget commands, not interactive sessions.
  Re-running the task is the right answer, not session persistence.
- **Windows**: Not yet. The PTY host uses Unix domain sockets and `openpty`.
  Windows ConPTY + named pipes would be a separate implementation.

## Known Issues

- **Socket path via stdout.** Fragile — anything the shell or environment
  setup prints to stdout breaks the protocol. The socket path is already
  deterministic (`$TMPDIR/pty-host/<uuid>.sock`), so this transport could
  be eliminated entirely.
- **Single client.** One connection per session — no multi-attach.
