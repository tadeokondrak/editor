mod terminal;

use anyhow::Result;

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use editor::location::{Column, Line, Movement, Position, Selection};
use editor::{
    do_action, show_message, Action, Buffer, History, Importance, Mode, StateRef, Window, WindowId,
};
use handy::typed::TypedHandleMap;
use log::{error, info, trace};
use ropey::Rope;
use signal_hook::{iterator::Signals, SIGWINCH};
use std::{
    fs::File,
    io::{self, Write as _},
    os::raw::c_int,
    thread,
};
use terminal::{Point, Rect};
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

pub struct State {
    pub signals: Receiver<c_int>,
    pub inputs: Receiver<io::Result<Event>>,
    pub exit_channels: (Sender<()>, Receiver<()>),
    pub windows: TypedHandleMap<Window>,
    pub buffers: TypedHandleMap<Buffer>,
    pub open_tabs: Vec<WindowId>,
    pub focused_tab: usize,
    pub tty: RawTerminal<File>,
    pub tabline_needs_redraw: bool,
    pub statusline_needs_redraw: bool,
    pub last_screen_height: Option<u16>,
    pub pending_message: Option<(Importance, String)>,
    pub want_quit: bool,
}

impl State {
    fn ref_(&mut self) -> StateRef {
        StateRef {
            windows: &mut self.windows,
            buffers: &mut self.buffers,
            open_tabs: &mut self.open_tabs,
            focused_tab: &mut self.focused_tab,
            last_screen_height: &mut self.last_screen_height,
            pending_message: &mut self.pending_message,
            want_quit: &mut self.want_quit,
        }
    }
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
            want_quit: false,
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
    while !state.want_quit {
        draw(&mut state)?;
        match handle_next_event(&mut state) {
            Ok(true) => continue,
            Ok(false) => return Ok(()),
            Err(err) => {
                error!("{}", err);
                show_message(&mut state.ref_(), Importance::Error, err.to_string());
            }
        }
    }
    Ok(())
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
            Event::Key(Key::Home) => actions.push(Action::Window_Move(Movement::LineStart)),
            Event::Key(Key::End) => actions.push(Action::Window_Move(Movement::LineEnd)),
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
            // Event::Key(Key::Char('C'))
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
        .try_for_each(|action| do_action(&mut state.ref_(), action))
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
