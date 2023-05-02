mod location;
mod terminal;

use anyhow::{format_err, Context as _};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use handy::typed::{TypedHandle, TypedHandleMap};
use log::{error, info, trace};
use ropey::Rope;
use shlex::split as shlex;
use signal_hook::{iterator::Signals, SIGWINCH};
use std::{
    collections::VecDeque,
    convert::Infallible,
    fmt::Debug,
    fs::{File, OpenOptions},
    io::{self, Write as _},
    mem::take,
    os::raw::c_int,
    path::PathBuf,
    thread,
};
use termion::{
    clear,
    color::{self, Color},
    cursor,
    event::{Event, Key},
    get_tty,
    input::TermRead,
    raw::{IntoRawMode, RawTerminal},
    screen, style, terminal_size,
};
use {
    location::{Column, Line, Movement, MovementError, Position, Selection},
    terminal::{Point, Rect},
};

type Result<T, E = anyhow::Error> = anyhow::Result<T, E>;

type WindowId = TypedHandle<Window>;
type BufferId = TypedHandle<Buffer>;
type SelectionId = TypedHandle<Selection>;

pub struct State {
    signals: Receiver<c_int>,
    inputs: Receiver<io::Result<Event>>,
    exit_channels: (Sender<()>, Receiver<()>),
    windows: TypedHandleMap<Window>,
    buffers: TypedHandleMap<Buffer>,
    open_tabs: Vec<WindowId>,
    focused_tab: usize,
    tty: RawTerminal<File>,
    tabline_needs_redraw: bool,
    statusline_needs_redraw: bool,
    last_screen_height: Option<u16>,
    pending_message: Option<(Importance, String)>,
}

pub struct Window {
    buffer: BufferId,
    mode: Mode,
    selections: TypedHandleMap<Selection>,
    primary_selection: SelectionId,
    command: String,
    top: Line,
}

pub struct Buffer {
    pub path: Option<PathBuf>,
    pub name: String,
    pub content: Rope,
    pub history: History,
}

pub struct NothingLeftToUndo;

#[derive(Default)]
pub struct History {
    edits: VecDeque<Edit>,
}

#[derive(Debug, Clone)]
pub enum Edit {
    Insert { pos: Position, text: String },
    Delete { pos: Position, text: String },
}

#[derive(Debug, Copy, Clone)]
pub enum Mode {
    Normal,
    Insert,
    Append,
    Goto { selecting: bool },
    Command,
}

#[derive(Debug, Copy, Clone)]
pub enum Importance {
    Error,
}

pub struct Context<'a> {
    editor: &'a mut State,
    window: WindowId,
}

pub struct CommandDesc {
    name: &'static str,
    aliases: &'static [&'static str],
    #[allow(dead_code)]
    description: &'static str,
    #[allow(dead_code)]
    required_arguments: usize,
    run: fn(cx: Context, args: &[&str]) -> Result<()>,
}

