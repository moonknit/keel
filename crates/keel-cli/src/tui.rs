//! TTY conversation host (REQ-3.22, REQ-3.30). ratatui stays in keel-cli.

use anyhow::Result;
use async_trait::async_trait;
use crossterm::cursor::Show;
use crossterm::event::{
    DisableMouseCapture, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use keel_core::agent::{Agent, Inbox};
use keel_core::traits::{Approver, Store};
use keel_core::types::{ApprovalRequest, Decision, Event, InboxMessage, RunStatus};
use keel_skills::discover;
use keel_store::SqliteStore;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Margin};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::io::{self, IsTerminal, Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cron::{self, DueAction};
use crate::dashboard;
use crate::last;
use crate::slash;

pub fn interactive_allowed(is_tty: bool) -> bool {
    is_tty
}

pub fn compact_notice() -> &'static str {
    "예정"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Exit,
    Help,
    Compact,
    Slash(String),
    Steer(String),
    Prompt(String),
}

/// Slash stays a command. Mid-run non-slash text is Steer (REQ-3.31).
pub fn classify_input(running: bool, line: &str) -> InputAction {
    let line = line.trim();
    if line == "/exit" {
        return InputAction::Exit;
    }
    if line == "/help" {
        return InputAction::Help;
    }
    if line == "/compact" {
        return InputAction::Compact;
    }
    if line.starts_with('/') {
        return InputAction::Slash(line.to_string());
    }
    if running {
        InputAction::Steer(line.to_string())
    } else {
        InputAction::Prompt(line.to_string())
    }
}

pub struct ChannelApprover {
    tx: mpsc::UnboundedSender<(ApprovalRequest, oneshot::Sender<Decision>)>,
}

impl ChannelApprover {
    pub fn pair() -> (
        Self,
        mpsc::UnboundedReceiver<(ApprovalRequest, oneshot::Sender<Decision>)>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn approve(&self, req: &ApprovalRequest) -> Decision {
        let (tx, rx) = oneshot::channel();
        if self.tx.send((req.clone(), tx)).is_err() {
            return Decision::Deny;
        }
        rx.await.unwrap_or(Decision::Deny)
    }
}

struct Ui {
    lines: Vec<String>,
    input: String,
    status: String,
    last_tool: String,
    running: bool,
    pending: Option<(ApprovalRequest, oneshot::Sender<Decision>)>,
    dashboard: bool,
    prompt_ctrl_c: bool,
    refresh_last: bool,
    scheduler: bool,
    dash_body: String,
    scroll: u16,
    follow: bool,
    view_h: u16,
    view_w: u16,
}

impl Ui {
    fn push(&mut self, s: impl AsRef<str>) {
        self.lines.push(s.as_ref().to_string());
        if self.lines.len() > 400 {
            self.lines.drain(0..self.lines.len() - 400);
        }
        if self.follow {
            self.scroll = u16::MAX;
        }
    }

    fn body(&self) -> String {
        if self.dashboard {
            self.dash_body.clone()
        } else {
            self.lines.join("\n")
        }
    }
}

/// Terminal columns. Width-0 marks (IME fillers) must not move the cursor.
pub fn display_cols(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn wrapped_rows(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let w = width as usize;
    let mut rows = 0u16;
    for line in text.split('\n') {
        let cols = display_cols(line).max(1);
        let need = ((cols + w - 1) / w) as u16;
        rows = rows.saturating_add(need);
    }
    rows
}

pub fn max_scroll(rows: u16, view_h: u16) -> u16 {
    rows.saturating_sub(view_h)
}

pub fn apply_scroll(scroll: u16, delta: i32, rows: u16, view_h: u16) -> (u16, bool) {
    let max = max_scroll(rows, view_h) as i32;
    let next = (i32::from(scroll) + delta).clamp(0, max) as u16;
    (next, next >= max as u16)
}

pub async fn run(
    db: Arc<SqliteStore>,
    session_id: &str,
    cwd: &Path,
    agent: Agent,
    policy_approver: Arc<keel_tools::PolicyApprover>,
    mut approvals: mpsc::UnboundedReceiver<(ApprovalRequest, oneshot::Sender<Decision>)>,
    model: &str,
) -> Result<()> {
    if !interactive_allowed(io::stdin().is_terminal()) {
        anyhow::bail!("TTY required for interactive keel");
    }
    let _term = TermRestore::enter()?;
    // A caught panic must not leave the alternate screen. TermRestore::drop
    // still restores the tty when the session actually ends.
    let _prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        if let Ok(mut slot) = LAST_PANIC.lock() {
            *slot = msg.clone();
        }
        write_panic_file(&msg);
    }));
    run_inner(
        db,
        session_id,
        cwd,
        agent,
        policy_approver,
        &mut approvals,
        model,
        io::stdout(),
    )
    .await
}

/// Always restore the tty, including after panic or early return.
/// Mouse motion (`1003`) is not enabled — it floods the shell if left on.
struct TermRestore;

impl TermRestore {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen)?;
        // button/wheel only (SGR). not any-event motion.
        out.write_all(b"\x1b[?1000h\x1b[?1006h")?;
        out.flush()?;
        Ok(Self)
    }
}

fn restore_tty() {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l");
    let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

impl Drop for TermRestore {
    fn drop(&mut self) {
        restore_tty();
    }
}

static LAST_PANIC: Mutex<String> = Mutex::new(String::new());
static RECENT_INPUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

fn last_panic_line() -> String {
    LAST_PANIC
        .lock()
        .ok()
        .and_then(|g| g.lines().next().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "panic".into())
}

fn write_panic_file(msg: &str) {
    let home = std::env::var("KEEL_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{home}/.keel")
    });
    let path = Path::new(&home).join("logs").join("panic.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let recent = RECENT_INPUT
        .lock()
        .map(|g| {
            g.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{msg}\ninput: {recent}\n");
    }
}

fn render(term: &mut Terminal<CrosstermBackend<io::Stdout>>, ui: &mut Ui) -> Result<()> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| term.draw(|f| draw(f, ui)).map(|_| ()))) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            ui.status = last_panic_line();
            ui.input.clear();
            match std::panic::catch_unwind(AssertUnwindSafe(|| {
                term.draw(|f| draw(f, ui)).map(|_| ())
            })) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e.into()),
                Err(_) => {
                    ui.lines.clear();
                    ui.dash_body.clear();
                    ui.dashboard = false;
                    ui.status = last_panic_line();
                    term.draw(|f| draw(f, ui))
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                }
            }
        }
    }
}

