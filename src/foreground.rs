//! Foreground-process probe for synthetic terminal title injection.
//!
//! Programs like `claude`, `tmux`, and well-configured shells emit OSC 0/2
//! escape sequences (`\e]0;<title>\a`) to advertise the terminal title.
//! Programs like `top`, `htop`, `vim`, and bare shells do not, so the
//! terminal's title stays stale or empty.
//!
//! For sessions hosted by `pty-host`, we work around this by sampling the
//! foreground process group on the master pty and synthesising an OSC 0
//! sequence when no real title has been emitted recently. The synthetic
//! sequence flows through the normal outbound data stream — Zed sees it as
//! if the program itself had set the title.
//!
//! Key constraints:
//! * Don't fight a program that *is* setting its own title — sniff the
//!   outbound stream for OSC 0/1/2 and snooze injection while one is
//!   recent.
//! * Don't inject mid-escape-sequence — we only inject between PTY reads,
//!   never within one.
//!
//! The probe is structured around two traits — `Clock` and `ForegroundInfoSource` —
//! so the timing-sensitive injection logic can be unit-tested deterministically.
//! Production code uses the system clock and a libc-backed lookup; tests
//! use fakes.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Minimum interval between foreground-pgid samples. Cheap (one syscall +
/// a sysctl/procfs read), but no need to do it on every byte.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// How long after seeing a real OSC 0/1/2 to suppress synthetic titles.
/// Long enough to give the real program a chance to update again before
/// we step on it; short enough that a one-shot title from a program that
/// then exits doesn't permanently lock us out.
pub const REAL_TITLE_QUIET_PERIOD: Duration = Duration::from_secs(3);

/// Foreground-process info gathered via OS-specific lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub name: String,
    pub cwd: Option<PathBuf>,
}

/// Time abstraction so injection logic can be tested deterministically.
pub trait Clock {
    fn now(&self) -> Instant;
}

/// Production clock — wraps `Instant::now`.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Source of foreground-process info for a pty master fd. Production
/// uses libproc/procfs; tests use a controllable mock.
pub trait ForegroundInfoSource {
    fn lookup(&self) -> Option<ProcessInfo>;
}

/// Production source: queries `tcgetpgrp(master_fd)` and looks up the
/// resulting pgid via OS-native APIs.
pub struct PtyForegroundSource {
    master_fd: std::os::unix::io::RawFd,
}

impl PtyForegroundSource {
    pub fn new(master_fd: std::os::unix::io::RawFd) -> Self {
        Self { master_fd }
    }
}

impl ForegroundInfoSource for PtyForegroundSource {
    fn lookup(&self) -> Option<ProcessInfo> {
        let pgid = unsafe { libc::tcgetpgrp(self.master_fd) };
        if pgid <= 0 {
            return None;
        }
        os::lookup_process_info(pgid as u32)
    }
}

/// Per-session state for the foreground-process title probe.
pub struct ForegroundProbe<C: Clock = SystemClock> {
    last_sample: Option<Instant>,
    last_synthesised_title: String,
    last_real_title_at: Option<Instant>,
    sniffer: OscSniffer,
    sample_interval: Duration,
    real_title_quiet_period: Duration,
    clock: C,
}

impl ForegroundProbe<SystemClock> {
    pub fn new() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl Default for ForegroundProbe<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> ForegroundProbe<C> {
    pub fn with_clock(clock: C) -> Self {
        Self {
            last_sample: None,
            last_synthesised_title: String::new(),
            last_real_title_at: None,
            sniffer: OscSniffer::default(),
            sample_interval: SAMPLE_INTERVAL,
            real_title_quiet_period: REAL_TITLE_QUIET_PERIOD,
            clock,
        }
    }

    /// Override timing constants. Used in tests; production callers should
    /// stick with `SAMPLE_INTERVAL` and `REAL_TITLE_QUIET_PERIOD`.
    pub fn with_timing(
        mut self,
        sample_interval: Duration,
        real_title_quiet_period: Duration,
    ) -> Self {
        self.sample_interval = sample_interval;
        self.real_title_quiet_period = real_title_quiet_period;
        self
    }

    /// Feed outbound data (bytes about to be sent to the client) through
    /// the OSC sniffer. If a real OSC 0/1/2 title-set is detected, the
    /// snooze timer is reset.
    pub fn observe_outbound(&mut self, bytes: &[u8]) {
        if self.sniffer.feed(bytes) {
            self.last_real_title_at = Some(self.clock.now());
        }
    }

    /// Returns a synthetic `\e]0;<title>\a` to inject after the most
    /// recent outbound chunk, or `None` if no injection is warranted.
    pub fn maybe_inject<S: ForegroundInfoSource>(&mut self, source: &S) -> Option<Vec<u8>> {
        let now = self.clock.now();

        if let Some(at) = self.last_real_title_at
            && now.saturating_duration_since(at) < self.real_title_quiet_period
        {
            return None;
        }

        if let Some(at) = self.last_sample
            && now.saturating_duration_since(at) < self.sample_interval
        {
            return None;
        }
        self.last_sample = Some(now);

        let info = source.lookup()?;
        let title = format_title(&info);
        if title.is_empty() || title == self.last_synthesised_title {
            return None;
        }
        self.last_synthesised_title = title.clone();

        Some(synthesise_osc_title(&title))
    }
}

fn format_title(info: &ProcessInfo) -> String {
    let cwd_name = info
        .cwd
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    if cwd_name.is_empty() {
        info.name.clone()
    } else {
        format!("{cwd_name} — {}", info.name)
    }
}

fn synthesise_osc_title(title: &str) -> Vec<u8> {
    let bytes = title.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 5);
    out.extend_from_slice(b"\x1b]0;");
    out.extend_from_slice(bytes);
    out.push(0x07);
    out
}