fn main() -> Result<()> {
    env_logger::init();
    let mut state = {
        let (signals, signal) = unbounded();
        let (inputs, input) = unbounded();
        let signal_iter = Signals::new([SIGWINCH])?;
        thread::spawn(move || {
            for signal in signal_iter.forever() {
                signals.send(signal).unwrap();
            }
        });
        let tty = get_tty()?;
        thread::spawn(move || {
            for event in tty.events() {
                inputs.send(event).unwrap();
            }
        });
        let mut windows = TypedHandleMap::new();
        let mut buffers = TypedHandleMap::new();
        let scratch_buffer = buffers.insert(Buffer {
            content: Rope::from("\n"),
            name: String::from("scratch"),
            history: History::default(),
            path: None,
        });
        let mut selections = TypedHandleMap::new();
        let primary_selection = selections.insert(Selection {
            start: Position::file_start(),
            end: Position::file_start(),
        });
        let focused_window = windows.insert(Window {
            buffer: scratch_buffer,
            mode: Mode::Normal,
            selections,
            primary_selection,
            command: String::new(),
            top: Line::from_one_based(1),
        });
        State {
            signals: signal,
            inputs: input,
            exit_channels: unbounded(),
            windows,
            buffers,
            open_tabs: vec![focused_window],
            focused_tab: 0,
            tty: get_tty()?.into_raw_mode()?,
            tabline_needs_redraw: true,
            statusline_needs_redraw: true,
            last_screen_height: None,
            pending_message: None,
        }
    };
    fn handle_next_event(state: &mut State) -> Result<bool> {
        select! {
            recv(state.inputs) -> input => handle_event(state, input??)?,
            recv(state.signals) -> signal => handle_signal(state, signal?)?,
            recv(state.exit_channels.1) -> exit => { exit?; return Ok(false); },
        }
        Ok(true)
    }

    write!(
        state.tty,
        "{}{}{}",
        screen::ToAlternateScreen,
        cursor::Hide,
        cursor::SteadyBar
    )?;
    loop {
        draw(&mut state)?;
        match handle_next_event(&mut state) {
            Ok(true) => continue,
            Ok(false) => return Ok(()),
            Err(err) => {
                error!("{}", err);
                show_message(&mut state, Importance::Error, err.to_string());
            }
        }
    }
}

fn run_command(state: &mut State, args: &[&str]) -> Result<()> {
    let name = args.first().copied().context("no command given")?;
    let cmd = COMMANDS
        .iter()
        .find(|desc| desc.name == name || desc.aliases.contains(&name))
        .ok_or_else(|| format_err!("command '{}' doesn't exist", name))?;
    (cmd.run)(
        Context {
            window: state.open_tabs[state.focused_tab],
            editor: state,
        },
        &args[1..],
    )
}

#[allow(dead_code)]
fn move_to(state: &mut State, movement: Movement, selecting: bool) -> Result<(), MovementError> {
    let window_id = state.open_tabs[state.focused_tab];
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    for selection in window.selections.iter_mut() {
        selection.move_to(&buffer.content, movement, selecting)?
    }
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Debug, Copy, Clone)]
enum Action {
    // Window actions
    Editor_PreviousTab,
    Editor_NextTab,
    // Buffer actions
    Buffer_Undo,
    Buffer_Redo,
    // Window actions
    Window_InsertAtSelectionStart(char),
    Window_InsertAtSelectionEnd(char),
    Window_Delete,
    Window_Move(Movement),
    Window_ShiftStart(Movement),
    Window_ShiftEnd(Movement),
    Window_ScrollPageUp,
    Window_ScrollPageDown,
    Window_ScrollHalfPageUp,
    Window_ScrollHalfPageDown,
    Window_OrderSelections,
    Window_SwitchToMode(Mode),
    // Command actions
    Command_Character(char),
    Command_Clear,
    Command_Tab,
    Command_Return,
    Command_Backspace,
}