fn spawn_input(tx: mpsc::UnboundedSender<CEvent>) {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        loop {
            let continued =
                std::panic::catch_unwind(AssertUnwindSafe(|| input_once(&mut buf, &tx)));
            match continued {
                Ok(true) => {}
                Ok(false) => break,
                Err(_) => {
                    buf.clear();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    });
}

fn input_once(buf: &mut Vec<u8>, tx: &mpsc::UnboundedSender<CEvent>) -> bool {
    let mut scratch = [0u8; 256];
    let idle = buf.is_empty();
    match read_some(&mut scratch, if idle { -1 } else { 20 }) {
        ReadSome::Bytes(n) => {
            remember(&scratch[..n]);
            buf.extend_from_slice(&scratch[..n]);
            drain_events(buf, false, tx)
        }
        ReadSome::Timeout => {
            if buf.is_empty() {
                true
            } else {
                drain_events(buf, true, tx)
            }
        }
        ReadSome::Eof => {
            let _ = tx.send(key_ev(KeyCode::Char('d'), KeyModifiers::CONTROL));
            false
        }
        ReadSome::Err => {
            std::thread::sleep(std::time::Duration::from_millis(20));
            true
        }
    }
}

fn remember(bytes: &[u8]) {
    if let Ok(mut recent) = RECENT_INPUT.lock() {
        recent.extend_from_slice(bytes);
        if recent.len() > 128 {
            let drop_n = recent.len() - 128;
            recent.drain(..drop_n);
        }
    }
}

fn drain_events(
    buf: &mut Vec<u8>,
    mut timed_out: bool,
    tx: &mpsc::UnboundedSender<CEvent>,
) -> bool {
    loop {
        match pop_step(buf, timed_out) {
            Step::Event(ev) => {
                timed_out = false;
                if tx.send(ev).is_err() {
                    return false;
                }
            }
            Step::NeedMore => return true,
        }
    }
}

enum ReadSome {
    Bytes(usize),
    Timeout,
    Eof,
    Err,
}

fn read_some(scratch: &mut [u8], timeout_ms: i32) -> ReadSome {
    let mut fds = [libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    }];
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };
    if rc == 0 {
        return ReadSome::Timeout;
    }
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            return ReadSome::Timeout;
        }
        return ReadSome::Err;
    }
    match io::stdin().read(scratch) {
        Ok(0) => ReadSome::Eof,
        Ok(n) => ReadSome::Bytes(n),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => ReadSome::Timeout,
        Err(_) => ReadSome::Err,
    }
}

#[derive(Debug)]
enum Step {
    Event(CEvent),
    NeedMore,
}

fn key_ev(code: KeyCode, modifiers: KeyModifiers) -> CEvent {
    CEvent::Key(KeyEvent::new(code, modifiers))
}

/// Bytes from the tty, without crossterm's CSI parser.
/// That parser debug-panics on a cursor report or SGR mouse coordinate of 0,
/// which a visible cursor plus the macOS Korean IME produces on a normal key.
fn pop_step(buf: &mut Vec<u8>, timed_out: bool) -> Step {
    loop {
        if buf.is_empty() {
            return Step::NeedMore;
        }
        match buf[0] {
            0x03 => {
                buf.remove(0);
                return Step::Event(key_ev(KeyCode::Char('c'), KeyModifiers::CONTROL));
            }
            0x04 => {
                buf.remove(0);
                return Step::Event(key_ev(KeyCode::Char('d'), KeyModifiers::CONTROL));
            }
            0x08 | 0x7f => {
                buf.remove(0);
                return Step::Event(key_ev(KeyCode::Backspace, KeyModifiers::NONE));
            }
            0x0d => {
                buf.remove(0);
                return Step::Event(key_ev(KeyCode::Enter, KeyModifiers::NONE));
            }
            0x1b => match pop_esc(buf, timed_out) {
                EscStep::Event(ev) => return Step::Event(ev),
                EscStep::NeedMore => return Step::NeedMore,
                EscStep::Again => continue,
            },
            b if b < 0x20 => {
                buf.remove(0);
            }
            _ => match pop_text(buf, timed_out) {
                TextStep::Event(ev) => return Step::Event(ev),
                TextStep::NeedMore => return Step::NeedMore,
                TextStep::Again => continue,
            },
        }
    }
}

enum EscStep {
    Event(CEvent),
    NeedMore,
    Again,
}

fn pop_esc(buf: &mut Vec<u8>, timed_out: bool) -> EscStep {
    if buf.len() == 1 {
        if timed_out {
            buf.remove(0);
            return EscStep::Event(key_ev(KeyCode::Esc, KeyModifiers::NONE));
        }
        return EscStep::NeedMore;
    }
    match buf[1] {
        b'[' => pop_csi(buf, timed_out),
        b'O' => pop_ss3(buf, timed_out),
        _ => {
            // Same packet as the committed character: the Esc is not a key.
            buf.remove(0);
            EscStep::Again
        }
    }
}

fn pop_ss3(buf: &mut Vec<u8>, timed_out: bool) -> EscStep {
    if buf.len() < 3 {
        if timed_out {
            buf.drain(..buf.len().min(2));
            return EscStep::Again;
        }
        return EscStep::NeedMore;
    }
    let ev = match buf[2] {
        b'A' => Some(KeyCode::Up),
        b'B' => Some(KeyCode::Down),
        b'C' => Some(KeyCode::Right),
        b'D' => Some(KeyCode::Left),
        b'H' => Some(KeyCode::Home),
        b'F' => Some(KeyCode::End),
        _ => None,
    };
    if let Some(code) = ev {
        buf.drain(..3);
        EscStep::Event(key_ev(code, KeyModifiers::NONE))
    } else {
        buf.remove(0);
        EscStep::Again
    }
}