// ---------------------------------------------------------------------------
// OS-specific process lookup
// ---------------------------------------------------------------------------

mod os {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::ProcessInfo;

    #[cfg(target_os = "macos")]
    pub fn lookup_process_info(pid: u32) -> Option<ProcessInfo> {
        let name = macos_proc_name(pid)?;
        let cwd = macos_proc_cwd(pid);
        Some(ProcessInfo { name, cwd })
    }

    #[cfg(target_os = "macos")]
    fn macos_proc_name(pid: u32) -> Option<String> {
        let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = unsafe {
            libc::proc_name(
                pid as i32,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as u32,
            )
        };
        if len <= 0 {
            return None;
        }
        let slice = &buf[..len as usize];
        Some(String::from_utf8_lossy(slice).into_owned())
    }

    #[cfg(target_os = "macos")]
    fn macos_proc_cwd(pid: u32) -> Option<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;

        #[repr(C)]
        struct VnodeInfoPath {
            _vip_vi: [u8; 152],
            vip_path: [u8; 1024],
        }

        impl Default for VnodeInfoPath {
            fn default() -> Self {
                Self {
                    _vip_vi: [0; 152],
                    vip_path: [0; 1024],
                }
            }
        }

        #[repr(C)]
        #[derive(Default)]
        struct ProcVnodepathinfo {
            pvi_cdir: VnodeInfoPath,
            pvi_rdir: VnodeInfoPath,
        }

        const PROC_PIDVNODEPATHINFO: i32 = 9;

        let mut info = ProcVnodepathinfo::default();
        let size = std::mem::size_of::<ProcVnodepathinfo>() as i32;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret != size {
            return None;
        }

        let path_bytes = &info.pvi_cdir.vip_path;
        let nul = path_bytes.iter().position(|&b| b == 0).unwrap_or(0);
        if nul == 0 {
            return None;
        }
        let os = std::ffi::OsString::from_vec(path_bytes[..nul].to_vec());
        Some(std::path::PathBuf::from(os))
    }

    #[cfg(target_os = "linux")]
    pub fn lookup_process_info(pid: u32) -> Option<ProcessInfo> {
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim_end().to_owned())?;
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        Some(ProcessInfo { name, cwd })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn lookup_process_info(_pid: u32) -> Option<super::ProcessInfo> {
        None
    }
}

// ---------------------------------------------------------------------------
// OSC sniffer
// ---------------------------------------------------------------------------

/// Stream-oriented detector for OSC 0/1/2 title-set escape sequences.
///
/// Recognises the sequence:
/// ```text
/// ESC ] (0|1|2) ; <title bytes> (BEL | ESC \)
/// ```
///
/// Returns `true` from `feed()` if at least one complete title-set
/// terminator was observed in the bytes fed.
#[derive(Default)]
struct OscSniffer {
    state: SnifferState,
}

#[derive(Default, PartialEq)]
enum SnifferState {
    #[default]
    Normal,
    /// Saw `ESC`.
    AfterEsc,
    /// Saw `ESC ]`.
    AfterOscOpen,
    /// Saw `ESC ] [012]`, expecting `;`.
    AfterOscCode,
    /// Inside the OSC payload, looking for terminator.
    InOscPayload,
    /// Saw `ESC` while inside payload, expecting `\` for ST.
    AfterPayloadEsc,
}