fn do_action(state: &mut State, action: Action) -> Result<()> {
    match action {
        Action::Editor_PreviousTab => {
            state.focused_tab = (state.focused_tab - 1) % state.open_tabs.len();
            Ok(())
        }
        Action::Editor_NextTab => {
            state.focused_tab = (state.focused_tab + 1) % state.open_tabs.len();
            Ok(())
        }
        Action::Buffer_Undo => {
            undo(state, state.open_tabs[state.focused_tab]);
            Ok(())
        }
        Action::Buffer_Redo => {
            redo(state, state.open_tabs[state.focused_tab]);
            Ok(())
        }
        action @ (Action::Window_InsertAtSelectionStart(_)
        | Action::Window_InsertAtSelectionEnd(_)
        | Action::Window_Delete
        | Action::Window_Move(_)
        | Action::Window_ShiftStart(_)
        | Action::Window_ShiftEnd(_)
        | Action::Window_ScrollPageUp
        | Action::Window_ScrollPageDown
        | Action::Window_ScrollHalfPageUp
        | Action::Window_ScrollHalfPageDown
        | Action::Window_OrderSelections) => {
            let window_id = state.open_tabs[state.focused_tab];
            let window = &mut state.windows[window_id];
            let buffer = &mut state.buffers[window.buffer];
            for selection in window.selections.iter_mut() {
                match action {
                    Action::Window_InsertAtSelectionStart(c) => {
                        selection.start.insert_char(buffer, c);
                    }
                    Action::Window_InsertAtSelectionEnd(c) => {
                        selection.end.insert_char(buffer, c);
                    }
                    Action::Window_Delete => {
                        selection.remove_from(buffer);
                    }
                    Action::Window_Move(movement) => {
                        selection.end.move_to(&buffer.content, movement)?;
                        selection.start = selection.end;
                    }
                    Action::Window_ShiftStart(movement) => {
                        selection.start.move_to(&buffer.content, movement)?;
                    }
                    Action::Window_ShiftEnd(movement) => {
                        selection.end.move_to(&buffer.content, movement)?;
                    }
                    Action::Window_ScrollPageUp
                    | Action::Window_ScrollPageDown
                    | Action::Window_ScrollHalfPageUp
                    | Action::Window_ScrollHalfPageDown => {
                        if let Some(height) = state.last_screen_height {
                            let height = usize::from(height);
                            let movement = match action {
                                Action::Window_ScrollPageUp => Movement::Up(height),
                                Action::Window_ScrollPageDown => Movement::Down(height),
                                Action::Window_ScrollHalfPageUp => Movement::Up(height / 2),
                                Action::Window_ScrollHalfPageDown => Movement::Down(height / 2),
                                _ => unreachable!(),
                            };
                            selection.end.move_to(&buffer.content, movement)?;
                            selection.start = selection.end;
                        }
                    }
                    Action::Window_OrderSelections => {
                        selection.order();
                    }
                    Action::Window_SwitchToMode(_)
                    | Action::Editor_PreviousTab
                    | Action::Editor_NextTab
                    | Action::Buffer_Undo
                    | Action::Buffer_Redo
                    | Action::Command_Character(_)
                    | Action::Command_Clear
                    | Action::Command_Tab
                    | Action::Command_Return
                    | Action::Command_Backspace => {
                        unreachable!()
                    }
                }
            }
            Ok(())
        }
        Action::Window_SwitchToMode(mode) => {
            state.windows[state.open_tabs[state.focused_tab]].mode = mode;
            Ok(())
        }
        Action::Command_Character(c) => {
            state.windows[state.open_tabs[state.focused_tab]]
                .command
                .push(c);
            Ok(())
        }
        Action::Command_Clear => {
            state.windows[state.open_tabs[state.focused_tab]]
                .command
                .clear();
            Ok(())
        }
        Action::Command_Tab => {
            // TODO
            Ok(())
        }
        Action::Command_Return => {
            let command = take(&mut state.windows[state.open_tabs[state.focused_tab]].command);
            state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Normal;
            let command = shlex(&command)
                .ok_or_else(|| format_err!("failed to parse command '{}'", command))?;
            trace!("command: {:?}", command);
            let command = command.iter().map(|x| &**x).collect::<Vec<&str>>();
            run_command(state, &command)?;
            Ok(())
        }
        Action::Command_Backspace => {
            if state.windows[state.open_tabs[state.focused_tab]]
                .command
                .pop()
                .is_none()
            {
                let mode: Mode = Mode::Normal;
                state.windows[state.open_tabs[state.focused_tab]].mode = mode;
            }
            Ok(())
        }
    }
}

