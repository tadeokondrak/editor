mod terminal;

use anyhow::Result;
use crossbeam_channel::{select, unbounded, Receiver};
use editor::location::{ColumnIndex, LineIndex, Movement, Position};
use editor::{
    perform_action, show_message, Action, BufferAction, CommandAction, EditorAction, EditorData,
    Importance, Mode, WindowAction, WindowId,
};
use log::{error, info, trace};
use signal_hook::{iterator::Signals, SIGWINCH};
use std::{
    io::{self, Write as _},
    os::raw::c_int,
    thread,
};
use terminal::{Point, Rect};
use termion::{
    clear,
    color::{self, Color},
    cursor, screen, style, terminal_size,
};
use textmode::blocking::{Input, RawGuard};
use textmode::Key;
use textmode::{blocking::Output, Textmode};

pub struct Tty(Output);

impl Tty {
    fn new() -> Result<Tty, textmode::Error> {
        Output::new().map(Tty)
    }
}

impl io::Write for Tty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .refresh()
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    }
}

pub struct State {
    pub editor: EditorData,
    pub signals: Receiver<c_int>,
    pub inputs: Receiver<Result<textmode::Key, textmode::Error>>,
    pub tty: Tty,
    pub tabline_needs_redraw: bool,
    pub statusline_needs_redraw: bool,
    pub _raw_guard: RawGuard,
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

fn main() -> Result<()> {
    env_logger::init();
    let mut state = {
        let (signal_s, signal_r) = unbounded();
        let (input_s, input_r) = unbounded();
        let signal_iter = Signals::new([SIGWINCH])?;
        thread::spawn(move || {
            for signal in signal_iter.forever() {
                signal_s.send(signal).unwrap();
            }
        });
        let mut input = Input::new()?;
        let raw_guard = input.take_raw_guard().unwrap();
        thread::spawn(move || {
            while let Some(result) = input.read_key().transpose() {
                input_s.send(result).unwrap();
            }
        });
        State {
            editor: EditorData::new(),
            signals: signal_r,
            inputs: input_r,
            tty: Tty::new()?,
            tabline_needs_redraw: true,
            statusline_needs_redraw: true,
            _raw_guard: raw_guard,
        }
    };
    fn handle_next_event(state: &mut State) -> Result<()> {
        select! {
            recv(state.inputs) -> input => handle_event(state, input??),
            recv(state.signals) -> signal => handle_signal(state, signal?),
        }
    }

    write!(
        state.tty,
        "{}{}{}",
        screen::ToAlternateScreen,
        cursor::Hide,
        cursor::SteadyBar
    )?;
    while !state.editor.want_quit {
        draw(&mut state)?;
        if let Err(e) = handle_next_event(&mut state) {
            error!("{e}");
            show_message(&mut state.editor, Importance::Error, e.to_string());
        }
    }
    Ok(())
}

fn handle_event(state: &mut State, key: textmode::Key) -> Result<()> {
    trace!("event: {:?}", key);

    const SHIFT_UP: &[u8] = &[27, 91, 49, 59, 50, 65];
    const SHIFT_DOWN: &[u8] = &[27, 91, 49, 59, 50, 66];
    const SHIFT_RIGHT: &[u8] = &[27, 91, 49, 59, 50, 67];
    const SHIFT_LEFT: &[u8] = &[27, 91, 49, 59, 50, 68];

    let mut actions = Vec::new();

    if let Mode::Normal | Mode::Insert | Mode::Append =
        state.editor.windows[state.editor.open_tabs[state.editor.focused_tab]].mode
    {
        match &key {
            Key::Left => actions.push(Action::Window(WindowAction::Move(Movement::Left(1)))),
            Key::Down => actions.push(Action::Window(WindowAction::Move(Movement::Down(1)))),
            Key::Up => actions.push(Action::Window(WindowAction::Move(Movement::Up(1)))),
            Key::Right => actions.push(Action::Window(WindowAction::Move(Movement::Right(1)))),
            Key::Ctrl(b'u') => actions.push(Action::Window(WindowAction::ScrollHalfPageUp)),
            Key::Ctrl(b'd') => actions.push(Action::Window(WindowAction::ScrollHalfPageDown)),
            Key::Home => actions.push(Action::Window(WindowAction::Move(Movement::LineStart))),
            Key::End => actions.push(Action::Window(WindowAction::Move(Movement::LineEnd))),
            Key::Ctrl(b'b') | Key::PageUp => {
                actions.push(Action::Window(WindowAction::ScrollPageUp));
            }
            Key::Ctrl(b'f') | Key::PageDown => {
                actions.push(Action::Window(WindowAction::ScrollPageDown));
            }
            Key::Ctrl(b'p') => actions.push(Action::Editor(EditorAction::PreviousTab)),
            Key::Ctrl(b'n') => actions.push(Action::Editor(EditorAction::NextTab)),
            Key::Bytes(keys) => match keys.as_slice() {
                SHIFT_LEFT => {
                    actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Left(1))))
                }
                SHIFT_DOWN => {
                    actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Down(1))))
                }
                SHIFT_UP => actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Up(1)))),
                SHIFT_RIGHT => {
                    actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Right(1))))
                }
                _ => {}
            },
            _ => {}
        }
    }

    match state.editor.windows[state.editor.open_tabs[state.editor.focused_tab]].mode {
        Mode::Normal => match key {
            Key::Char('i') => {
                actions.push(Action::Window(WindowAction::OrderSelections));
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Insert)));
            }
            Key::Char('c') => {
                actions.push(Action::Window(WindowAction::Delete));
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Insert)));
            }
            Key::Char('a') => {
                actions.push(Action::Window(WindowAction::OrderSelections));
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Append)));
            }
            Key::Char('A') => {
                actions.push(Action::Window(WindowAction::Move(Movement::LineEnd)));
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Insert)));
            }
            Key::Char('o') => {
                actions.push(Action::Window(WindowAction::Move(Movement::LineEnd)));
                actions.push(Action::Window(WindowAction::InsertAtSelectionEnd('\n')));
                actions.push(Action::Window(WindowAction::Move(Movement::Down(1))));
                actions.push(Action::Window(WindowAction::Move(Movement::LineStart)));
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Insert)));
            }
            Key::Char('x') => {
                //self.move_selections(self.focused, Movement::Line, false)?;
            }
            Key::Char('X') => {
                //self.move_selections(self.focused, Movement::Line, true)?;
            }
            // Key::Char('C')
            Key::Char('g') => {
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Goto {
                    selecting: false,
                })));
            }
            Key::Char('G') => {
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Goto {
                    selecting: true,
                })));
            }
            Key::Char(':') => {
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Command)))
            }
            Key::Char('h') => actions.push(Action::Window(WindowAction::Move(Movement::Left(1)))),
            Key::Char('j') => actions.push(Action::Window(WindowAction::Move(Movement::Down(1)))),
            Key::Char('k') => actions.push(Action::Window(WindowAction::Move(Movement::Up(1)))),
            Key::Char('l') => actions.push(Action::Window(WindowAction::Move(Movement::Right(1)))),
            Key::Char('H') => {
                actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Left(1))))
            }
            Key::Char('J') => {
                actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Down(1))))
            }
            Key::Char('K') => actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Up(1)))),
            Key::Char('L') => {
                actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Right(1))))
            }
            Key::Char('d') => actions.push(Action::Window(WindowAction::Delete)),
            Key::Char('u') => actions.push(Action::Buffer(BufferAction::Undo)),
            Key::Char('U') => actions.push(Action::Buffer(BufferAction::Redo)),
            _ => {}
        },
        Mode::Goto { selecting } => {
            let wrapper = |m| {
                if selecting {
                    Action::Window(WindowAction::ShiftEnd(m))
                } else {
                    Action::Window(WindowAction::Move(m))
                }
            };
            let movement = match key {
                Key::Char('h') => Some(Movement::LineStart),
                Key::Char('j') => Some(Movement::FileEnd),
                Key::Char('k') => Some(Movement::FileStart),
                Key::Char('l') => Some(Movement::LineEnd),
                _ => None,
            };
            if let Some(movement) = movement {
                actions.push(wrapper(movement));
            }
            actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Normal)))
        }
        mode @ Mode::Insert | mode @ Mode::Append => match key {
            Key::Escape => actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Normal))),
            Key::Char(c) => match mode {
                Mode::Insert => {
                    actions.push(Action::Window(WindowAction::InsertAtSelectionStart(c)));
                    actions.push(Action::Window(WindowAction::ShiftStart(Movement::Right(1))));
                    actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Right(1))));
                }
                Mode::Append => {
                    actions.push(Action::Window(WindowAction::ShiftEnd(Movement::Right(1))));
                    actions.push(Action::Window(WindowAction::InsertAtSelectionEnd(c)));
                }
                _ => unreachable!(),
            },
            Key::Backspace => {
                actions.push(Action::Window(WindowAction::Move(Movement::Left(1))));
                actions.push(Action::Window(WindowAction::Delete));
            }
            _ => {}
        },
        Mode::Command => match key {
            Key::Escape => {
                actions.push(Action::Command(CommandAction::Clear));
                actions.push(Action::Window(WindowAction::SwitchToMode(Mode::Normal)));
            }
            Key::Char('\t') => actions.push(Action::Command(CommandAction::Tab)),
            Key::Ctrl(b'm') | Key::Char('\n') => {
                actions.push(Action::Command(CommandAction::Return))
            }
            Key::Char(c) => actions.push(Action::Command(CommandAction::Character(c))),
            Key::Backspace => actions.push(Action::Command(CommandAction::Backspace)),
            _ => {}
        },
    }

    if let Err(e) = actions
        .into_iter()
        .try_for_each(|action| perform_action(&mut state.editor, action))
    {
        state.editor.pending_message = Some((Importance::Error, e.to_string()));
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
    draw_window(
        state,
        state.editor.open_tabs[state.editor.focused_tab],
        region,
    )?;
    state.editor.last_screen_height = Some(region.height());

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
    for (window_id, window) in state.editor.windows.iter_with_handles() {
        let buffer = &state.editor.buffers[window.buffer];
        if window_id == state.editor.open_tabs[state.editor.focused_tab] {
            write!(state.tty, "{}{}{} ", style::Bold, buffer.name, style::Reset,)?;
        } else {
            write!(state.tty, "{} ", buffer.name)?;
        }
    }
    state.tabline_needs_redraw = false;
    Ok(())
}

fn draw_status(state: &mut State, region: Rect) -> Result<()> {
    if let Some((_importance, message)) = state.editor.pending_message.take() {
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
        let mode = state.editor.windows[state.editor.open_tabs[state.editor.focused_tab]].mode;
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
                    state.editor.windows[state.editor.open_tabs[state.editor.focused_tab]].command,
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
    let window = &mut state.editor.windows[window_id];
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
    let buffer = &state.editor.buffers[window.buffer];
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
                    line: LineIndex::from_zero_based(line),
                    column: ColumnIndex::from_zero_based(file_col),
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