impl OscSniffer {
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut saw_complete = false;
        for &b in bytes {
            match self.state {
                SnifferState::Normal => {
                    if b == 0x1b {
                        self.state = SnifferState::AfterEsc;
                    }
                }
                SnifferState::AfterEsc => {
                    self.state = if b == b']' {
                        SnifferState::AfterOscOpen
                    } else {
                        SnifferState::Normal
                    };
                }
                SnifferState::AfterOscOpen => {
                    self.state = if matches!(b, b'0' | b'1' | b'2') {
                        SnifferState::AfterOscCode
                    } else {
                        SnifferState::Normal
                    };
                }
                SnifferState::AfterOscCode => {
                    self.state = if b == b';' {
                        SnifferState::InOscPayload
                    } else {
                        SnifferState::Normal
                    };
                }
                SnifferState::InOscPayload => {
                    if b == 0x07 {
                        saw_complete = true;
                        self.state = SnifferState::Normal;
                    } else if b == 0x1b {
                        self.state = SnifferState::AfterPayloadEsc;
                    }
                }
                SnifferState::AfterPayloadEsc => {
                    if b == b'\\' {
                        saw_complete = true;
                    }
                    self.state = SnifferState::Normal;
                }
            }
        }
        saw_complete
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;

    // ----- OSC sniffer tests --------------------------------------------

    #[test]
    fn sniffer_recognises_bel_terminated() {
        let mut s = OscSniffer::default();
        assert!(s.feed(b"\x1b]0;hello\x07"));
    }

    #[test]
    fn sniffer_recognises_st_terminated() {
        let mut s = OscSniffer::default();
        assert!(s.feed(b"\x1b]2;world\x1b\\"));
    }

    #[test]
    fn sniffer_recognises_osc_1_icon_name() {
        // OSC 1 is icon name; some terminals treat it as title-equivalent.
        let mut s = OscSniffer::default();
        assert!(s.feed(b"\x1b]1;icon\x07"));
    }

    #[test]
    fn sniffer_recognises_split_across_feeds() {
        let mut s = OscSniffer::default();
        assert!(!s.feed(b"\x1b]0;hel"));
        assert!(s.feed(b"lo\x07"));
    }

    #[test]
    fn sniffer_ignores_osc_7_cwd() {
        // OSC 7 is cwd advertisement, not title.
        let mut s = OscSniffer::default();
        assert!(!s.feed(b"\x1b]7;file:///tmp\x07"));
    }

    #[test]
    fn sniffer_ignores_osc_133_prompt() {
        // OSC 133 is shell-integration semantic prompt; not title.
        let mut s = OscSniffer::default();
        assert!(!s.feed(b"\x1b]133;A\x07"));
    }

    #[test]
    fn sniffer_ignores_plain_text() {
        let mut s = OscSniffer::default();
        assert!(!s.feed(b"hello world\nrunning command\n"));
    }

    #[test]
    fn sniffer_recovers_from_malformed_escape() {
        // ESC followed by something other than ] resets to Normal.
        let mut s = OscSniffer::default();
        assert!(!s.feed(b"\x1bA"));
        // Sniffer back to Normal; subsequent valid OSC should work.
        assert!(s.feed(b"\x1b]0;recovered\x07"));
    }

    // ----- format_title tests -------------------------------------------

    #[test]
    fn format_title_with_cwd_and_name() {
        let info = ProcessInfo {
            name: "vim".into(),
            cwd: Some(PathBuf::from("/Users/me/myproject")),
        };
        assert_eq!(format_title(&info), "myproject — vim");
    }

    #[test]
    fn format_title_without_cwd_falls_back_to_name() {
        let info = ProcessInfo {
            name: "top".into(),
            cwd: None,
        };
        assert_eq!(format_title(&info), "top");
    }

    #[test]
    fn format_title_with_root_cwd_falls_back_to_name() {
        // file_name() returns None for "/", so cwd_name is empty.
        let info = ProcessInfo {
            name: "htop".into(),
            cwd: Some(PathBuf::from("/")),
        };
        assert_eq!(format_title(&info), "htop");
    }

    // ----- synthesise_osc_title tests -----------------------------------

    #[test]
    fn synthesise_emits_well_formed_osc_0() {
        let bytes = synthesise_osc_title("hello");
        assert_eq!(bytes, b"\x1b]0;hello\x07");
    }

    // ----- ForegroundProbe tests ----------------------------------------

    /// Test clock — shared mutable Instant.
    #[derive(Clone)]
    struct FakeClock(Rc<Cell<Instant>>);

    impl FakeClock {
        fn new() -> Self {
            Self(Rc::new(Cell::new(Instant::now())))
        }

