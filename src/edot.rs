use crate::{
    id_vec::{Id, IdVec},
    location::{Column, Line, Movement, MovementError, Position, Selection},
    terminal::{Point, Rect},
    Result,
};
use anyhow::{format_err, Context as _};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use log::{error, info, trace};
use ropey::Rope;
use shlex::split as shlex;
use signal_hook::{iterator::Signals, SIGWINCH};
use std::{
    collections::VecDeque,
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

#[macro_export]
macro_rules! id {
    ($T:ident) => {
        #[derive(Debug, Copy, Clone, Eq, PartialEq)]
        pub struct $T(usize);

        impl Id for $T {
            fn id(self) -> usize {
                self.0
            }
        }
    };
}

pub struct Edot {
    signal: Receiver<c_int>,
    input: Receiver<io::Result<Event>>,
    exit: (Sender<()>, Receiver<()>),
    windows: IdVec<WindowId, Window>,
    buffers: IdVec<BufferId, Buffer>,
    output: RawTerminal<File>,
    focused: WindowId,
    tabline_dirty: bool,
    editor_dirty: bool,
    statusline_dirty: bool,
    last_screen_height: Option<u16>,
    message: Option<(Importance, String)>,
}

id!(WindowId);
id!(BufferId);

pub fn new() -> Result<Edot> {
    let (signals, signal) = unbounded();
    let (inputs, input) = unbounded();
    let signal_iter = Signals::new(&[SIGWINCH])?;
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
    Ok(Edot {
        signal,
        input,
        exit: unbounded(),
        windows: vec![Window {
            buffer: BufferId(0),
            mode: Mode::Normal,
            selections: vec![Selection {
                start: Position {
                    line: Line::from_one_based(1),
                    column: Column::from_one_based(1),
                },
                end: Position {
                    line: Line::from_one_based(1),
                    column: Column::from_one_based(1),
                },
            }]
            .into(),
            command: String::new(),
            top: Line::from_one_based(1),
        }]
        .into(),
        buffers: vec![Buffer {
            content: Rope::from("\n"),
            name: String::from("scratch"),
            history: History::new(),
            path: None,
        }]
        .into(),
        output: get_tty()?.into_raw_mode()?,
        focused: WindowId(0),
        tabline_dirty: true,
        editor_dirty: true,
        statusline_dirty: true,
        last_screen_height: None,
        message: None,
    })
}

#[allow(unreachable_code)]
pub fn run(mut state: Edot) -> Result {
    fn handle_next_event(state: &mut Edot) -> Result<bool> {
        select! {
            recv(state.input) -> input => handle_event(state, input??)?,
            recv(state.signal) -> signal => handle_signal(state, signal?)?,
            recv(state.exit.1) -> exit => { exit?; return Ok(false); },
        }
        Ok(true)
    }

    write!(
        state.output,
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

fn run_command(state: &mut Edot, args: &[&str]) -> Result {
    let name = args.get(0).copied().context("no command given")?;
    let cmd = COMMANDS
        .iter()
        .find(|desc| desc.name == name || desc.aliases.contains(&name))
        .ok_or_else(|| format_err!("command '{}' doesn't exist", name))?;
    (cmd.run)(
        Context {
            window: state.focused,
            editor: state,
        },
        &args[1..],
    )?;
    Ok(())
}
fn handle_event(state: &mut Edot, event: Event) -> Result {
    trace!("event: {:?}", event);

    const SHIFT_UP: &[u8] = &[27, 91, 49, 59, 50, 65];
    const SHIFT_DOWN: &[u8] = &[27, 91, 49, 59, 50, 66];
    const SHIFT_RIGHT: &[u8] = &[27, 91, 49, 59, 50, 67];
    const SHIFT_LEFT: &[u8] = &[27, 91, 49, 59, 50, 68];

    'arrows: {
        if let Mode::Normal | Mode::Insert | Mode::Append = state.windows[state.focused].mode {
            match event {
                Event::Key(Key::Left) => {
                    move_selections(state, state.focused, Movement::Left(1), false)?;
                }
                Event::Key(Key::Down) => {
                    move_selections(state, state.focused, Movement::Down(1), false)?;
                }
                Event::Key(Key::Up) => {
                    move_selections(state, state.focused, Movement::Up(1), false)?;
                }
                Event::Key(Key::Right) => {
                    move_selections(state, state.focused, Movement::Right(1), false)?;
                }
                Event::Key(Key::Ctrl('u')) => {
                    if let Some(height) = state.last_screen_height {
                        move_selection(
                            state,
                            state.focused,
                            SelectionId::PRIMARY,
                            Movement::Up(usize::from(height / 2)),
                            false,
                        )?;
                    }
                }
                Event::Key(Key::Ctrl('d')) => {
                    if let Some(height) = state.last_screen_height {
                        move_selection(
                            state,
                            state.focused,
                            SelectionId::PRIMARY,
                            Movement::Down(usize::from(height / 2)),
                            false,
                        )?;
                    }
                }
                Event::Key(Key::Ctrl('b') | Key::PageUp) => {
                    if let Some(height) = state.last_screen_height {
                        move_selection(
                            state,
                            state.focused,
                            SelectionId::PRIMARY,
                            Movement::Up(usize::from(height)),
                            false,
                        )?;
                    }
                }
                Event::Key(Key::Ctrl('f') | Key::PageDown) => {
                    if let Some(height) = state.last_screen_height {
                        move_selection(
                            state,
                            state.focused,
                            SelectionId::PRIMARY,
                            Movement::Down(usize::from(height)),
                            false,
                        )?;
                    }
                }
                Event::Key(Key::Ctrl('p')) => {
                    state.focused = WindowId((state.focused.0 - 1) % state.windows.len());
                }
                Event::Key(Key::Ctrl('n')) => {
                    state.focused = WindowId((state.focused.0 + 1) % state.windows.len());
                }
                Event::Unsupported(keys) => match keys.as_slice() {
                    SHIFT_LEFT => {
                        move_selections(state, state.focused, Movement::Left(1), true)?;
                    }
                    SHIFT_DOWN => {
                        move_selections(state, state.focused, Movement::Down(1), true)?;
                    }
                    SHIFT_UP => {
                        move_selections(state, state.focused, Movement::Up(1), true)?;
                    }
                    SHIFT_RIGHT => {
                        move_selections(state, state.focused, Movement::Right(1), true)?;
                    }
                    _ => {}
                },
                _ => break 'arrows,
            }
            return Ok(());
        }
    }

    match state.windows[state.focused].mode {
        Mode::Normal => match event {
            Event::Key(Key::Char('i')) => {
                order_selections(state, state.focused);
                set_mode(state, state.focused, Mode::Insert);
            }
            Event::Key(Key::Char('c')) => {
                delete_selections(state, state.focused);
                set_mode(state, state.focused, Mode::Insert);
            }
            Event::Key(Key::Char('a')) => {
                order_selections(state, state.focused);
                set_mode(state, state.focused, Mode::Append);
            }
            Event::Key(Key::Char('A')) => {
                move_selections(state, state.focused, Movement::LineEnd, false)?;
                set_mode(state, state.focused, Mode::Insert);
            }
            Event::Key(Key::Char('o')) => {
                for selection_id in selections(state, state.focused) {
                    move_selection(state, state.focused, selection_id, Movement::LineEnd, false)?;
                    insert_char_after(state, state.focused, selection_id, '\n');
                    move_selection(state, state.focused, selection_id, Movement::Down(1), false)?;
                    move_selection(
                        state,
                        state.focused,
                        selection_id,
                        Movement::LineStart,
                        false,
                    )?;
                }
                set_mode(state, state.focused, Mode::Insert);
            }
            Event::Key(Key::Char('x')) => {
                // self.move_selections(self.focused, Movement::Line, false)?;
            }
            Event::Key(Key::Char('X')) => {
                // self.move_selections(self.focused, Movement::Line, true)?;
            }
            Event::Key(Key::Char('g')) => {
                set_mode(state, state.focused, Mode::Goto { drag: false });
            }
            Event::Key(Key::Char('G')) => {
                set_mode(state, state.focused, Mode::Goto { drag: true });
            }
            Event::Key(Key::Char(':')) => {
                set_mode(state, state.focused, Mode::Command);
            }
            Event::Key(Key::Char('h')) => {
                move_selections(state, state.focused, Movement::Left(1), false)?;
            }
            Event::Key(Key::Char('j')) => {
                move_selections(state, state.focused, Movement::Down(1), false)?;
            }
            Event::Key(Key::Char('k')) => {
                move_selections(state, state.focused, Movement::Up(1), false)?;
            }
            Event::Key(Key::Char('l')) => {
                move_selections(state, state.focused, Movement::Right(1), false)?;
            }
            Event::Key(Key::Char('H')) => {
                move_selections(state, state.focused, Movement::Left(1), true)?;
            }
            Event::Key(Key::Char('J')) => {
                move_selections(state, state.focused, Movement::Down(1), true)?;
            }
            Event::Key(Key::Char('K')) => {
                move_selections(state, state.focused, Movement::Up(1), true)?;
            }
            Event::Key(Key::Char('L')) => {
                move_selections(state, state.focused, Movement::Right(1), true)?;
            }
            Event::Key(Key::Char('d')) => {
                delete_selections(state, state.focused);
            }
            Event::Key(Key::Char('u')) => {
                undo(state, state.focused);
            }
            _ => {}
        },
        Mode::Goto { drag } => {
            match event {
                Event::Key(Key::Char('h')) => {
                    move_selections(state, state.focused, Movement::LineStart, drag)?;
                }
                Event::Key(Key::Char('j')) => {
                    move_selections(state, state.focused, Movement::FileEnd, drag)?;
                }
                Event::Key(Key::Char('k')) => {
                    move_selections(state, state.focused, Movement::FileStart, drag)?;
                }
                Event::Key(Key::Char('l')) => {
                    move_selections(state, state.focused, Movement::LineEnd, drag)?;
                }
                _ => {}
            };
            set_mode(state, state.focused, Mode::Normal);
        }
        mode @ Mode::Insert | mode @ Mode::Append => match event {
            Event::Key(Key::Esc) => set_mode(state, state.focused, Mode::Normal),
            Event::Key(Key::Char(c)) => {
                for selection_id in selections(state, state.focused) {
                    match mode {
                        Mode::Insert => {
                            insert_char_before(state, state.focused, selection_id, c);
                            shift_selection(
                                state,
                                state.focused,
                                selection_id,
                                Movement::Right(1),
                            )?;
                        }
                        Mode::Append => {
                            move_selection(
                                state,
                                state.focused,
                                selection_id,
                                Movement::Right(1),
                                true,
                            )?;
                            insert_char_after(state, state.focused, selection_id, c);
                        }
                        _ => unreachable!(),
                    }
                }
            }
            Event::Key(Key::Backspace) => {
                move_selections(state, state.focused, Movement::Left(1), false)?;
                delete_selections(state, state.focused);
            }
            _ => {}
        },
        Mode::Command => match event {
            Event::Key(Key::Esc) => {
                state.windows[state.focused].command.clear();
                set_mode(state, state.focused, Mode::Normal);
            }
            Event::Key(Key::Char('\t')) => {}
            Event::Key(Key::Char('\n')) => {
                let command = take(&mut state.windows[state.focused].command);
                set_mode(state, state.focused, Mode::Normal);
                let command = shlex(&command)
                    .ok_or_else(|| format_err!("failed to parse command '{}'", command))?;
                trace!("command: {:?}", command);
                let command = command.iter().map(|x| &**x).collect::<Vec<&str>>();
                run_command(state, &command)?;
            }
            Event::Key(Key::Char(c)) => {
                state.windows[state.focused].command.push(c);
            }
            Event::Key(Key::Backspace) => {
                if state.windows[state.focused].command.pop().is_none() {
                    set_mode(state, state.focused, Mode::Normal);
                } else {
                }
            }
            _ => {}
        },
    }
    Ok(())
}

fn handle_signal(state: &mut Edot, signal: c_int) -> Result {
    info!("received signal: {}", signal);
    match signal {
        signal_hook::SIGWINCH => draw(state)?,
        _ => {}
    }
    Ok(())
}

fn draw(state: &mut Edot) -> Result {
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
    draw_window(state, state.focused, region)?;
    state.last_screen_height = Some(region.height());

    let region = Rect {
        start: Point { x: 1, y: height },
        end: Point {
            x: width,
            y: height,
        },
    };
    draw_status(state, region)?;

    state.output.flush()?;
    Ok(())
}

fn draw_tabs(state: &mut Edot, region: Rect) -> Result {
    write!(
        state.output,
        "{}{}",
        region.start.goto(),
        clear::CurrentLine
    )?;
    for window_id in (0..state.windows.len()).map(WindowId) {
        let window = &state.windows[window_id];
        let buffer = &state.buffers[window.buffer];
        if window_id == state.focused {
            write!(
                state.output,
                "{}{}{} ",
                style::Bold,
                buffer.name,
                style::Reset,
            )?;
        } else {
            write!(state.output, "{} ", buffer.name)?;
        }
    }
    state.tabline_dirty = false;
    Ok(())
}

fn draw_status(state: &mut Edot, region: Rect) -> Result {
    if let Some((_importance, message)) = state.message.take() {
        write!(
            state.output,
            "{}{}{}{} {} {}",
            region.start.goto(),
            clear::CurrentLine,
            color::Bg(color::Red),
            color::Fg(color::White),
            message,
            style::Reset,
        )?;
    } else {
        let mode = state.windows[state.focused].mode;
        let color: &dyn Color = match mode {
            Mode::Normal => &color::White,
            Mode::Insert => &color::LightYellow,
            Mode::Append => &color::White,
            Mode::Goto { .. } => &color::White,
            Mode::Command => &color::White,
        };
        write!(
            state.output,
            "{}{}{}{} {:?} {}",
            region.start.goto(),
            clear::CurrentLine,
            style::Invert,
            color::Fg(color),
            mode,
            style::Reset,
        )?;
        match mode {
            Mode::Command => {
                write!(
                    state.output,
                    " :{}{} {}",
                    state.windows[state.focused].command,
                    style::Invert,
                    style::Reset,
                )?;
            }
            _ => {}
        }
        state.statusline_dirty = false;
    }
    Ok(())
}

fn draw_window(state: &mut Edot, window_id: WindowId, region: Rect) -> Result {
    // TODO: draw a block where the next character will go in insert mode
    let window = &mut state.windows[window_id];
    {
        let first_visible_line = window.top;
        let last_visible_line = window.top + usize::from(region.height());
        let main_selection = window.selections[SelectionId::PRIMARY];
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
        write!(state.output, "{}{}", cursor::Goto(1, y), clear::CurrentLine)?;
        if let Some((line, text)) = lines.next() {
            let mut chars = text.chars().enumerate();
            let mut col = 0;
            while let Some((file_col, mut c)) = chars.next() {
                if col == region.width() as usize + 1 {
                    write!(state.output, "\r\n{}", clear::CurrentLine)?;
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
                        write!(state.output, "{}    {}", style::Invert, style::Reset)?;
                        col += 4;
                    } else {
                        write!(state.output, "{}{}{}", style::Invert, c, style::Reset)?;
                        col += 1;
                    }
                } else {
                    if c == '\t' {
                        write!(state.output, "    ")?;
                        col += 4;
                    } else {
                        write!(state.output, "{}", c)?;
                        col += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn show_message(state: &mut Edot, importance: Importance, message: String) {
    state.message = Some((importance, message));
}

pub fn quit(state: &mut Edot) {
    state.exit.0.send(()).unwrap();
}

pub fn set_mode(state: &mut Edot, window: WindowId, mode: Mode) {
    state.windows[window].mode = mode;
    match mode {
        Mode::Normal => {}
        Mode::Insert => {}
        Mode::Append => {}
        Mode::Goto { .. } => {}
        Mode::Command => {}
    }
}

pub fn selections(state: &Edot, window: WindowId) -> impl Iterator<Item = SelectionId> {
    let window = &state.windows[window];
    (0..window.selections.len()).map(SelectionId)
}

pub fn insert_char_before(
    state: &mut Edot,
    window_id: WindowId,
    selection_id: SelectionId,
    c: char,
) {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    let selection = &mut window.selections[selection_id];
    selection.start.insert_char(buffer, c);
}

pub fn insert_char_after(
    state: &mut Edot,
    window_id: WindowId,
    selection_id: SelectionId,
    c: char,
) {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    let selection = &mut window.selections[selection_id];
    selection.end.insert_char(buffer, c);
}

pub fn move_selection(
    state: &mut Edot,
    window_id: WindowId,
    selection_id: SelectionId,
    movement: Movement,
    drag: bool,
) -> Result<(), MovementError> {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    let selection = &mut window.selections[selection_id];
    let result = selection.end.move_to(&buffer.content, movement);
    if !drag {
        selection.start = selection.end;
    }
    result
}

pub fn move_selections(
    state: &mut Edot,
    window_id: WindowId,
    movement: Movement,
    drag: bool,
) -> Result<(), MovementError> {
    for selection_id in selections(state, window_id) {
        move_selection(state, window_id, selection_id, movement, drag)?;
    }
    Ok(())
}

pub fn shift_selection(
    state: &mut Edot,
    window_id: WindowId,
    selection_id: SelectionId,
    movement: Movement,
) -> Result<(), MovementError> {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    let selection = &mut window.selections[selection_id];
    selection.start.move_to(&buffer.content, movement)?;
    selection.end.move_to(&buffer.content, movement)?;
    Ok(())
}

pub fn shift_selections(
    state: &mut Edot,
    window_id: WindowId,
    movement: Movement,
) -> Result<(), MovementError> {
    for selection_id in selections(state, window_id) {
        shift_selection(state, window_id, selection_id, movement)?;
    }
    Ok(())
}

pub fn delete_selection(state: &mut Edot, window_id: WindowId, selection_id: SelectionId) {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    let selection = &mut window.selections[selection_id];
    selection.remove_from(buffer);
}

pub fn delete_selections(state: &mut Edot, window_id: WindowId) {
    for selection_id in selections(state, window_id) {
        delete_selection(state, window_id, selection_id);
    }
}

pub fn flip_selection(state: &mut Edot, window_id: WindowId, selection_id: SelectionId) {
    let window = &mut state.windows[window_id];
    let selection = &mut window.selections[selection_id];
    selection.flip();
}

pub fn order_selections(state: &mut Edot, window_id: WindowId) {
    for selection_id in selections(state, window_id) {
        order_selection(state, window_id, selection_id);
    }
}

pub fn order_selection(state: &mut Edot, window_id: WindowId, selection_id: SelectionId) {
    let window = &mut state.windows[window_id];
    let selection = &mut window.selections[selection_id];
    selection.order();
}

pub fn flip_selections(state: &mut Edot, window_id: WindowId) {
    for selection_id in selections(state, window_id) {
        flip_selection(state, window_id, selection_id);
    }
}

pub fn for_each_selection<F>(state: &mut Edot, window_id: WindowId, mut f: F) -> Result
where
    F: FnMut(&mut Edot, WindowId, SelectionId) -> Result,
{
    let mut errors = Vec::new();
    for selection_id in selections(state, window_id) {
        if let Err(e) = f(state, window_id, selection_id) {
            errors.push(e);
        }
    }
    errors.pop().map_or(Ok(()), Err)
}

pub fn undo(state: &mut Edot, window_id: WindowId) {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    match buffer.history.undo(&mut buffer.content) {
        Ok(()) => {
            for_each_selection(state, window_id, |this, window, selection| {
                let window = &mut this.windows[window];
                let selection = &mut window.selections[selection];
                let buffer = &mut this.buffers[window.buffer];
                selection.validate(&buffer.content);
                Ok(())
            })
            .unwrap();
        }
        Err(NothingLeftToUndo) => {
            show_message(state, Importance::Error, "nothing left to undo".into());
        }
    }
}

impl Drop for Edot {
    fn drop(&mut self) {
        _ = write!(
            self.output,
            "{}{}{}",
            cursor::Show,
            cursor::SteadyBlock,
            screen::ToMainScreen
        );
    }
}

pub struct Window {
    buffer: BufferId,
    mode: Mode,
    selections: IdVec<SelectionId, Selection>,
    command: String,
    top: Line,
}

id!(SelectionId);

impl SelectionId {
    const PRIMARY: Self = Self(0);
}

pub struct Buffer {
    pub path: Option<PathBuf>,
    pub name: String,
    pub content: Rope,
    pub history: History,
}

pub struct NothingLeftToUndo;

pub struct History {
    edits: VecDeque<Edit>,
}

impl History {
    pub fn new() -> Self {
        Self {
            edits: VecDeque::new(),
        }
    }

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
    Goto { drag: bool },
    Command,
}

#[derive(Debug, Copy, Clone)]
pub enum Importance {
    Error,
}

pub struct Context<'a> {
    editor: &'a mut Edot,
    window: WindowId,
}

pub struct CommandDesc {
    name: &'static str,
    aliases: &'static [&'static str],
    description: &'static str,
    required_arguments: usize,
    run: fn(cx: Context, args: &[&str]) -> Result,
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
                history: History::new(),
            };
            let buffer_id = BufferId(cx.editor.buffers.len());
            cx.editor.buffers.push(buffer);
            let window = Window {
                buffer: buffer_id,
                command: String::new(),
                mode: Mode::Normal,
                selections: vec![Selection {
                    // TODO move this out
                    start: Position {
                        line: Line::from_one_based(1),
                        column: Column::from_one_based(1),
                    },
                    end: Position {
                        line: Line::from_one_based(1),
                        column: Column::from_one_based(1),
                    },
                }]
                .into(),
                top: Line::from_one_based(1),
            };
            let window_id = WindowId(cx.editor.windows.len());
            cx.editor.windows.push(window);
            cx.editor.focused = window_id;
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