fn pop_csi(buf: &mut Vec<u8>, timed_out: bool) -> EscStep {
    let final_at = buf.iter().enumerate().skip(2).find_map(|(i, b)| {
        if (0x40..=0x7e).contains(b) {
            Some(i)
        } else {
            None
        }
    });
    let Some(end) = final_at else {
        if timed_out || buf.len() > 64 {
            buf.remove(0);
            return EscStep::Again;
        }
        return EscStep::NeedMore;
    };
    let seq: Vec<u8> = buf.drain(..=end).collect();
    if let Some(ev) = csi_event(&seq) {
        EscStep::Event(ev)
    } else {
        EscStep::Again
    }
}

fn csi_event(seq: &[u8]) -> Option<CEvent> {
    if seq.len() < 3 || seq[0] != 0x1b || seq[1] != b'[' {
        return None;
    }
    let final_b = *seq.last()?;
    if seq.len() >= 3 && seq[2] == b'<' && (final_b == b'M' || final_b == b'm') {
        return sgr_mouse(seq);
    }
    if final_b == b'u' {
        return kitty_key(seq);
    }
    // Cursor position report: ESC [ y ; x R. Ignore it.
    if final_b == b'R' {
        return None;
    }
    let params = &seq[2..seq.len() - 1];
    match final_b {
        b'A' => Some(key_ev(KeyCode::Up, KeyModifiers::NONE)),
        b'B' => Some(key_ev(KeyCode::Down, KeyModifiers::NONE)),
        b'C' => Some(key_ev(KeyCode::Right, KeyModifiers::NONE)),
        b'D' => Some(key_ev(KeyCode::Left, KeyModifiers::NONE)),
        b'H' => Some(key_ev(KeyCode::Home, KeyModifiers::NONE)),
        b'F' => Some(key_ev(KeyCode::End, KeyModifiers::NONE)),
        b'~' => tilde_key(params),
        _ => None,
    }
}

fn tilde_key(params: &[u8]) -> Option<CEvent> {
    let first = params.split(|b| *b == b';').next().unwrap_or(params);
    let code = match parse_u16(first)? {
        1 | 7 => KeyCode::Home,
        3 => KeyCode::Backspace,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        _ => return None,
    };
    Some(key_ev(code, KeyModifiers::NONE))
}

fn sgr_mouse(seq: &[u8]) -> Option<CEvent> {
    let body = &seq[3..seq.len() - 1];
    let mut parts = body.split(|b| *b == b';');
    let cb = parse_u16(parts.next()?)?;
    let cx = parse_u16(parts.next()?).unwrap_or(1).saturating_sub(1);
    let cy = parse_u16(parts.next()?).unwrap_or(1).saturating_sub(1);
    let kind = mouse_kind(cb)?;
    Some(CEvent::Mouse(MouseEvent {
        kind,
        column: cx,
        row: cy,
        modifiers: KeyModifiers::empty(),
    }))
}

fn mouse_kind(cb: u16) -> Option<MouseEventKind> {
    let cb = u8::try_from(cb).ok()?;
    let button = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 == 0b0010_0000;
    Some(match (button, dragging) {
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        _ => return None,
    })
}

fn kitty_key(seq: &[u8]) -> Option<CEvent> {
    let params = &seq[2..seq.len() - 1];
    let mut parts = params.split(|b| *b == b';');
    let code_field = parts.next()?;
    let cp_bytes = code_field
        .split(|b| *b == b':')
        .next()
        .unwrap_or(code_field);
    let cp = parse_u32(cp_bytes)?;
    if (0xE000..=0xF8FF).contains(&cp) {
        return None;
    }
    let mods = parts.next().and_then(parse_u16).unwrap_or(1);
    let bits = mods.saturating_sub(1);
    let mut modifiers = KeyModifiers::empty();
    if bits & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    let ch = char::from_u32(cp)?;
    let code = match ch {
        '\u{1b}' => KeyCode::Esc,
        '\r' | '\n' => KeyCode::Enter,
        '\u{7f}' | '\u{8}' => KeyCode::Backspace,
        c if c.is_control() => return None,
        c => KeyCode::Char(c),
    };
    Some(key_ev(code, modifiers))
}

enum TextStep {
    Event(CEvent),
    NeedMore,
    Again,
}

