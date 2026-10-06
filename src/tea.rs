//! A minimal Elm-architecture terminal runtime modeled on bubbletea: the app
//! turns messages into state changes plus commands, commands run on worker
//! threads, and their results come back as messages.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::terminal;

use crate::ansi;

/// Work requested by an update; `None` stands for "no command".
pub enum Cmd<M> {
    Run(Box<dyn FnOnce() -> M + Send>),
    Tick(Duration, M),
    Batch(Vec<Cmd<M>>),
    Exec(Command, Box<dyn FnOnce(io::Result<ExitStatus>) -> M + Send>),
    Quit,
}

impl<M> Cmd<M> {
    pub fn run(f: impl FnOnce() -> M + Send + 'static) -> Self {
        Self::Run(Box::new(f))
    }

    /// Hands the terminal to `command` until it exits, then reports its status.
    pub fn exec(command: Command, done: impl FnOnce(io::Result<ExitStatus>) -> M + Send + 'static) -> Self {
        Self::Exec(command, Box::new(done))
    }
}

/// Combines commands like tea.Batch: absent when none remain, the command
/// itself when only one does.
pub fn batch<M>(cmds: impl IntoIterator<Item = Option<Cmd<M>>>) -> Option<Cmd<M>> {
    let mut cmds: Vec<Cmd<M>> = cmds.into_iter().flatten().collect();
    match cmds.len() {
        0 => None,
        1 => cmds.pop(),
        _ => Some(Cmd::Batch(cmds)),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyCode {
    Runes(String),
    Space,
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    ShiftTab,
    Backspace,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PgUp,
    PgDown,
    F(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub paste: bool,
}

impl Key {
    pub fn new(code: KeyCode) -> Self {
        Self { code, alt: false, ctrl: false, shift: false, paste: false }
    }

    pub fn runes(text: &str) -> Self {
        Self::new(KeyCode::Runes(text.to_string()))
    }

    /// The key's name as bubbletea's KeyMsg.String() spells it.
    pub fn name(&self) -> String {
        let named = |name: &str| {
            let mut out = String::new();
            if self.ctrl {
                out.push_str("ctrl+");
            }
            if self.shift {
                out.push_str("shift+");
            }
            out + name
        };
        let base = match &self.code {
            KeyCode::Runes(text) if self.paste => format!("[{text}]"),
            KeyCode::Runes(text) => text.clone(),
            KeyCode::Space => " ".to_string(),
            KeyCode::Ctrl(c) => format!("ctrl+{c}"),
            KeyCode::Enter => named("enter"),
            KeyCode::Esc => named("esc"),
            KeyCode::Tab => named("tab"),
            KeyCode::ShiftTab => "shift+tab".to_string(),
            KeyCode::Backspace => named("backspace"),
            KeyCode::Delete => named("delete"),
            KeyCode::Insert => named("insert"),
            KeyCode::Up => named("up"),
            KeyCode::Down => named("down"),
            KeyCode::Left => named("left"),
            KeyCode::Right => named("right"),
            KeyCode::Home => named("home"),
            KeyCode::End => named("end"),
            KeyCode::PgUp => named("pgup"),
            KeyCode::PgDown => named("pgdown"),
            KeyCode::F(n) => named(&format!("f{n}")),
        };
        if self.alt { format!("alt+{base}") } else { base }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    None,
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub x: i32,
    pub y: i32,
    pub action: MouseAction,
    pub button: MouseButton,
}

impl Mouse {
    pub fn is_wheel(&self) -> bool {
        matches!(
            self.button,
            MouseButton::WheelUp | MouseButton::WheelDown | MouseButton::WheelLeft | MouseButton::WheelRight
        )
    }
}

pub enum Event {
    Key(Key),
    Mouse(Mouse),
    Resize { width: usize, height: usize },
}

pub trait App {
    type Msg: Send + 'static + From<Event>;
    fn init(&mut self) -> Option<Cmd<Self::Msg>>;
    fn update(&mut self, msg: Self::Msg) -> Option<Cmd<Self::Msg>>;
    fn view(&self) -> String;
}

// Alt screen, hidden cursor, button-event mouse tracking in SGR encoding
// (bubbletea's WithMouseCellMotion), and bracketed paste.
const ENTER: &str = "\x1b[?1049h\x1b[2J\x1b[?25l\x1b[?1002h\x1b[?1006h\x1b[?2004h";
const LEAVE: &str = "\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?25h\x1b[?1049l";

fn enter_screen() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    out.write_all(ENTER.as_bytes())?;
    out.flush()
}

fn leave_screen() {
    let mut out = io::stdout();
    let _ = out.write_all(LEAVE.as_bytes());
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
}

struct Screen;

impl Screen {
    fn enter() -> io::Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            leave_screen();
            previous(info);
        }));
        enter_screen()?;
        Ok(Self)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        leave_screen();
    }
}