        fn advance(&self, by: Duration) {
            self.0.set(self.0.get() + by);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    /// Test source that returns whatever info the test wants.
    #[derive(Clone)]
    struct FakeSource(Rc<Cell<Option<ProcessInfo>>>);

    impl FakeSource {
        fn new(info: Option<ProcessInfo>) -> Self {
            Self(Rc::new(Cell::new(info)))
        }

        fn set(&self, info: Option<ProcessInfo>) {
            self.0.set(info);
        }
    }

    impl ForegroundInfoSource for FakeSource {
        fn lookup(&self) -> Option<ProcessInfo> {
            // Cell::take + put back: clone via Option's Clone.
            let value = self.0.replace(None);
            self.0.set(value.clone());
            value
        }
    }

    fn probe(clock: FakeClock) -> ForegroundProbe<FakeClock> {
        ForegroundProbe::with_clock(clock).with_timing(
            Duration::from_millis(500),
            Duration::from_secs(3),
        )
    }

    fn vim_in_myproject() -> ProcessInfo {
        ProcessInfo {
            name: "vim".into(),
            cwd: Some(PathBuf::from("/Users/me/myproject")),
        }
    }

    fn top_no_cwd() -> ProcessInfo {
        ProcessInfo {
            name: "top".into(),
            cwd: None,
        }
    }

    #[test]
    fn injects_when_idle_and_source_has_info() {
        let clock = FakeClock::new();
        let source = FakeSource::new(Some(vim_in_myproject()));
        let mut p = probe(clock);

        let out = p.maybe_inject(&source).expect("should inject");
        assert_eq!(out, b"\x1b]0;myproject \xe2\x80\x94 vim\x07");
    }

    #[test]
    fn does_not_inject_when_source_returns_none() {
        let clock = FakeClock::new();
        let source = FakeSource::new(None);
        let mut p = probe(clock);
        assert!(p.maybe_inject(&source).is_none());
    }

    #[test]
    fn snoozes_after_real_osc_title_observed() {
        let clock = FakeClock::new();
        let source = FakeSource::new(Some(vim_in_myproject()));
        let mut p = probe(clock.clone());

        // Real program emits OSC 0.
        p.observe_outbound(b"\x1b]0;real-title\x07");

        // Within quiet period, no synthetic injection.
        assert!(p.maybe_inject(&source).is_none());

        // Even after rate-limit window passes, still in quiet period.
        clock.advance(Duration::from_millis(600));
        assert!(p.maybe_inject(&source).is_none());

        // After quiet period passes, injection resumes.
        clock.advance(Duration::from_secs(3));
        assert!(p.maybe_inject(&source).is_some());
    }

    #[test]
    fn rate_limits_consecutive_injections() {
        let clock = FakeClock::new();
        let source = FakeSource::new(Some(vim_in_myproject()));
        let mut p = probe(clock.clone());

        // First injection succeeds.
        assert!(p.maybe_inject(&source).is_some());

        // Source updates immediately to a different process.
        source.set(Some(top_no_cwd()));

        // Within sample interval, no second injection (even though title
        // would change).
        clock.advance(Duration::from_millis(100));
        assert!(p.maybe_inject(&source).is_none());

        // After sample interval elapses, second injection succeeds.
        clock.advance(Duration::from_millis(500));
        let out = p.maybe_inject(&source).expect("should inject after rate limit");
        assert_eq!(out, b"\x1b]0;top\x07");
    }

    #[test]
    fn dedups_when_title_unchanged() {
        let clock = FakeClock::new();
        let source = FakeSource::new(Some(vim_in_myproject()));
        let mut p = probe(clock.clone());

        // First injection.
        assert!(p.maybe_inject(&source).is_some());

        // Time passes, title hasn't changed.
        clock.advance(Duration::from_secs(1));
        assert!(p.maybe_inject(&source).is_none(), "no re-inject for same title");

        // Title actually changes.
        source.set(Some(top_no_cwd()));
        clock.advance(Duration::from_secs(1));
        assert!(p.maybe_inject(&source).is_some(), "inject on actual change");
    }

    #[test]
    fn injection_resumes_immediately_when_no_real_title_yet() {
        // Scenario: terminal thread starts with a non-OSC program (e.g.
        // top) running. No real OSC was ever observed, so quiet period
        // never engaged; injection should fire on first probe.
        let clock = FakeClock::new();
        let source = FakeSource::new(Some(top_no_cwd()));
        let mut p = probe(clock);
        assert!(p.maybe_inject(&source).is_some());
    }

    #[test]
    fn observe_outbound_with_partial_osc_does_not_arm_quiet() {
        // A truncated OSC (no terminator yet) should NOT arm the quiet
        // period — only completed sequences count.
        let clock = FakeClock::new();
        let source = FakeSource::new(Some(vim_in_myproject()));
        let mut p = probe(clock);
        p.observe_outbound(b"\x1b]0;not-finished-yet");
        // Synthetic injection still works.
        assert!(p.maybe_inject(&source).is_some());
    }

    #[test]
    fn observe_outbound_completes_osc_split_across_feeds() {
        let clock = FakeClock::new();
        let source = FakeSource::new(Some(vim_in_myproject()));
        let mut p = probe(clock);
        p.observe_outbound(b"\x1b]0;split");
        p.observe_outbound(b"-title\x07");
        // OSC was completed across feeds → quiet period engaged.
        assert!(p.maybe_inject(&source).is_none());
    }
}