fn pop_text(buf: &mut Vec<u8>, timed_out: bool) -> TextStep {
    let need = match buf[0] {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => {
            buf.remove(0);
            return TextStep::Again;
        }
    };
    if buf.len() < need {
        if timed_out {
            buf.remove(0);
            return TextStep::Again;
        }
        return TextStep::NeedMore;
    }
    match std::str::from_utf8(&buf[..need]) {
        Ok(s) => {
            let Some(ch) = s.chars().next() else {
                buf.drain(..need);
                return TextStep::Again;
            };
            if ch.len_utf8() != need {
                buf.remove(0);
                return TextStep::Again;
            }
            buf.drain(..need);
            if ch.is_control() {
                TextStep::Again
            } else {
                TextStep::Event(key_ev(KeyCode::Char(ch), KeyModifiers::NONE))
            }
        }
        Err(_) => {
            buf.remove(0);
            TextStep::Again
        }
    }
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }
    let mut n: u16 = 0;
    for b in bytes {
        let digit = b.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u16::from(digit))?;
    }
    Some(n)
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for b in bytes {
        let digit = b.checked_sub(b'0')?;
        if digit > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    Some(n)
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    db: Arc<SqliteStore>,
    session_id: &str,
    cwd: &Path,
    agent: Agent,
    policy_approver: Arc<keel_tools::PolicyApprover>,
    approvals: &mut mpsc::UnboundedReceiver<(ApprovalRequest, oneshot::Sender<Decision>)>,
    model: &str,
    stdout: io::Stdout,
) -> Result<()> {
    let store: Arc<dyn Store> = db.clone();
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<CEvent>();
    spawn_input(key_tx);
    let mut keys_dead = false;
    let mut ui = Ui {
        lines: vec![format!("session {session_id}"), slash::HELP.into()],
        input: String::new(),
        status: String::new(),
        last_tool: String::new(),
        running: false,
        pending: None,
        dashboard: false,
        prompt_ctrl_c: false,
        refresh_last: false,
        scheduler: false,
        dash_body: String::new(),
        scroll: 0,
        follow: true,
        view_h: 1,
        view_w: 1,
    };
    let mut session_id = session_id.to_string();
    let cwd_s = cwd.to_string_lossy().into_owned();
    let inbox = agent.inbox.clone();
    let agent = Arc::new(agent);
    let mut cancel = CancellationToken::new();
    let mut run_handle: Option<JoinHandle<()>> = None;
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    let (due_tx, mut due_rx) = mpsc::unbounded_channel::<DueAction>();
    let running_flag = Arc::new(AtomicBool::new(false));
    let sid_flag = Arc::new(Mutex::new(session_id.clone()));
    let me = i64::from(std::process::id());
    if db.acquire_scheduler(me).unwrap_or(false) {
        ui.scheduler = true;
        let tick_db = db.clone();
        let tick_inbox = inbox.clone();
        let tick_running = running_flag.clone();
        let tick_sid = sid_flag.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let sid = tick_sid.lock().ok().map(|g| g.clone());
                let actions = cron::tick(
                    tick_db.as_ref(),
                    now,
                    me,
                    sid.as_deref(),
                    tick_running.load(Ordering::Acquire),
                    Some(&tick_inbox),
                )
                .await;
                for action in actions {
                    if due_tx.send(action).is_err() {
                        return;
                    }
                }
            }
        });
    }

    let result = loop {
        if ui.refresh_last {
            ui.refresh_last = false;
            if let Ok(ws) = store.open_session(&session_id).await {
                if let Some(output) = last::last_tool_output(&ws) {
                    ui.last_tool = output.to_string();
                }
            }
        }
        render(&mut term, &mut ui)?;
        tokio::select! {
            Some((req, reply)) = approvals.recv() => {
                ui.status = format!("allow {} {}? [y]es [a]lways [n]o", req.name, req.args_summary);
                ui.pending = Some((req, reply));
            }
            Some(ev) = ev_rx.recv() => {
                apply_event(&mut ui, ev, &running_flag);
            }
            Some(action) = due_rx.recv() => {
                if let DueAction::Run { prompt, session_id: sid, .. } = action {
                    if !ui.running && sid == session_id {
                        start_run(
                            prompt,
                            &mut ui,
                            agent.clone(),
                            &session_id,
                            &cwd_s,
                            &mut cancel,
                            ev_tx.clone(),
                            &mut run_handle,
                            &running_flag,
                        );
                    }
                }
            }
            ev = key_rx.recv(), if !keys_dead => {
                match ev {
                    Some(CEvent::Mouse(mouse)) => {
                        let delta = match mouse.kind {
                            MouseEventKind::ScrollUp => -3,
                            MouseEventKind::ScrollDown => 3,
                            _ => 0,
                        };
                        if delta != 0 {
                            scroll_by(&mut ui, delta);
                        }
                    }
                    Some(CEvent::Key(key)) => {
                        if let Some(done) = handle_key(
                            key,
                            &mut ui,
                            agent.clone(),
                            store.clone(),
                            &mut session_id,
                            cwd,
                            &cwd_s,
                            model,
                            &policy_approver,
                            &inbox,
                            &mut cancel,
                            ev_tx.clone(),
                            &mut run_handle,
                            &db,
                            &running_flag,
                            &sid_flag,
                        ).await? {
                            break done;
                        }
                    }
                    Some(_) => {}
                    None => keys_dead = true,
                }
            }
        }
    };

    cancel.cancel();
    if let Some(h) = run_handle.take() {
        let _ = h.await;
    }
    if ui.scheduler {
        let _ = db.release_scheduler(me);
    }
    store.close(&session_id).await.map_err(into)?;
    result
}