/// Runs the app until it quits or the process gets SIGINT/SIGTERM.
pub fn run<A: App>(app: &mut A) -> io::Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&interrupted))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&interrupted))?;
    let _screen = Screen::enter()?;
    let (tx, rx) = mpsc::channel();
    let mut renderer = Renderer::default();
    let mut queue = VecDeque::new();
    let (width, height) = terminal::size()?;
    renderer.resize(width.into(), height.into());
    queue.push_back(A::Msg::from(Event::Resize { width: width.into(), height: height.into() }));
    let mut execs = Vec::new();
    if let Some(cmd) = app.init()
        && dispatch(cmd, &tx, &mut execs)
    {
        return Ok(());
    }
    loop {
        if interrupted.load(Ordering::Relaxed) {
            return Ok(());
        }
        let changed = !queue.is_empty();
        while let Some(msg) = queue.pop_front() {
            if let Some(cmd) = app.update(msg)
                && dispatch(cmd, &tx, &mut execs)
            {
                return Ok(());
            }
            for (mut command, done) in execs.drain(..) {
                leave_screen();
                let status = command.status();
                enter_screen()?;
                queue.push_back(done(status));
                let (width, height) = terminal::size()?;
                renderer.resize(width.into(), height.into());
                queue.push_back(A::Msg::from(Event::Resize { width: width.into(), height: height.into() }));
            }
        }
        if changed {
            renderer.draw(&app.view())?;
        }
        if event::poll(Duration::from_millis(16))?
            && let Some(ev) = translate(event::read()?)
        {
            if let Event::Resize { width, height } = ev {
                renderer.resize(width, height);
            }
            queue.push_back(A::Msg::from(ev));
        }
        while let Ok(msg) = rx.try_recv() {
            queue.push_back(msg);
        }
    }
}

type PendingExec<M> = (Command, Box<dyn FnOnce(io::Result<ExitStatus>) -> M + Send>);

/// Starts a command's work; returns true when it asks the program to quit.
fn dispatch<M: Send + 'static>(cmd: Cmd<M>, tx: &Sender<M>, execs: &mut Vec<PendingExec<M>>) -> bool {
    match cmd {
        Cmd::Run(f) => {
            let tx = tx.clone();
            thread::spawn(move || {
                let _ = tx.send(f());
            });
        }
        Cmd::Tick(delay, msg) => {
            let tx = tx.clone();
            thread::spawn(move || {
                thread::sleep(delay);
                let _ = tx.send(msg);
            });
        }
        Cmd::Batch(cmds) => {
            for cmd in cmds {
                if dispatch(cmd, tx, execs) {
                    return true;
                }
            }
        }
        Cmd::Exec(command, done) => execs.push((command, done)),
        Cmd::Quit => return true,
    }
    false
}

fn translate(ev: event::Event) -> Option<Event> {
    match ev {
        event::Event::Key(key) if key.kind != KeyEventKind::Release => translate_key(key).map(Event::Key),
        event::Event::Mouse(mouse) => Some(Event::Mouse(translate_mouse(mouse))),
        event::Event::Paste(text) => Some(Event::Key(Key { paste: true, ..Key::new(KeyCode::Runes(text)) })),
        event::Event::Resize(width, height) => Some(Event::Resize { width: width.into(), height: height.into() }),
        _ => None,
    }
}