fn handle_event(state: &mut State, event: Event) -> Result<()> {
    trace!("event: {:?}", event);

    const SHIFT_UP: &[u8] = &[27, 91, 49, 59, 50, 65];
    const SHIFT_DOWN: &[u8] = &[27, 91, 49, 59, 50, 66];
    const SHIFT_RIGHT: &[u8] = &[27, 91, 49, 59, 50, 67];
    const SHIFT_LEFT: &[u8] = &[27, 91, 49, 59, 50, 68];

    let mut actions = Vec::new();

    if let Mode::Normal | Mode::Insert | Mode::Append =
        state.windows[state.open_tabs[state.focused_tab]].mode
    {
        match &event {
            Event::Key(Key::Left) => actions.push(Action::Window_Move(Movement::Left(1))),
            Event::Key(Key::Down) => actions.push(Action::Window_Move(Movement::Down(1))),
            Event::Key(Key::Up) => actions.push(Action::Window_Move(Movement::Up(1))),
            Event::Key(Key::Right) => actions.push(Action::Window_Move(Movement::Right(1))),
            Event::Key(Key::Ctrl('u')) => actions.push(Action::Window_ScrollHalfPageUp),
            Event::Key(Key::Ctrl('d')) => actions.push(Action::Window_ScrollHalfPageDown),
            Event::Key(Key::Ctrl('b') | Key::PageUp) => {
                actions.push(Action::Window_ScrollPageUp);
            }
            Event::Key(Key::Ctrl('f') | Key::PageDown) => {
                actions.push(Action::Window_ScrollPageDown);
            }
            Event::Key(Key::Ctrl('p')) => actions.push(Action::Editor_PreviousTab),
            Event::Key(Key::Ctrl('n')) => actions.push(Action::Editor_NextTab),
            Event::Unsupported(keys) => match keys.as_slice() {
                SHIFT_LEFT => actions.push(Action::Window_ShiftEnd(Movement::Left(1))),
                SHIFT_DOWN => actions.push(Action::Window_ShiftEnd(Movement::Down(1))),
                SHIFT_UP => actions.push(Action::Window_ShiftEnd(Movement::Up(1))),
                SHIFT_RIGHT => actions.push(Action::Window_ShiftEnd(Movement::Right(1))),
                _ => {}
            },
            _ => {}
        }
    }

    match state.windows[state.open_tabs[state.focused_tab]].mode {
        Mode::Normal => match event {
            Event::Key(Key::Char('i')) => {
                actions.push(Action::Window_OrderSelections);
                actions.push(Action::Window_SwitchToMode(Mode::Insert));
            }
            Event::Key(Key::Char('c')) => {
                actions.push(Action::Window_Delete);
                actions.push(Action::Window_SwitchToMode(Mode::Insert));
            }
            Event::Key(Key::Char('a')) => {
                actions.push(Action::Window_OrderSelections);
                actions.push(Action::Window_SwitchToMode(Mode::Append));
            }
            Event::Key(Key::Char('A')) => {
                actions.push(Action::Window_Move(Movement::LineEnd));
                actions.push(Action::Window_SwitchToMode(Mode::Insert));
            }
            Event::Key(Key::Char('o')) => {
                actions.push(Action::Window_Move(Movement::LineEnd));
                actions.push(Action::Window_InsertAtSelectionEnd('\n'));
                actions.push(Action::Window_Move(Movement::Down(1)));
                actions.push(Action::Window_Move(Movement::LineStart));
                actions.push(Action::Window_SwitchToMode(Mode::Insert));
            }
            Event::Key(Key::Char('x')) => {
                //self.move_selections(self.focused, Movement::Line, false)?;
            }
            Event::Key(Key::Char('X')) => {
                //self.move_selections(self.focused, Movement::Line, true)?;
            }
            Event::Key(Key::Char('g')) => {
                actions.push(Action::Window_SwitchToMode(Mode::Goto { selecting: false }));
            }
            Event::Key(Key::Char('G')) => {
                actions.push(Action::Window_SwitchToMode(Mode::Goto { selecting: true }));
            }
            Event::Key(Key::Char(':')) => actions.push(Action::Window_SwitchToMode(Mode::Command)),
            Event::Key(Key::Char('h')) => actions.push(Action::Window_Move(Movement::Left(1))),
            Event::Key(Key::Char('j')) => actions.push(Action::Window_Move(Movement::Down(1))),
            Event::Key(Key::Char('k')) => actions.push(Action::Window_Move(Movement::Up(1))),
            Event::Key(Key::Char('l')) => actions.push(Action::Window_Move(Movement::Right(1))),
            Event::Key(Key::Char('H')) => actions.push(Action::Window_ShiftEnd(Movement::Left(1))),
            Event::Key(Key::Char('J')) => actions.push(Action::Window_ShiftEnd(Movement::Down(1))),
            Event::Key(Key::Char('K')) => actions.push(Action::Window_ShiftEnd(Movement::Up(1))),
            Event::Key(Key::Char('L')) => actions.push(Action::Window_ShiftEnd(Movement::Right(1))),
            Event::Key(Key::Char('d')) => actions.push(Action::Window_Delete),
            Event::Key(Key::Char('u')) => actions.push(Action::Buffer_Undo),
            Event::Key(Key::Char('U')) => actions.push(Action::Buffer_Redo),
            _ => {}
        },
        Mode::Goto { selecting } => {
            let wrapper = if selecting {
                Action::Window_ShiftEnd
            } else {
                Action::Window_Move
            };
            let movement = match event {
                Event::Key(Key::Char('h')) => Some(Movement::LineStart),
                Event::Key(Key::Char('j')) => Some(Movement::FileEnd),
                Event::Key(Key::Char('k')) => Some(Movement::FileStart),
                Event::Key(Key::Char('l')) => Some(Movement::LineEnd),
                _ => None,
            };
            if let Some(movement) = movement {
                actions.push(wrapper(movement));
            }
            actions.push(Action::Window_SwitchToMode(Mode::Normal))
        }
        mode @ Mode::Insert | mode @ Mode::Append => match event {
            Event::Key(Key::Esc) => actions.push(Action::Window_SwitchToMode(Mode::Normal)),
            Event::Key(Key::Char(c)) => match mode {
                Mode::Insert => {
                    actions.push(Action::Window_InsertAtSelectionStart(c));
                    actions.push(Action::Window_ShiftStart(Movement::Right(1)));
                    actions.push(Action::Window_ShiftEnd(Movement::Right(1)));
                }
                Mode::Append => {
                    actions.push(Action::Window_ShiftEnd(Movement::Right(1)));
                    actions.push(Action::Window_InsertAtSelectionEnd(c));
                }
                _ => unreachable!(),
            },
            Event::Key(Key::Backspace) => {
                actions.push(Action::Window_Move(Movement::Left(1)));
                actions.push(Action::Window_Delete);
            }
            _ => {}
        },
        Mode::Command => match event {
            Event::Key(Key::Esc) => {
                actions.push(Action::Command_Clear);
                actions.push(Action::Window_SwitchToMode(Mode::Normal));
            }
            Event::Key(Key::Char('\t')) => actions.push(Action::Command_Tab),
            Event::Key(Key::Char('\n')) => actions.push(Action::Command_Return),
            Event::Key(Key::Char(c)) => actions.push(Action::Command_Character(c)),
            Event::Key(Key::Backspace) => actions.push(Action::Command_Backspace),
            _ => {}
        },
    }

    if let Err(e) = actions
        .into_iter()
        .try_for_each(|action| do_action(state, action))
    {
        state.pending_message = Some((Importance::Error, e.to_string()));
    }
    Ok(())
}