fn apply_event(ui: &mut Ui, ev: Event, running_flag: &AtomicBool) {
    match ev {
        Event::TextDelta(s) => {
            if let Some(last) = ui.lines.last_mut() {
                if last.starts_with("assistant: ") {
                    last.push_str(&s);
                    if ui.follow {
                        ui.scroll = u16::MAX;
                    }
                    return;
                }
            }
            ui.push(format!("assistant: {s}"));
        }
        Event::ToolStart { name, args } => ui.push(format!("tool {name} {args}")),
        Event::ToolEnd { name, ok, .. } => {
            ui.push(format!("tool {name} {}", if ok { "ok" } else { "err" }));
        }
        Event::RunEnd { status } => {
            ui.running = false;
            running_flag.store(false, Ordering::Release);
            ui.status.clear();
            ui.refresh_last = true;
            if status == RunStatus::Interrupted {
                ui.push("interrupted");
            }
        }
        Event::RunStart { .. } => {
            ui.running = true;
            running_flag.store(true, Ordering::Release);
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_key(
    key: KeyEvent,
    ui: &mut Ui,
    agent: Arc<Agent>,
    store: Arc<dyn Store>,
    session_id: &mut String,
    cwd: &Path,
    cwd_s: &str,
    model: &str,
    policy_approver: &Arc<keel_tools::PolicyApprover>,
    inbox: &Inbox,
    cancel: &mut CancellationToken,
    ev_tx: mpsc::UnboundedSender<Event>,
    run_handle: &mut Option<JoinHandle<()>>,
    db: &SqliteStore,
    running_flag: &Arc<AtomicBool>,
    sid_flag: &Arc<Mutex<String>>,
) -> Result<Option<Result<()>>> {
    if key.kind != KeyEventKind::Press {
        return Ok(None);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if ui.running {
            cancel.cancel();
            ui.prompt_ctrl_c = false;
            return Ok(None);
        }
        if ui.prompt_ctrl_c {
            return Ok(Some(Ok(())));
        }
        ui.prompt_ctrl_c = true;
        ui.input.clear();
        ui.status = "Ctrl-C again to exit".into();
        return Ok(None);
    }
    ui.prompt_ctrl_c = false;
    if let Some((_, reply)) = ui.pending.take() {
        let d = match key.code {
            KeyCode::Char('a') => Decision::Always,
            KeyCode::Char('n') => Decision::Deny,
            _ => Decision::Once,
        };
        let _ = reply.send(d);
        ui.status.clear();
        return Ok(None);
    }
    match key.code {
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(Some(Ok(())));
        }
        // macOS 한글 IME는 조합 중 Backspace를 Esc로 보내기도 한다. 입력 중이면 지운다.
        KeyCode::Esc if !ui.input.is_empty() => pop_input(&mut ui.input),
        KeyCode::Esc => {
            ui.dashboard = !ui.dashboard;
            ui.follow = true;
            ui.scroll = u16::MAX;
            if ui.dashboard {
                refresh_dash(ui, db).await;
            }
        }
        KeyCode::Char('h')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            pop_input(&mut ui.input);
        }
        KeyCode::PageUp => scroll_by(ui, -i32::from(ui.view_h.max(1))),
        KeyCode::PageDown => scroll_by(ui, i32::from(ui.view_h.max(1))),
        KeyCode::Up => scroll_by(ui, -1),
        KeyCode::Down => scroll_by(ui, 1),
        KeyCode::Home => {
            ui.scroll = 0;
            ui.follow = false;
        }
        KeyCode::End => {
            ui.follow = true;
            ui.scroll = u16::MAX;
        }
        // Alt+char is text. macOS IME prefixes a committed character with Esc,
        // which crossterm used to report as Alt and this match used to drop.
        KeyCode::Char(_)
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) => {}
        KeyCode::Char(c) => ingest_char(&mut ui.input, c),
        KeyCode::Backspace => pop_input(&mut ui.input),
        KeyCode::Enter => {
            let line = std::mem::take(&mut ui.input);
            if ui.dashboard {
                apply_dashboard_line(ui, db, cwd_s, &line).await;
                return Ok(None);
            }
            if let Some(end) = handle_line(
                &line,
                ui,
                agent,
                store,
                session_id,
                cwd,
                cwd_s,
                model,
                policy_approver,
                inbox,
                cancel,
                ev_tx,
                run_handle,
                running_flag,
                sid_flag,
            )
            .await?
            {
                return Ok(Some(end));
            }
        }
        _ => {}
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn handle_line(
    line: &str,
    ui: &mut Ui,
    agent: Arc<Agent>,
    store: Arc<dyn Store>,
    session_id: &mut String,
    cwd: &Path,
    cwd_s: &str,
    model: &str,
    policy_approver: &Arc<keel_tools::PolicyApprover>,
    inbox: &Inbox,
    cancel: &mut CancellationToken,
    ev_tx: mpsc::UnboundedSender<Event>,
    run_handle: &mut Option<JoinHandle<()>>,
    running_flag: &Arc<AtomicBool>,
    sid_flag: &Arc<Mutex<String>>,
) -> Result<Option<Result<()>>> {
    match classify_input(ui.running, line) {
        InputAction::Exit => return Ok(Some(Ok(()))),
        InputAction::Help => {
            ui.push(slash::HELP);
            return Ok(None);
        }
        InputAction::Compact => {
            ui.push(compact_notice());
            return Ok(None);
        }
        InputAction::Steer(text) => {
            inbox.push(InboxMessage::Steer { text: text.clone() }).await;
            ui.push(format!("steer: {text}"));
            return Ok(None);
        }
        InputAction::Slash(cmd) => {
            return apply_slash(
                &cmd,
                ui,
                store,
                session_id,
                cwd,
                cwd_s,
                model,
                policy_approver,
                cancel,
                ev_tx,
                agent,
                run_handle,
                running_flag,
                sid_flag,
            )
            .await;
        }
        InputAction::Prompt(prompt) => {
            start_run(
                prompt,
                ui,
                agent,
                session_id,
                cwd_s,
                cancel,
                ev_tx,
                run_handle,
                running_flag,
            );
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn apply_slash(
    line: &str,
    ui: &mut Ui,
    store: Arc<dyn Store>,
    session_id: &mut String,
    cwd: &Path,
    cwd_s: &str,
    model: &str,
    policy_approver: &Arc<keel_tools::PolicyApprover>,
    cancel: &mut CancellationToken,
    ev_tx: mpsc::UnboundedSender<Event>,
    agent: Arc<Agent>,
    run_handle: &mut Option<JoinHandle<()>>,
    running_flag: &Arc<AtomicBool>,
    sid_flag: &Arc<Mutex<String>>,
) -> Result<Option<Result<()>>> {
    if line == "/plan" || line.starts_with("/plan ") {
        match slash::parse_plan(line) {
            Some(cmd) => ui.push(slash::apply_plan(&policy_approver.policy, cmd)),
            None => ui.push("/plan [approve|deny]"),
        }
        return Ok(None);
    }
    if let Some(switch) = slash::parse_session_switch(line) {
        match slash::apply_session_switch(store.as_ref(), session_id, cwd_s, model, switch).await {
            Ok(new_id) => {
                *session_id = new_id;
                if let Ok(mut g) = sid_flag.lock() {
                    *g = session_id.clone();
                }
                ui.push(session_id.clone());
            }
            Err(e) => ui.push(e.to_string()),
        }
        return Ok(None);
    }
    if line == "/last" {
        ui.push(ui.last_tool.clone());
        return Ok(None);
    }
    if line == "/skills" {
        for s in discover(cwd) {
            ui.push(format!("{}  {}", s.name, s.description));
        }
        return Ok(None);
    }
    if let Some(name) = line.strip_prefix("/name ") {
        store.rename(session_id, name).await.map_err(into)?;
        return Ok(None);
    }
    if line == "/session" {
        match store.open_session(session_id).await {
            Ok(ws) => ui.push(format!(
                "{}  {}  {}",
                ws.meta.id, ws.meta.cwd, ws.meta.model
            )),
            Err(e) => ui.push(e.to_string()),
        }
        return Ok(None);
    }
    if let Some(rest) = line.strip_prefix("/skill:") {
        let name = rest.split_whitespace().next().unwrap_or("");
        let prompt = if let Some(meta) = discover(cwd).into_iter().find(|s| s.name == name) {
            keel_skills::skill_body(&meta)
        } else {
            format!("unknown skill {name}")
        };
        if ui.running {
            agent
                .inbox
                .push(InboxMessage::Steer {
                    text: prompt.clone(),
                })
                .await;
            ui.push(format!("steer: {prompt}"));
        } else {
            start_run(
                prompt,
                ui,
                agent,
                session_id,
                cwd_s,
                cancel,
                ev_tx,
                run_handle,
                running_flag,
            );
        }
        return Ok(None);
    }
    ui.push(format!("unknown {line}"));
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn start_run(
    prompt: String,
    ui: &mut Ui,
    agent: Arc<Agent>,
    session_id: &str,
    cwd_s: &str,
    cancel: &mut CancellationToken,
    ev_tx: mpsc::UnboundedSender<Event>,
    run_handle: &mut Option<JoinHandle<()>>,
    running_flag: &AtomicBool,
) {
    ui.push(format!("you: {prompt}"));
    *cancel = CancellationToken::new();
    let cancel_run = cancel.clone();
    let sid = session_id.to_string();
    let cwd_run = cwd_s.to_string();
    ui.running = true;
    running_flag.store(true, Ordering::Release);
    *run_handle = Some(tokio::spawn(async move {
        let _ = agent.run(&sid, &prompt, &cwd_run, cancel_run, ev_tx).await;
    }));
}

async fn refresh_dash(ui: &mut Ui, db: &SqliteStore) {
    let sessions = db.list_sessions(None, false).await.unwrap_or_default();
    let jobs = db.list_cron(None).unwrap_or_default();
    ui.dash_body = dashboard::dashboard_text(&sessions, &jobs, ui.running, ui.scheduler);
}

async fn apply_dashboard_line(ui: &mut Ui, db: &SqliteStore, cwd: &str, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        refresh_dash(ui, db).await;
        return;
    }
    match dashboard::parse_dash(line) {
        Some(cmd) => match dashboard::apply_dash(db, cwd, cmd) {
            Ok(msg) => ui.status = msg,
            Err(e) => ui.status = e.to_string(),
        },
        None => ui.status = "dashboard: add|rm|on|off only".into(),
    }
    refresh_dash(ui, db).await;
}

fn into(e: keel_core::KeelError) -> anyhow::Error {
    anyhow::anyhow!(e.to_string())
}

fn scroll_by(ui: &mut Ui, delta: i32) {
    let rows = wrapped_rows(&ui.body(), ui.view_w);
    let (next, at_end) = apply_scroll(ui.scroll, delta, rows, ui.view_h);
    ui.scroll = next;
    ui.follow = at_end;
}

fn draw(f: &mut ratatui::Frame, ui: &mut Ui) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(f.area());
    let inner = chunks[0].inner(Margin::new(1, 1));
    ui.view_w = inner.width;
    ui.view_h = inner.height;
    let body = ui.body();
    let rows = wrapped_rows(&body, ui.view_w);
    let max = max_scroll(rows, ui.view_h);
    if ui.follow || ui.scroll > max {
        ui.scroll = max;
        ui.follow = true;
    }
    let title = if ui.dashboard { "dashboard" } else { "keel" };
    f.render_widget(
        Paragraph::new(body)
            .wrap(padding_wrap())
            .scroll((ui.scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(title)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(ui.status.as_str())
            .style(Style::default().add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL).title("status")),
        chunks[1],
    );
    let input_inner = chunks[2].inner(Margin::new(1, 1));
    f.render_widget(
        Paragraph::new(padded_line(&ui.input, input_inner.width))
            .block(Block::default().borders(Borders::ALL).title(">")),
        chunks[2],
    );
    if ui.pending.is_none() && input_inner.width > 0 && input_inner.height > 0 {
        let col = (display_cols(&ui.input) as u16).min(input_inner.width.saturating_sub(1));
        let x = input_inner.x.saturating_add(col);
        let y = input_inner.y;
        if x < f.area().right() && y < f.area().bottom() {
            f.set_cursor_position((x, y));
        }
    }
}

fn padding_wrap() -> Wrap {
    Wrap { trim: false }
}

/// Fit the line to the widget and pad the rest with spaces.
/// A 2-cell Hangul glyph that does not fit is left out, so the following
/// cells stay spaces and a leftover wide cell is overwritten (ratatui 0.29).
pub fn padded_line(text: &str, width: u16) -> String {
    let w = usize::from(width);
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if cw == 0 {
            continue;
        }
        if used + cw > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    if used < w {
        out.push_str(&" ".repeat(w - used));
    }
    out
}

pub fn ingest_char(buf: &mut String, c: char) {
    if c == '\u{8}' || c == '\u{7f}' {
        pop_input(buf);
        return;
    }
    if c.is_control() {
        return;
    }
    buf.push(c);
    compose_hangul_tail(buf);
}

/// Drop one jamo. `각` becomes `가`, then `ㄱ`.
pub fn pop_input(buf: &mut String) {
    let mut chs: Vec<char> = buf.chars().collect();
    let Some(last) = chs.pop() else {
        return;
    };
    if let Some((l, v, t)) = as_syllable(last) {
        if t == 0 {
            if let Some(cho) = L_COMPAT.get(l as usize) {
                chs.push(*cho);
            }
        } else {
            chs.push(syllable(l, v, 0));
        }
    }
    *buf = chs.into_iter().collect();
}

const L_COMPAT: &[char] = &[
    'ㄱ', 'ㄲ', 'ㄴ', 'ㄷ', 'ㄸ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅃ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅉ', 'ㅊ', 'ㅋ',
    'ㅌ', 'ㅍ', 'ㅎ',
];
const V_COMPAT: &[char] = &[
    'ㅏ', 'ㅐ', 'ㅑ', 'ㅒ', 'ㅓ', 'ㅔ', 'ㅕ', 'ㅖ', 'ㅗ', 'ㅘ', 'ㅙ', 'ㅚ', 'ㅛ', 'ㅜ', 'ㅝ', 'ㅞ',
    'ㅟ', 'ㅠ', 'ㅡ', 'ㅢ', 'ㅣ',
];
const T_COMPAT: &[char] = &[
    'ㄱ', 'ㄲ', 'ㄳ', 'ㄴ', 'ㄵ', 'ㄶ', 'ㄷ', 'ㄹ', 'ㄺ', 'ㄻ', 'ㄼ', 'ㄽ', 'ㄾ', 'ㄿ', 'ㅀ', 'ㅁ',
    'ㅂ', 'ㅄ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅊ', 'ㅋ', 'ㅌ', 'ㅍ', 'ㅎ',
];

fn as_l(c: char) -> Option<u32> {
    L_COMPAT
        .iter()
        .position(|x| *x == c)
        .map(|i| i as u32)
        .or_else(|| {
            let u = c as u32;
            (0x1100..=0x1112).contains(&u).then(|| u - 0x1100)
        })
}

fn as_v(c: char) -> Option<u32> {
    V_COMPAT
        .iter()
        .position(|x| *x == c)
        .map(|i| i as u32)
        .or_else(|| {
            let u = c as u32;
            (0x1161..=0x1175).contains(&u).then(|| u - 0x1161)
        })
}

fn as_t(c: char) -> Option<u32> {
    T_COMPAT
        .iter()
        .position(|x| *x == c)
        .map(|i| i as u32 + 1)
        .or_else(|| {
            let u = c as u32;
            (0x11A8..=0x11C2).contains(&u).then(|| u - 0x11A7)
        })
}

fn as_syllable(c: char) -> Option<(u32, u32, u32)> {
    let u = c as u32;
    if !(0xAC00..=0xD7A3).contains(&u) {
        return None;
    }
    let s = u - 0xAC00;
    let t = s % 28;
    let n = s / 28;
    Some((n / 21, n % 21, t))
}

fn jong_to_cho(t: u32) -> Option<u32> {
    Some(match t {
        1 => 0,
        2 => 1,
        4 => 2,
        7 => 3,
        8 => 5,
        16 => 6,
        17 => 7,
        19 => 9,
        20 => 10,
        21 => 11,
        22 => 12,
        23 => 14,
        24 => 15,
        25 => 16,
        26 => 17,
        27 => 18,
        _ => return None,
    })
}

fn syllable(l: u32, v: u32, t: u32) -> char {
    char::from_u32(0xAC00 + (l * 21 + v) * 28 + t).unwrap_or('\u{AC00}')
}

enum Compose {
    One(char),
    Two(char, char),
}

fn compose_step(a: char, b: char) -> Option<Compose> {
    if let (Some(l), Some(v)) = (as_l(a), as_v(b)) {
        return Some(Compose::One(syllable(l, v, 0)));
    }
    if let Some((l, v, t)) = as_syllable(a) {
        if t == 0 {
            if let Some(tt) = as_t(b) {
                return Some(Compose::One(syllable(l, v, tt)));
            }
        } else if let Some(vv) = as_v(b) {
            if let Some(nl) = jong_to_cho(t) {
                return Some(Compose::Two(syllable(l, v, 0), syllable(nl, vv, 0)));
            }
        }
    }
    None
}

pub fn compose_hangul_tail(s: &mut String) {
    loop {
        let mut chs: Vec<char> = s.chars().collect();
        if chs.len() < 2 {
            return;
        }
        let b = chs[chs.len() - 1];
        let a = chs[chs.len() - 2];
        let Some(step) = compose_step(a, b) else {
            return;
        };
        chs.pop();
        chs.pop();
        let done = matches!(step, Compose::Two(_, _));
        match step {
            Compose::One(c) => chs.push(c),
            Compose::Two(x, y) => {
                chs.push(x);
                chs.push(y);
            }
        }
        *s = chs.into_iter().collect();
        if done {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_3_30_interactive_requires_tty() {
        assert!(interactive_allowed(true));
        assert!(!interactive_allowed(false));
    }

    #[test]
    fn req_3_22_compact_is_notice_only() {
        assert_eq!(compact_notice(), "예정");
        assert_eq!(classify_input(false, "/compact"), InputAction::Compact);
    }

    #[test]
    fn req_3_22_slash_and_exit_are_commands() {
        assert_eq!(classify_input(false, "/exit"), InputAction::Exit);
        assert_eq!(classify_input(false, "/help"), InputAction::Help);
        assert_eq!(
            classify_input(false, "/plan approve"),
            InputAction::Slash("/plan approve".into())
        );
    }

    #[test]
    fn req_3_30_hangul_compose_and_backspace_drops_syllable() {
        let mut s = String::new();
        ingest_char(&mut s, 'ㄱ');
        ingest_char(&mut s, 'ㅏ');
        assert_eq!(s, "가");
        ingest_char(&mut s, 'ㄱ');
        assert_eq!(s, "각");
        ingest_char(&mut s, 'ㅏ');
        assert_eq!(s, "가가");
        pop_input(&mut s);
        assert_eq!(s, "가ㄱ");
        pop_input(&mut s);
        assert_eq!(s, "가");
        pop_input(&mut s);
        assert_eq!(s, "ㄱ");
        pop_input(&mut s);
        assert_eq!(s, "");
        let pad = padded_line("가", 4);
        assert_eq!(display_cols(&pad), 4);
        assert!(pad.starts_with('가'));
        assert!(pad.ends_with(' '));
        assert_eq!(padded_line("가", 1), " ");
        assert_eq!(display_cols(&padded_line("\u{200e}가", 4)), 4);
    }

    #[test]
    fn char_bytes_stay_in_the_session() {
        let mut buf = "a가".as_bytes().to_vec();
        assert!(matches!(
            pop_step(&mut buf, false),
            Step::Event(CEvent::Key(k)) if k.code == KeyCode::Char('a')
        ));
        assert!(matches!(
            pop_step(&mut buf, false),
            Step::Event(CEvent::Key(k)) if k.code == KeyCode::Char('가')
        ));
        assert!(buf.is_empty());

        let mut prefixed = "\u{1b}가".as_bytes().to_vec();
        assert!(matches!(
            pop_step(&mut prefixed, false),
            Step::Event(CEvent::Key(k)) if k.code == KeyCode::Char('가')
        ));

        let mut kitty = b"\x1b[44032u".to_vec();
        assert!(matches!(
            pop_step(&mut kitty, false),
            Step::Event(CEvent::Key(k)) if k.code == KeyCode::Char('가')
        ));

        let mut bs = vec![0x7f, 0x08];
        assert!(matches!(
            pop_step(&mut bs, false),
            Step::Event(CEvent::Key(k)) if k.code == KeyCode::Backspace
        ));
        assert!(matches!(
            pop_step(&mut bs, false),
            Step::Event(CEvent::Key(k)) if k.code == KeyCode::Backspace
        ));

        let mut lone = vec![0x1b];
        assert!(matches!(pop_step(&mut lone, false), Step::NeedMore));
        assert!(matches!(
            pop_step(&mut lone, true),
            Step::Event(CEvent::Key(k)) if k.code == KeyCode::Esc
        ));

        let mut report = b"\x1b[0;1R\x1b[<0;0;0M\x1b[<64;0;0M".to_vec();
        let mut saw_scroll = false;
        loop {
            match pop_step(&mut report, true) {
                Step::NeedMore => break,
                Step::Event(CEvent::Mouse(mouse)) if mouse.kind == MouseEventKind::ScrollUp => {
                    saw_scroll = true;
                }
                Step::Event(_) => {}
            }
        }
        assert!(saw_scroll);
        assert!(report.is_empty());
    }

    #[test]
    fn req_3_30_hangul_nfd_jamo_composes() {
        let mut s = String::new();
        ingest_char(&mut s, '\u{1100}');
        ingest_char(&mut s, '\u{1161}');
        assert_eq!(s, "가");
        ingest_char(&mut s, '\u{11A8}');
        assert_eq!(s, "각");
    }

    #[test]
    fn hangul_backspace_redraw_does_not_panic() {
        for (w, h) in [
            (80, 24),
            (40, 12),
            (20, 10),
            (10, 9),
            (8, 9),
            (5, 9),
            (4, 8),
        ] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut term = ratatui::Terminal::new(backend).unwrap();
            let mut ui = Ui {
                lines: vec!["session x".into(), "한글 본문".into()],
                input: String::new(),
                status: String::new(),
                last_tool: String::new(),
                running: false,
                pending: None,
                dashboard: false,
                prompt_ctrl_c: false,
                refresh_last: false,
                scheduler: false,
                dash_body: "dashboard — 한글 잡".into(),
                scroll: 0,
                follow: true,
                view_h: 1,
                view_w: 1,
            };
            for c in ["ㄱ", "ㅏ", "ㄴ", "ㅣ", "각", "가"] {
                for ch in c.chars() {
                    ingest_char(&mut ui.input, ch);
                }
                term.draw(|f| draw(f, &mut ui)).unwrap();
                pop_input(&mut ui.input);
                term.draw(|f| draw(f, &mut ui)).unwrap();
            }
            ui.dashboard = true;
            term.draw(|f| draw(f, &mut ui)).unwrap();
        }
    }

    fn replay(bytes: &[u8]) -> String {
        let mut buf = bytes.to_vec();
        let mut input = String::new();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        let mut ui = Ui {
            lines: vec!["session x".into()],
            input: String::new(),
            status: String::new(),
            last_tool: String::new(),
            running: false,
            pending: None,
            dashboard: false,
            prompt_ctrl_c: false,
            refresh_last: false,
            scheduler: false,
            dash_body: String::new(),
            scroll: 0,
            follow: true,
            view_h: 1,
            view_w: 1,
        };
        loop {
            match pop_step(&mut buf, true) {
                Step::NeedMore => break,
                Step::Event(CEvent::Key(key)) => {
                    match key.code {
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            ingest_char(&mut input, c);
                        }
                        KeyCode::Backspace => pop_input(&mut input),
                        KeyCode::Esc if !input.is_empty() => pop_input(&mut input),
                        _ => {}
                    }
                    ui.input.clone_from(&input);
                    term.draw(|f| draw(f, &mut ui)).unwrap();
                }
                Step::Event(_) => {}
            }
        }
        input
    }

    #[test]
    fn key_patterns_do_not_overflow() {
        let cases = [
            ("aa", "aa"),
            ("dd", "dd"),
            ("/a", "/a"),
            ("a/", "a/"),
            ("12", "12"),
            ("AB", "AB"),
            ("!@#", "!@#"),
            ("  ", "  "),
            ("aㄱ", "aㄱ"),
            ("ㅇㅇ", "ㅇㅇ"),
            ("/ㅇ", "/ㅇ"),
            ("ㄱㅏ", "가"),
            ("ㄱㅏㄴ", "간"),
            ("ㄱㅏㄱ", "각"),
            ("ㄱㅏㄱㅏ", "가가"),
            ("가나", "가나"),
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        ];
        for (raw, expect) in cases {
            assert_eq!(replay(raw.as_bytes()), expect, "{raw}");
        }
        assert_eq!(replay("ab\u{7f}c".as_bytes()), "ac");
        assert_eq!(replay("가\u{8}".as_bytes()), "ㄱ");
        assert_eq!(replay("각\u{7f}\u{7f}\u{7f}".as_bytes()), "");
        assert_eq!(replay(b"\x1b[A\x1b[Ba"), "a");
        assert_eq!(replay(b"\x1b[0;1Ra\x1b[<0;0;0Mb"), "ab");
        assert_eq!(replay(b"\x1b[97u\x1b[98u"), "ab");
        assert_eq!(replay(b"\x1b[<64;0;0Mx"), "x");
        assert_eq!(replay(&[0x7f, 0x7f, b'a', b'a']), "aa");
        // Bytes from the session that died on the second key.
        assert_eq!(replay(b"\x2f\x61"), "/a");
        assert_eq!(replay(b"\x64\x64"), "dd");
        assert_eq!(replay(b"\x61\x61"), "aa");
        assert_eq!(replay("\u{3147}\u{3147}".as_bytes()), "ㅇㅇ");
        assert_eq!(replay("/\u{3147}".as_bytes()), "/ㅇ");
    }

    #[test]
    fn req_3_30_scroll_clamps_and_follow_bottom() {
        assert_eq!(wrapped_rows("a\nb\nc", 10), 3);
        assert_eq!(wrapped_rows("abcdefghij", 4), 3);
        assert_eq!(max_scroll(10, 4), 6);
        assert_eq!(apply_scroll(0, 3, 10, 4), (3, false));
        assert_eq!(apply_scroll(5, 10, 10, 4), (6, true));
        assert_eq!(apply_scroll(2, -10, 10, 4), (0, false));
    }

    #[test]
    fn req_3_31_running_text_is_steer_not_cancel() {
        assert_eq!(
            classify_input(true, "nudge the model"),
            InputAction::Steer("nudge the model".into())
        );
        assert_eq!(
            classify_input(false, "nudge the model"),
            InputAction::Prompt("nudge the model".into())
        );
        assert_eq!(classify_input(true, "/exit"), InputAction::Exit);
    }

    #[tokio::test]
    async fn req_3_31_steer_lands_in_inbox() {
        let inbox = Inbox::new();
        match classify_input(true, "keep going") {
            InputAction::Steer(text) => {
                inbox.push(InboxMessage::Steer { text }).await;
            }
            other => panic!("{other:?}"),
        }
        let drained = inbox.drain().await;
        assert!(
            matches!(drained.as_slice(), [InboxMessage::Steer { text }] if text == "keep going")
        );
    }
}