fn translate_key(key: event::KeyEvent) -> Option<Key> {
    use event::KeyCode as K;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let code = match key.code {
        K::Char(c) if ctrl => KeyCode::Ctrl(c.to_ascii_lowercase()),
        K::Char(' ') => KeyCode::Space,
        K::Char(c) => KeyCode::Runes(c.to_string()),
        K::Enter => KeyCode::Enter,
        K::Esc => KeyCode::Esc,
        K::Tab => KeyCode::Tab,
        K::BackTab => KeyCode::ShiftTab,
        K::Backspace => KeyCode::Backspace,
        K::Delete => KeyCode::Delete,
        K::Insert => KeyCode::Insert,
        K::Up => KeyCode::Up,
        K::Down => KeyCode::Down,
        K::Left => KeyCode::Left,
        K::Right => KeyCode::Right,
        K::Home => KeyCode::Home,
        K::End => KeyCode::End,
        K::PageUp => KeyCode::PgUp,
        K::PageDown => KeyCode::PgDown,
        K::F(n) => KeyCode::F(n),
        _ => return None,
    };
    let named = !matches!(code, KeyCode::Runes(_) | KeyCode::Space | KeyCode::Ctrl(_));
    Some(Key {
        code,
        alt: key.modifiers.contains(KeyModifiers::ALT),
        ctrl: named && ctrl,
        shift: named && key.modifiers.contains(KeyModifiers::SHIFT),
        paste: false,
    })
}

fn translate_mouse(mouse: event::MouseEvent) -> Mouse {
    use event::MouseButton as B;
    let button = |b: B| match b {
        B::Left => MouseButton::Left,
        B::Middle => MouseButton::Middle,
        B::Right => MouseButton::Right,
    };
    let (action, button) = match mouse.kind {
        MouseEventKind::Down(b) => (MouseAction::Press, button(b)),
        MouseEventKind::Up(b) => (MouseAction::Release, button(b)),
        MouseEventKind::Drag(b) => (MouseAction::Motion, button(b)),
        MouseEventKind::Moved => (MouseAction::Motion, MouseButton::None),
        MouseEventKind::ScrollUp => (MouseAction::Press, MouseButton::WheelUp),
        MouseEventKind::ScrollDown => (MouseAction::Press, MouseButton::WheelDown),
        MouseEventKind::ScrollLeft => (MouseAction::Press, MouseButton::WheelLeft),
        MouseEventKind::ScrollRight => (MouseAction::Press, MouseButton::WheelRight),
    };
    Mouse { x: mouse.column.into(), y: mouse.row.into(), action, button }
}

/// Repaints only the rows that changed since the previous frame.
#[derive(Default)]
struct Renderer {
    lines: Vec<String>,
    width: usize,
    height: usize,
    repaint: bool,
}

impl Renderer {
    fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.repaint = true;
    }

    fn draw(&mut self, view: &str) -> io::Result<()> {
        let mut lines: Vec<String> = view
            .split('\n')
            .map(|line| {
                if self.width > 0 && ansi::width(line) > self.width {
                    ansi::truncate(line, self.width, "")
                } else {
                    line.to_string()
                }
            })
            .collect();
        if self.height > 0 && lines.len() > self.height {
            lines.drain(..lines.len() - self.height);
        }
        let mut out = String::new();
        if self.repaint {
            out.push_str("\x1b[2J");
        }
        for (row, line) in lines.iter().enumerate() {
            if !self.repaint && self.lines.get(row) == Some(line) {
                continue;
            }
            let _ = write!(out, "\x1b[{};1H{line}", row + 1);
            if ansi::width(line) < self.width {
                out.push_str("\x1b[K");
            }
        }
        for row in lines.len()..self.lines.len() {
            let _ = write!(out, "\x1b[{};1H\x1b[2K", row + 1);
        }
        self.lines = lines;
        self.repaint = false;
        if out.is_empty() {
            return Ok(());
        }
        let mut stdout = io::stdout();
        stdout.write_all(out.as_bytes())?;
        stdout.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_match_bubbletea() {
        assert_eq!(Key::runes("q").name(), "q");
        assert_eq!(Key::new(KeyCode::Ctrl('c')).name(), "ctrl+c");
        assert_eq!(Key::new(KeyCode::PgUp).name(), "pgup");
        assert_eq!(Key { shift: true, ..Key::new(KeyCode::Up) }.name(), "shift+up");
        assert_eq!(Key { paste: true, ..Key::runes("hi") }.name(), "[hi]");
        assert_eq!(Key { alt: true, ..Key::runes("a") }.name(), "alt+a");
        assert_eq!(Key::new(KeyCode::Space).name(), " ");
    }

    #[test]
    fn batch_collapses_like_tea_batch() {
        assert!(batch::<()>([None, None]).is_none());
        assert!(matches!(batch([None, Some(Cmd::Quit::<()>)]), Some(Cmd::Quit)));
        assert!(matches!(batch([Some(Cmd::Quit::<()>), Some(Cmd::Quit)]), Some(Cmd::Batch(v)) if v.len() == 2));
    }
}
