use crate::{
    location::{Column, Line, Movement, MovementError, Position, Selection},
    terminal::{Point, Rect},
    Result,
};
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

type WindowId = TypedHandle<Window>;
type BufferId = TypedHandle<Buffer>;

pub fn new() -> Result<State> {
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
        history: History::new(),
        path: None,
    });
    let mut selections = TypedHandleMap::new();
    let primary_selection = selections.insert(Selection {
        start: Position {
            line: Line::from_one_based(1),
            column: Column::from_one_based(1),
        },
        end: Position {
            line: Line::from_one_based(1),
            column: Column::from_one_based(1),
        },
    });
    let focused_window = windows.insert(Window {
        buffer: scratch_buffer,
        mode: Mode::Normal,
        selections,
        primary_selection,
        command: String::new(),
        top: Line::from_one_based(1),
    });
    Ok(State {
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
    })
}

pub fn run(mut state: State) -> Result<()> {
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
    )?;
    Ok(())
}

fn handle_event(state: &mut State, event: Event) -> Result<()> {
    trace!("event: {:?}", event);

    const SHIFT_UP: &[u8] = &[27, 91, 49, 59, 50, 65];
    const SHIFT_DOWN: &[u8] = &[27, 91, 49, 59, 50, 66];
    const SHIFT_RIGHT: &[u8] = &[27, 91, 49, 59, 50, 67];
    const SHIFT_LEFT: &[u8] = &[27, 91, 49, 59, 50, 68];

    'arrows: {
        if let Mode::Normal | Mode::Insert | Mode::Append =
            state.windows[state.open_tabs[state.focused_tab]].mode
        {
            match event {
                Event::Key(Key::Left) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::Left(1), false)
                    })
                }?,
                Event::Key(Key::Down) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::Down(1), false)
                    })
                }?,
                Event::Key(Key::Up) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::Up(1), false)
                    })
                }?,
                Event::Key(Key::Right) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::Right(1), false)
                    })
                }?,
                Event::Key(Key::Ctrl('u')) => {
                    if let Some(height) = state.last_screen_height {
                        with_primary_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(
                                &buffer.content,
                                Movement::Up(usize::from(height / 2)),
                                false,
                            )
                        })?;
                    }
                }
                Event::Key(Key::Ctrl('d')) => {
                    if let Some(height) = state.last_screen_height {
                        with_primary_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(
                                &buffer.content,
                                Movement::Down(usize::from(height / 2)),
                                false,
                            )
                        })?;
                    }
                }
                Event::Key(Key::Ctrl('b') | Key::PageUp) => {
                    if let Some(height) = state.last_screen_height {
                        with_primary_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(
                                &buffer.content,
                                Movement::Up(usize::from(height)),
                                false,
                            )
                        })?;
                    }
                }
                Event::Key(Key::Ctrl('f') | Key::PageDown) => {
                    if let Some(height) = state.last_screen_height {
                        with_primary_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(
                                &buffer.content,
                                Movement::Down(usize::from(height)),
                                false,
                            )
                        })?;
                    }
                }
                Event::Key(Key::Ctrl('p')) => {
                    state.focused_tab = (state.focused_tab - 1) % state.open_tabs.len();
                }
                Event::Key(Key::Ctrl('n')) => {
                    state.focused_tab = (state.focused_tab + 1) % state.open_tabs.len();
                }
                Event::Unsupported(keys) => match keys.as_slice() {
                    SHIFT_LEFT => {
                        try_for_each_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(&buffer.content, Movement::Left(1), true)
                        })?
                    }
                    SHIFT_DOWN => {
                        try_for_each_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(&buffer.content, Movement::Down(1), true)
                        })?
                    }
                    SHIFT_UP => {
                        try_for_each_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(&buffer.content, Movement::Up(1), true)
                        })?
                    }
                    SHIFT_RIGHT => {
                        try_for_each_selection_in_focused_window(state, |buffer, selection| {
                            selection.move_to(&buffer.content, Movement::Right(1), true)
                        })?
                    }
                    _ => {}
                },
                _ => break 'arrows,
            }
            return Ok(());
        }
    }

    match state.windows[state.open_tabs[state.focused_tab]].mode {
        Mode::Normal => match event {
            Event::Key(Key::Char('i')) => {
                for_each_selection_in_focused_window(state, |_buffer, selection| {
                    selection.order();
                });
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Insert;
            }
            Event::Key(Key::Char('c')) => {
                for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.remove_from(buffer);
                });
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Insert;
            }
            Event::Key(Key::Char('a')) => {
                for_each_selection_in_focused_window(state, |_buffer, selection| {
                    selection.order();
                });
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Append;
            }
            Event::Key(Key::Char('A')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::LineEnd, false)
                })?;
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Insert;
            }
            Event::Key(Key::Char('o')) => {
                try_for_each_selection_in_focused_window::<_, MovementError>(
                    state,
                    |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::LineEnd, false)?;
                        selection.end.insert_char(buffer, '\n');
                        selection.move_to(&buffer.content, Movement::Down(1), false)?;
                        selection.move_to(&buffer.content, Movement::LineStart, false)?;
                        Ok(())
                    },
                )?;
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Insert;
            }
            Event::Key(Key::Char('x')) => {
                //self.move_selections(self.focused, Movement::Line, false)?;
            }
            Event::Key(Key::Char('X')) => {
                //self.move_selections(self.focused, Movement::Line, true)?;
            }
            Event::Key(Key::Char('g')) => {
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Goto { drag: false };
            }
            Event::Key(Key::Char('G')) => {
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Goto { drag: true };
            }
            Event::Key(Key::Char(':')) => {
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Command;
            }
            Event::Key(Key::Char('h')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Left(1), false)
                })?
            }
            Event::Key(Key::Char('j')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Down(1), false)
                })?
            }
            Event::Key(Key::Char('k')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Up(1), false)
                })?
            }
            Event::Key(Key::Char('l')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Right(1), false)
                })?
            }
            Event::Key(Key::Char('H')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Left(1), true)
                })?
            }
            Event::Key(Key::Char('J')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Down(1), true)
                })?
            }
            Event::Key(Key::Char('K')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Up(1), true)
                })?
            }
            Event::Key(Key::Char('L')) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Right(1), true)
                })?
            }
            Event::Key(Key::Char('d')) => {
                for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.remove_from(buffer);
                });
            }
            Event::Key(Key::Char('u')) => {
                undo(state, state.open_tabs[state.focused_tab]);
            }
            _ => {}
        },
        Mode::Goto { drag } => {
            match event {
                Event::Key(Key::Char('h')) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::LineStart, drag)
                    })?;
                }
                Event::Key(Key::Char('j')) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::FileEnd, drag)
                    })?;
                }
                Event::Key(Key::Char('k')) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::FileStart, drag)
                    })?;
                }
                Event::Key(Key::Char('l')) => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.move_to(&buffer.content, Movement::LineEnd, drag)
                    })?;
                }
                _ => {}
            };
            {
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Normal;
            };
        }
        mode @ Mode::Insert | mode @ Mode::Append => match event {
            Event::Key(Key::Esc) => {
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Normal;
            }
            Event::Key(Key::Char(c)) => match mode {
                Mode::Insert => {
                    try_for_each_selection_in_focused_window(state, |buffer, selection| {
                        selection.start.insert_char(buffer, c);
                        selection
                            .start
                            .move_to(&buffer.content, Movement::Right(1))?;
                        selection.end.move_to(&buffer.content, Movement::Right(1))
                    })?;
                }
                Mode::Append => {
                    try_for_each_selection_in_focused_window::<_, MovementError>(
                        state,
                        |buffer, selection| {
                            selection
                                .start
                                .move_to(&buffer.content, Movement::Right(1))?;
                            selection.end.move_to(&buffer.content, Movement::Right(1))?;
                            selection.end.insert_char(buffer, c);
                            Ok(())
                        },
                    )?;
                }
                _ => unreachable!(),
            },
            Event::Key(Key::Backspace) => {
                try_for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.move_to(&buffer.content, Movement::Left(1), false)
                })?;
                for_each_selection_in_focused_window(state, |buffer, selection| {
                    selection.remove_from(buffer);
                });
            }
            _ => {}
        },
        Mode::Command => match event {
            Event::Key(Key::Esc) => {
                state.windows[state.open_tabs[state.focused_tab]]
                    .command
                    .clear();
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Normal;
            }
            Event::Key(Key::Char('\t')) => {}
            Event::Key(Key::Char('\n')) => {
                let command = take(&mut state.windows[state.open_tabs[state.focused_tab]].command);
                state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Normal;
                let command = shlex(&command)
                    .ok_or_else(|| format_err!("failed to parse command '{}'", command))?;
                trace!("command: {:?}", command);
                let command = command.iter().map(|x| &**x).collect::<Vec<&str>>();
                run_command(state, &command)?;
            }
            Event::Key(Key::Char(c)) => {
                state.windows[state.open_tabs[state.focused_tab]]
                    .command
                    .push(c);
            }
            Event::Key(Key::Backspace) => {
                if state.windows[state.open_tabs[state.focused_tab]]
                    .command
                    .pop()
                    .is_none()
                {
                    let mode: Mode = Mode::Normal;
                    state.windows[state.open_tabs[state.focused_tab]].mode = mode;
                }
            }
            _ => {}
        },
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

