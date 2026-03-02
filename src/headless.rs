//! Headless terminal emulator for state tracking and grid serialisation.
//!
//! Wraps alacritty's `Term<VoidListener>` to process all PTY output through a
//! real terminal emulator running inside the host process. On reconnect, the
//! terminal state (grids, cursor, modes, scroll region) is serialised via
//! serde/bincode and sent to the client, which deserialises and restores it
//! directly.
//!
//! The serialisation logic lives in `alacritty_terminal::term::serialize` — this module only
//! provides the `HeadlessTerminal` wrapper that owns the `Term` and `Processor`.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Config;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi;

use crate::protocol::WindowSize;

/// A headless terminal emulator that tracks terminal state without rendering.
///
/// Feeds all PTY output through alacritty's parser so the grid, scrollback,
/// cursor, and modes are always up-to-date. On reconnect, the grid can be
/// serialised to an escape sequence stream for replay.
pub struct HeadlessTerminal {
    term: Term<VoidListener>,
    parser: ansi::Processor,
}

impl HeadlessTerminal {
    /// Create a new headless terminal with the given dimensions and scrollback.
    pub fn new(size: &WindowSize, max_scrollback_lines: usize) -> Self {
        let config = Config {
            scrolling_history: max_scrollback_lines,
            ..Default::default()
        };
        let term_size = TermSize::new(size.cols as usize, size.rows as usize);
        #[cfg(feature = "alacritty-graphics")]
        let term_size = {
            let mut s = term_size;
            s.cell_width = size.cell_width.max(1) as f32;
            s.cell_height = size.cell_height.max(1) as f32;
            s
        };
        let term = Term::new(config, &term_size, VoidListener);
        let parser = ansi::Processor::new();

        Self { term, parser }
    }

    /// Feed raw bytes from the PTY into the terminal emulator.
    ///
    /// This updates the grid, cursor, scrollback, modes, etc. — exactly the
    /// same processing that happens in alacritty's `EventLoop::pty_read`.
    ///
    /// Panics inside alacritty's parser (e.g. sixel handling with zero-sized
    /// cell dimensions) are caught so the pty-host process survives. The
    /// real client still receives the raw data — only the headless state
    /// tracker skips the problematic input.
    pub fn process(&mut self, bytes: &[u8]) {
        // AssertUnwindSafe: the parser and term may be left in a slightly
        // inconsistent state after a panic, but that only affects replay
        // fidelity — the live PTY stream is unaffected.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parser.advance(&mut self.term, bytes);
        }));
        if let Err(payload) = result {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            log::error!(
                "headless terminal parser panicked, skipping {} bytes: {msg}",
                bytes.len()
            );
        }
    }

    /// Notify the headless terminal of a resize.
    pub fn resize(&mut self, size: &WindowSize) {
        let new_size = TermSize::new(size.cols as usize, size.rows as usize);
        #[cfg(feature = "alacritty-graphics")]
        let new_size = {
            let mut s = new_size;
            s.cell_width = size.cell_width.max(1) as f32;
            s.cell_height = size.cell_height.max(1) as f32;
            s
        };
        self.term.resize(new_size);
    }

    /// Borrow the underlying alacritty `Term` for direct grid access.
    ///
    /// This allows renderers to read cell contents, colors, and attributes
    /// without going through ANSI serialization.
    pub fn term(&self) -> &Term<VoidListener> {
        &self.term
    }

    /// Return the current terminal dimensions.
    pub fn current_size(&self) -> WindowSize {
        use alacritty_terminal::grid::Dimensions;
        let grid = self.term.grid();
        WindowSize {
            cols: grid.columns() as u16,
            rows: grid.screen_lines() as u16,
            cell_width: 0,
            cell_height: 0,
        }
    }

    /// Capture a serialised snapshot of the full terminal state.
    ///
    /// Returns bincode-serialised `TermState` bytes containing both grid
    /// buffers, cursor, terminal modes, and scroll region. The client
    /// deserialises with `bincode::deserialize::<TermState>(&bytes)` and
    /// calls `term.restore(state)`.
    ///
    pub fn snapshot(&self) -> Vec<u8> {
        let state = self.term.snapshot();
        bincode::serialize(&state).expect("TermState bincode serialization should not fail")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    fn make_size(cols: u16, rows: u16) -> crate::protocol::WindowSize {
        crate::protocol::WindowSize {
            cols,
            rows,
            cell_width: 8,
            cell_height: 16,
        }
    }

    #[test]
    fn create_headless_terminal() {
        let size = make_size(80, 24);
        let term = HeadlessTerminal::new(&size, 1000);
        let grid = term.term.grid();
        assert_eq!(grid.columns(), 80);
        assert_eq!(grid.screen_lines(), 24);
    }

    #[test]
    fn process_simple_text() {
        let size = make_size(80, 24);
        let mut term = HeadlessTerminal::new(&size, 100);
        term.process(b"hello");

        let grid = term.term.grid();
        let row = &grid[alacritty_terminal::index::Line(0)];
        let text: String = (0..5)
            .map(|i| row[alacritty_terminal::index::Column(i)].c)
            .collect();
        assert_eq!(text, "hello");
    }

    #[test]
    fn process_colored_text() {
        let size = make_size(80, 24);
        let mut term = HeadlessTerminal::new(&size, 100);
        // ESC[31m = set fg red, ESC[0m = reset
        term.process(b"\x1b[31mred\x1b[0m");

        let grid = term.term.grid();
        let cell = &grid[alacritty_terminal::index::Line(0)][alacritty_terminal::index::Column(0)];
        assert_eq!(cell.c, 'r');
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn process_multiline() {
        let size = make_size(40, 10);
        let mut term = HeadlessTerminal::new(&size, 100);
        term.process(b"line one\r\nline two");

        let grid = term.term.grid();
        let row0 = &grid[alacritty_terminal::index::Line(0)];
        let row1 = &grid[alacritty_terminal::index::Line(1)];
        assert_eq!(row0[alacritty_terminal::index::Column(0)].c, 'l');
        assert_eq!(row1[alacritty_terminal::index::Column(5)].c, 't');
    }

    #[test]
    fn resize_updates_dimensions() {
        let size = make_size(80, 24);
        let mut term = HeadlessTerminal::new(&size, 100);
        assert_eq!(term.term.grid().columns(), 80);
        assert_eq!(term.term.grid().screen_lines(), 24);

        term.resize(&WindowSize {
            cols: 120,
            rows: 40,
            cell_width: 8,
            cell_height: 16,
        });
        assert_eq!(term.term.grid().columns(), 120);
        assert_eq!(term.term.grid().screen_lines(), 40);
    }
}