fn handle_signal(state: &mut State, signal: c_int) -> Result<()> {
    info!("received signal: {}", signal);
    #[allow(clippy::single_match)]
    match signal {
        signal_hook::SIGWINCH => draw(state)?,
        _ => {}
    }
    Ok(())
}

fn draw(state: &mut State) -> Result<()> {
    let (width, height) = terminal_size()?;

    let region = Rect {
        start: Point { x: 1, y: 1 },
        end: Point { x: width, y: 1 },
    };
    draw_tabs(state, region)?;

    let region = Rect {
        start: Point { x: 1, y: 2 },
        end: Point {
            x: width,
            y: height - 1,
        },
    };
    draw_window(state, state.open_tabs[state.focused_tab], region)?;
    state.last_screen_height = Some(region.height());

    let region = Rect {
        start: Point { x: 1, y: height },
        end: Point {
            x: width,
            y: height,
        },
    };
    draw_status(state, region)?;

    state.tty.flush()?;
    Ok(())
}

fn draw_tabs(state: &mut State, region: Rect) -> Result<()> {
    write!(state.tty, "{}{}", region.start.goto(), clear::CurrentLine)?;
    for (window_id, window) in state.windows.iter_with_handles() {
        let buffer = &state.buffers[window.buffer];
        if window_id == state.open_tabs[state.focused_tab] {
            write!(state.tty, "{}{}{} ", style::Bold, buffer.name, style::Reset,)?;
        } else {
            write!(state.tty, "{} ", buffer.name)?;
        }
    }
    state.tabline_needs_redraw = false;
    Ok(())
}