fn for_each_selection_in_focused_window<F>(state: &mut State, mut f: F)
where
    F: FnMut(&mut Buffer, &mut Selection),
{
    let window_id = state.open_tabs[state.focused_tab];
    for_each_selection_in_window(state, window_id, |buf, sel| f(buf, sel));
}

fn for_each_selection_in_window<F>(state: &mut State, window_id: WindowId, mut f: F)
where
    F: FnMut(&mut Buffer, &mut Selection),
{
    try_for_each_selection_in_window::<_, Infallible>(state, window_id, |buf, sel| {
        f(buf, sel);
        Ok(())
    })
    .unwrap()
}

fn with_primary_selection_in_focused_window<F, R>(state: &mut State, f: F) -> R
where
    F: FnOnce(&mut Buffer, &mut Selection) -> R,
{
    let window_id = state.open_tabs[state.focused_tab];
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    let selection = &mut window.selections[window.primary_selection];
    f(buffer, selection)
}

fn try_for_each_selection_in_focused_window<F, E>(state: &mut State, f: F) -> Result<(), E>
where
    F: FnMut(&mut Buffer, &mut Selection) -> Result<(), E>,
{
    let window_id = state.open_tabs[state.focused_tab];
    try_for_each_selection_in_window(state, window_id, f)
}