fn draw_status(state: &mut State, region: Rect) -> Result<()> {
    if let Some((_importance, message)) = state.pending_message.take() {
        write!(
            state.tty,
            "{}{}{}{} {} {}",
            region.start.goto(),
            clear::CurrentLine,
            color::Bg(color::Red),
            color::Fg(color::White),
            message,
            style::Reset,
        )?;
    } else {
        let mode = state.windows[state.open_tabs[state.focused_tab]].mode;
        let color: &dyn Color = match mode {
            Mode::Normal => &color::White,
            Mode::Insert => &color::LightYellow,
            Mode::Append => &color::White,
            Mode::Goto { .. } => &color::White,
            Mode::Command => &color::White,
        };
        write!(
            state.tty,
            "{}{}{}{} {:?} {}",
            region.start.goto(),
            clear::CurrentLine,
            style::Invert,
            color::Fg(color),
            mode,
            style::Reset,
        )?;
        #[allow(clippy::single_match)]
        match mode {
            Mode::Command => {
                write!(
                    state.tty,
                    " :{}{} {}",
                    state.windows[state.open_tabs[state.focused_tab]].command,
                    style::Invert,
                    style::Reset,
                )?;
            }
            _ => {}
        }
        state.statusline_needs_redraw = false;
    }
    Ok(())
}

fn draw_window(state: &mut State, window_id: WindowId, region: Rect) -> Result<()> {
    // TODO: draw a block where the next character will go in insert mode
    let window = &mut state.windows[window_id];
    {
        let first_visible_line = window.top;
        let last_visible_line = window.top + usize::from(region.height());
        let main_selection = window.selections[window.primary_selection];
        if main_selection.end.line < first_visible_line {
            window.top = main_selection.end.line;
        } else if main_selection.end.line > last_visible_line {
            window.top = main_selection.end.line - usize::from(region.height());
        }
    }
    let buffer = &state.buffers[window.buffer];
    let mut lines = buffer
        .content
        .lines_at(window.top.zero_based())
        .enumerate()
        .map(|(line, text)| (line + window.top.zero_based(), text));
    let mut range_y = region.range_y();
    'outer: while let Some(y) = range_y.next() {
        write!(state.tty, "{}{}", cursor::Goto(1, y), clear::CurrentLine)?;
        if let Some((line, text)) = lines.next() {
            let mut col = 0;
            for (file_col, mut c) in text.chars().enumerate() {
                if col == region.width() as usize + 1 {
                    write!(state.tty, "\r\n{}", clear::CurrentLine)?;
                    if range_y.next().is_none() {
                        break 'outer;
                    }
                    col = 0;
                }
                let pos = Position {
                    line: Line::from_zero_based(line),
                    column: Column::from_zero_based(file_col),
                };
                if c == '\n' {
                    c = ' ';
                }
                if window
                    .selections
                    .iter()
                    .map(|s| s.valid(&buffer.content))
                    .any(|s| s.contains(pos))
                {
                    if c == '\t' {
                        write!(state.tty, "{}    {}", style::Invert, style::Reset)?;
                        col += 4;
                    } else {
                        write!(state.tty, "{}{}{}", style::Invert, c, style::Reset)?;
                        col += 1;
                    }
                } else if c == '\t' {
                    write!(state.tty, "    ")?;
                    col += 4;
                } else {
                    write!(state.tty, "{}", c)?;
                    col += 1;
                }
            }
        }
    }
    Ok(())
}

pub fn show_message(state: &mut State, importance: Importance, message: String) {
    state.pending_message = Some((importance, message));
}

pub fn quit(state: &mut State) {
    state.exit_channels.0.send(()).unwrap();
}

pub fn undo(state: &mut State, window_id: WindowId) {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    match buffer.history.undo(&mut buffer.content) {
        Ok(()) => {
            let window_id = state.open_tabs[state.focused_tab];
            let window = &mut state.windows[window_id];
            let buffer = &mut state.buffers[window.buffer];
            for selection in window.selections.iter_mut() {
                selection.validate(&buffer.content);
            }
        }
        Err(NothingLeftToUndo) => {
            show_message(state, Importance::Error, "nothing left to undo".into());
        }
    }
}

pub fn redo(_state: &mut State, _window_id: WindowId) {
    todo!()
}

impl Drop for State {
    fn drop(&mut self) {
        _ = write!(
            self.tty,
            "{}{}{}",
            cursor::Show,
            cursor::SteadyBlock,
            screen::ToMainScreen
        );
    }
}

impl History {
    pub fn insert_char(&mut self, rope: &mut Rope, pos: Position, c: char) {
        rope.insert_char(pos.char_of(rope), c);
        self.push_back(Edit::Insert {
            pos,
            text: c.to_string(),
        });
    }

    pub fn remove_selection(&mut self, rope: &mut Rope, sel: Selection) {
        let text = sel.slice_of(rope).to_string();
        rope.remove(sel.range_of(rope));
        self.push_back(Edit::Delete {
            pos: sel.start,
            text,
        });
    }

    pub fn undo(&mut self, rope: &mut Rope) -> Result<(), NothingLeftToUndo> {
        let edit = self.edits.pop_back().ok_or(NothingLeftToUndo)?;
        trace!("undoing edit: {:?}", edit);
        match edit {
            Edit::Insert { pos, text } => {
                rope.remove(pos.char_of(rope)..pos.char_of(rope) + text.len());
                Ok(())
            }
            Edit::Delete { pos, text } => {
                rope.insert(pos.char_of(rope), &text);
                Ok(())
            }
        }
    }

    pub fn push_back(&mut self, edit: Edit) {
        trace!("pushing edit: {:?}", edit);
        self.edits.push_back(edit);
    }
}

const COMMANDS: &[CommandDesc] = &[
    CommandDesc {
        name: "quit",
        aliases: &["q"],
        description: "quit the editor",
        required_arguments: 0,
        run: |cx, _args| {
            quit(cx.editor);
            Ok(())
        },
    },
    CommandDesc {
        name: "open",
        aliases: &["e"],
        description: "open a file",
        required_arguments: 1,
        run: |cx, args| {
            let name = String::from(args[0]);
            let path = PathBuf::from(&name).canonicalize()?;
            let reader = File::open(&path)?;
            let buffer = Buffer {
                path: Some(path),
                name,
                content: Rope::from_reader(reader)?,
                history: History::default(),
            };
            let buffer_id = cx.editor.buffers.insert(buffer);
            let mut selections = TypedHandleMap::new();
            let selection_id = selections.insert(Selection {
                start: Position::file_start(),
                end: Position::file_start(),
            });
            let window = Window {
                buffer: buffer_id,
                command: String::new(),
                mode: Mode::Normal,
                selections,
                primary_selection: selection_id,
                top: Line::from_one_based(1),
            };
            let focused_tab = cx.editor.open_tabs.len();
            cx.editor.open_tabs.push(cx.editor.windows.insert(window));
            cx.editor.focused_tab = focused_tab;
            Ok(())
        },
    },
    CommandDesc {
        name: "write",
        aliases: &["w"],
        description: "write the current buffer contents to disk",
        required_arguments: 0,
        run: |cx, _args| {
            let buffer = &cx.editor.buffers[cx.editor.windows[cx.window].buffer];
            let path = buffer
                .path
                .as_ref()
                .context("cannot save a scratch buffer")?;
            let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
            buffer.content.write_to(&mut file)?;
            Ok(())
        },
    },
];