fn try_for_each_selection_in_window<F, E>(
    state: &mut State,
    window_id: WindowId,
    mut f: F,
) -> Result<(), E>
where
    F: FnMut(&mut Buffer, &mut Selection) -> Result<(), E>,
{
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    for selection in window.selections.iter_mut() {
        f(buffer, selection)?;
    }
    Ok(())
}

pub fn undo(state: &mut State, window_id: WindowId) {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    match buffer.history.undo(&mut buffer.content) {
        Ok(()) => for_each_selection_in_focused_window(state, |buffer, selection| {
            selection.validate(&buffer.content);
        }),
        Err(NothingLeftToUndo) => {
            show_message(state, Importance::Error, "nothing left to undo".into());
        }
    }
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

pub struct Window {
    buffer: BufferId,
    mode: Mode,
    selections: TypedHandleMap<Selection>,
    primary_selection: SelectionId,
    command: String,
    top: Line,
}

type SelectionId = TypedHandle<Selection>;

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
            let buffer_id = cx.editor.buffers.insert(buffer);
            let mut selections = TypedHandleMap::new();
            let selection_id = selections.insert(Selection {
                // TODO move this out
                start: Position {
                    line: Line::from_one_based(1),
                    column: Column::from_one_based(1),
                },
                end: Position {
                    line: Line::from_one_based(1),
                    column: Column::from_one_based(1),
                },
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
