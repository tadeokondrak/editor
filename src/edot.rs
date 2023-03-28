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
    collections::{HashMap, VecDeque},
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
    commands: HashMap<String, CommandDesc>,
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

impl Edot {
    pub fn new() -> Result<Self> {
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
        Ok(Self {
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
                history: VecDeque::new(),
                path: None,
            }]
            .into(),
            commands: HashMap::new(),
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
    pub fn run(mut self) -> Result {
        write!(
            self.output,
            "{}{}{}",
            screen::ToAlternateScreen,
            cursor::Hide,
            cursor::SteadyBar
        )?;
        self.register::<Quit>("q")
            .register::<Quit>("quit")
            .register::<Edit>("e")
            .register::<Edit>("edit")
            .register::<Write>("w")
            .register::<Write>("write");
        loop {
            self.draw()?;
            match self.main() {
                Ok(true) => continue,
                Ok(false) => return Ok(()),
                Err(err) => {
                    error!("{}", err);
                    self.show_message(Importance::Error, err.to_string());
                }
            }
        }
    }

    fn main(&mut self) -> Result<bool> {
        select! {
            recv(self.input) -> input => self.event(input??)?,
            recv(self.signal) -> signal => self.signal(signal?)?,
            recv(self.exit.1) -> exit => { exit?; return Ok(false); },
        }
        Ok(true)
    }

    fn cmd(&mut self, args: &[&str]) -> Result {
        let name = args.get(0).context("no command given")?;
        let cmd = self
            .commands
            .get(*name)
            .ok_or_else(|| format_err!("command '{}' doesn't exist", name))?;
        (cmd.run)(
            Context {
                window: self.focused,
                editor: self,
            },
            &args[1..],
        )?;
        Ok(())
    }

    fn register<T: Command>(&mut self, s: &str) -> &mut Self {
        self.commands.insert(s.to_owned(), CommandDesc::of::<T>());
        self
    }

    fn event(&mut self, event: Event) -> Result {
        trace!("event: {:?}", event);

        const SHIFT_UP: &[u8] = &[27, 91, 49, 59, 50, 65];
        const SHIFT_DOWN: &[u8] = &[27, 91, 49, 59, 50, 66];
        const SHIFT_RIGHT: &[u8] = &[27, 91, 49, 59, 50, 67];
        const SHIFT_LEFT: &[u8] = &[27, 91, 49, 59, 50, 68];

        'arrows: {
            if let Mode::Normal | Mode::Insert | Mode::Append = self.windows[self.focused].mode {
                match event {
                    Event::Key(Key::Left) => {
                        self.move_selections(self.focused, Movement::Left(1), false)?;
                    }
                    Event::Key(Key::Down) => {
                        self.move_selections(self.focused, Movement::Down(1), false)?;
                    }
                    Event::Key(Key::Up) => {
                        self.move_selections(self.focused, Movement::Up(1), false)?;
                    }
                    Event::Key(Key::Right) => {
                        self.move_selections(self.focused, Movement::Right(1), false)?;
                    }
                    Event::Key(Key::Ctrl('p')) => {
                        self.focused = WindowId((self.focused.0 - 1) % self.windows.len());
                    }
                    Event::Key(Key::Ctrl('n')) => {
                        self.focused = WindowId((self.focused.0 + 1) % self.windows.len());
                    }
                    Event::Unsupported(keys) => match keys.as_slice() {
                        SHIFT_LEFT => {
                            self.move_selections(self.focused, Movement::Left(1), true)?;
                        }
                        SHIFT_DOWN => {
                            self.move_selections(self.focused, Movement::Down(1), true)?;
                        }
                        SHIFT_UP => {
                            self.move_selections(self.focused, Movement::Up(1), true)?;
                        }
                        SHIFT_RIGHT => {
                            self.move_selections(self.focused, Movement::Right(1), true)?;
                        }
                        _ => {}
                    },
                    _ => break 'arrows,
                }
                return Ok(());
            }
        }

        match self.windows[self.focused].mode {
            Mode::Normal => match event {
                Event::Key(Key::Char('i')) => {
                    self.order_selections(self.focused);
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('c')) => {
                    self.delete_selections(self.focused);
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('a')) => {
                    self.order_selections(self.focused);
                    self.set_mode(self.focused, Mode::Append);
                }
                Event::Key(Key::Char('A')) => {
                    self.move_selections(self.focused, Movement::LineEnd, false)?;
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('o')) => {
                    for selection_id in self.selections(self.focused) {
                        self.move_selection(self.focused, selection_id, Movement::LineEnd, false)?;
                        self.insert_char_after(self.focused, selection_id, '\n');
                        self.move_selection(self.focused, selection_id, Movement::Down(1), false)?;
                        self.move_selection(
                            self.focused,
                            selection_id,
                            Movement::LineStart,
                            false,
                        )?;
                    }
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('x')) => {
                    // self.move_selections(self.focused, Movement::Line, false)?;
                }
                Event::Key(Key::Char('X')) => {
                    // self.move_selections(self.focused, Movement::Line, true)?;
                }
                Event::Key(Key::Char('g')) => {
                    self.set_mode(self.focused, Mode::Goto { drag: false });
                }
                Event::Key(Key::Char('G')) => {
                    self.set_mode(self.focused, Mode::Goto { drag: true });
                }
                Event::Key(Key::Char(':')) => {
                    self.set_mode(self.focused, Mode::Command);
                }
                Event::Key(Key::Char('h')) => {
                    self.move_selections(self.focused, Movement::Left(1), false)?;
                }
                Event::Key(Key::Char('j')) => {
                    self.move_selections(self.focused, Movement::Down(1), false)?;
                }
                Event::Key(Key::Char('k')) => {
                    self.move_selections(self.focused, Movement::Up(1), false)?;
                }
                Event::Key(Key::Char('l')) => {
                    self.move_selections(self.focused, Movement::Right(1), false)?;
                }
                Event::Key(Key::Char('H')) => {
                    self.move_selections(self.focused, Movement::Left(1), true)?;
                }
                Event::Key(Key::Char('J')) => {
                    self.move_selections(self.focused, Movement::Down(1), true)?;
                }
                Event::Key(Key::Char('K')) => {
                    self.move_selections(self.focused, Movement::Up(1), true)?;
                }
                Event::Key(Key::Char('L')) => {
                    self.move_selections(self.focused, Movement::Right(1), true)?;
                }
                Event::Key(Key::Char('d')) => {
                    self.delete_selections(self.focused);
                }
                Event::Key(Key::Ctrl('u')) => {
                    if let Some(height) = self.last_screen_height {
                        self.move_selection(
                            self.focused,
                            SelectionId(0),
                            Movement::Up(usize::from(height / 2)),
                            false,
                        )?;
                    }
                }
                Event::Key(Key::Ctrl('d')) => {
                    if let Some(height) = self.last_screen_height {
                        self.move_selection(
                            self.focused,
                            SelectionId(0),
                            Movement::Down(usize::from(height / 2)),
                            false,
                        )?;
                    }
                }
                Event::Key(Key::Ctrl('b') | Key::PageUp) => {
                    if let Some(height) = self.last_screen_height {
                        self.move_selection(
                            self.focused,
                            SelectionId(0),
                            Movement::Up(usize::from(height)),
                            false,
                        )?;
                    }
                }
                Event::Key(Key::Ctrl('f') | Key::PageDown) => {
                    if let Some(height) = self.last_screen_height {
                        self.move_selection(
                            self.focused,
                            SelectionId(0),
                            Movement::Down(usize::from(height)),
                            false,
                        )?;
                    }
                }
                _ => {}
            },
            Mode::Goto { drag } => {
                match event {
                    Event::Key(Key::Char('h')) => {
                        self.move_selections(self.focused, Movement::LineStart, drag)?;
                    }
                    Event::Key(Key::Char('j')) => {
                        self.move_selections(self.focused, Movement::FileEnd, drag)?;
                    }
                    Event::Key(Key::Char('k')) => {
                        self.move_selections(self.focused, Movement::FileStart, drag)?;
                    }
                    Event::Key(Key::Char('l')) => {
                        self.move_selections(self.focused, Movement::LineEnd, drag)?;
                    }
                    _ => {}
                };
                self.set_mode(self.focused, Mode::Normal);
            }
            mode @ Mode::Insert | mode @ Mode::Append => match event {
                Event::Key(Key::Esc) => self.set_mode(self.focused, Mode::Normal),
                Event::Key(Key::Char(c)) => {
                    for selection_id in self.selections(self.focused) {
                        match mode {
                            Mode::Insert => {
                                self.insert_char_before(self.focused, selection_id, c);
                                self.shift_selection(
                                    self.focused,
                                    selection_id,
                                    Movement::Right(1),
                                )?;
                            }
                            Mode::Append => {
                                match self.move_selection(
                                    self.focused,
                                    selection_id,
                                    Movement::Right(1),
                                    true,
                                ) {
                                    Ok(()) | Err(MovementError::NoNextLine) => {
                                        // Ignore the error here so we can add a newline at the end of a
                                        // file even if there isn't one there already.
                                    }
                                    Err(e) => return Err(e.into()),
                                }
                                self.insert_char_after(self.focused, selection_id, c);
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                Event::Key(Key::Backspace) => {
                    self.move_selections(self.focused, Movement::Left(1), false)?;
                    self.delete_selections(self.focused);
                }
                _ => {}
            },
            Mode::Command => match event {
                Event::Key(Key::Esc) => {
                    self.windows[self.focused].command.clear();
                    self.set_mode(self.focused, Mode::Normal);
                }
                Event::Key(Key::Char('\t')) => {}
                Event::Key(Key::Char('\n')) => {
                    let command = take(&mut self.windows[self.focused].command);
                    self.set_mode(self.focused, Mode::Normal);
                    let command = shlex(&command)
                        .ok_or_else(|| format_err!("failed to parse command '{}'", command))?;
                    trace!("command: {:?}", command);
                    let command = command.iter().map(|x| &**x).collect::<Vec<&str>>();
                    self.cmd(&command)?;
                }
                Event::Key(Key::Char(c)) => {
                    self.windows[self.focused].command.push(c);
                }
                Event::Key(Key::Backspace) => {
                    if self.windows[self.focused].command.pop().is_none() {
                        self.set_mode(self.focused, Mode::Normal);
                    } else {
                    }
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn signal(&mut self, signal: c_int) -> Result {
        info!("received signal: {}", signal);
        match signal {
            signal_hook::SIGWINCH => self.draw()?,
            _ => {}
        }
        Ok(())
    }

    fn draw(&mut self) -> Result {
        let (width, height) = terminal_size()?;

        let region = Rect {
            start: Point { x: 1, y: 1 },
            end: Point { x: width, y: 1 },
        };
        self.draw_tabs(region)?;

        let region = Rect {
            start: Point { x: 1, y: 2 },
            end: Point {
                x: width,
                y: height - 1,
            },
        };
        self.draw_window(self.focused, region)?;
        self.last_screen_height = Some(region.height());

        let region = Rect {
            start: Point { x: 1, y: height },
            end: Point {
                x: width,
                y: height,
            },
        };
        self.draw_status(region)?;

        self.output.flush()?;
        Ok(())
    }

    fn draw_tabs(&mut self, region: Rect) -> Result {
        write!(self.output, "{}{}", region.start.goto(), clear::CurrentLine)?;
        for window_id in (0..self.windows.len()).map(WindowId) {
            let window = &self.windows[window_id];
            let buffer = &self.buffers[window.buffer];
            if window_id == self.focused {
                write!(
                    self.output,
                    "{}{}{} ",
                    style::Bold,
                    buffer.name,
                    style::Reset,
                )?;
            } else {
                write!(self.output, "{} ", buffer.name)?;
            }
        }
        self.tabline_dirty = false;
        Ok(())
    }

    fn draw_status(&mut self, region: Rect) -> Result {
        if let Some((_importance, message)) = self.message.take() {
            write!(
                self.output,
                "{}{}{}{} {} {}",
                region.start.goto(),
                clear::CurrentLine,
                color::Bg(color::Red),
                color::Fg(color::White),
                message,
                style::Reset,
            )?;
        } else {
            let mode = self.windows[self.focused].mode;
            let color: &dyn Color = match mode {
                Mode::Normal => &color::White,
                Mode::Insert => &color::LightYellow,
                Mode::Append => &color::White,
                Mode::Goto { .. } => &color::White,
                Mode::Command => &color::White,
            };
            write!(
                self.output,
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
                        self.output,
                        " :{}{} {}",
                        self.windows[self.focused].command,
                        style::Invert,
                        style::Reset,
                    )?;
                }
                _ => {}
            }
            self.statusline_dirty = false;
        }
        Ok(())
    }

    fn draw_window(&mut self, window_id: WindowId, region: Rect) -> Result {
        // TODO: draw a block where the next character will go in insert mode
        let window = &mut self.windows[window_id];
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
        let buffer = &self.buffers[window.buffer];
        let mut lines = buffer
            .content
            .lines_at(window.top.zero_based())
            .enumerate()
            .map(|(line, text)| (line + window.top.zero_based(), text));
        let mut range_y = region.range_y();
        'outer: while let Some(y) = range_y.next() {
            write!(self.output, "{}{}", cursor::Goto(1, y), clear::CurrentLine)?;
            if let Some((line, text)) = lines.next() {
                let mut chars = text.chars().enumerate();
                let mut col = 0;
                while let Some((file_col, mut c)) = chars.next() {
                    if col == region.width() as usize + 1 {
                        write!(self.output, "\r\n{}", clear::CurrentLine)?;
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
                            write!(self.output, "{}    {}", style::Invert, style::Reset)?;
                            col += 4;
                        } else {
                            write!(self.output, "{}{}{}", style::Invert, c, style::Reset)?;
                            col += 1;
                        }
                    } else {
                        if c == '\t' {
                            write!(self.output, "    ")?;
                            col += 4;
                        } else {
                            write!(self.output, "{}", c)?;
                            col += 1;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn show_message(&mut self, importance: Importance, message: String) {
        self.message = Some((importance, message));
    }

    pub fn quit(&mut self) {
        self.exit.0.send(()).unwrap();
    }

    pub fn set_mode(&mut self, window: WindowId, mode: Mode) {
        self.windows[window].mode = mode;
        match mode {
            Mode::Normal => {}
            Mode::Insert => {}
            Mode::Append => {}
            Mode::Goto { .. } => {}
            Mode::Command => {}
        }
    }

    pub fn selections(&self, window: WindowId) -> impl Iterator<Item = SelectionId> {
        let window = &self.windows[window];
        (0..window.selections.len()).map(SelectionId)
    }

    pub fn insert_char_before(&mut self, window_id: WindowId, selection_id: SelectionId, c: char) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.start.insert_char(&mut buffer.content, c);
    }

    pub fn insert_char_after(&mut self, window_id: WindowId, selection_id: SelectionId, c: char) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.end.insert_char(&mut buffer.content, c);
    }

    pub fn move_selection(
        &mut self,
        window_id: WindowId,
        selection_id: SelectionId,
        movement: Movement,
        drag: bool,
    ) -> Result<(), MovementError> {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        let result = selection.end.move_to(&buffer.content, movement);
        if !drag {
            selection.start = selection.end;
        }
        result
    }

    pub fn move_selections(
        &mut self,
        window_id: WindowId,
        movement: Movement,
        drag: bool,
    ) -> Result<(), MovementError> {
        for selection_id in self.selections(window_id) {
            self.move_selection(window_id, selection_id, movement, drag)?;
        }
        Ok(())
    }

    pub fn shift_selection(
        &mut self,
        window_id: WindowId,
        selection_id: SelectionId,
        movement: Movement,
    ) -> Result<(), MovementError> {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.start.move_to(&buffer.content, movement)?;
        selection.end.move_to(&buffer.content, movement)?;
        Ok(())
    }

    pub fn shift_selections(
        &mut self,
        window_id: WindowId,
        movement: Movement,
    ) -> Result<(), MovementError> {
        for selection_id in self.selections(window_id) {
            self.shift_selection(window_id, selection_id, movement)?;
        }
        Ok(())
    }

    pub fn delete_selection(&mut self, window_id: WindowId, selection_id: SelectionId) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.remove_from(&mut buffer.content);
    }

    pub fn delete_selections(&mut self, window_id: WindowId) {
        for selection_id in self.selections(window_id) {
            self.delete_selection(window_id, selection_id);
        }
    }

    pub fn flip_selection(&mut self, window_id: WindowId, selection_id: SelectionId) {
        let window = &mut self.windows[window_id];
        let selection = &mut window.selections[selection_id];
        selection.flip();
    }

    pub fn order_selections(&mut self, window_id: WindowId) {
        for selection_id in self.selections(window_id) {
            self.order_selection(window_id, selection_id);
        }
    }

    pub fn order_selection(&mut self, window_id: WindowId, selection_id: SelectionId) {
        let window = &mut self.windows[window_id];
        let selection = &mut window.selections[selection_id];
        selection.order();
    }

    pub fn flip_selections(&mut self, window_id: WindowId) {
        for selection_id in self.selections(window_id) {
            self.flip_selection(window_id, selection_id);
        }
    }

    pub fn for_each_selection<F>(&mut self, window_id: WindowId, mut f: F) -> Result
    where
        F: FnMut(&mut Self, WindowId, SelectionId) -> Result,
    {
        let mut errors = Vec::new();
        for selection_id in self.selections(window_id) {
            if let Err(e) = f(self, window_id, selection_id) {
                errors.push(e);
            }
        }
        errors.pop().map_or(Ok(()), Err)
    }
}

impl Drop for Edot {
    fn drop(&mut self) {
        let _ = write!(
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
    path: Option<PathBuf>,
    name: String,
    content: Rope,
    history: VecDeque<Modification>,
}

#[derive(Debug, Copy, Clone)]
pub enum Modification {}

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

pub trait Command: Sized {
    const DESCRIPTION: &'static str;
    const REQUIRED_ARGUMENTS: usize = 0;

    fn run(cx: Context, args: &[&str]) -> Result;
}

pub struct CommandDesc {
    description: &'static str,
    required_arguments: usize,
    run: fn(cx: Context, args: &[&str]) -> Result,
}

impl CommandDesc {
    fn of<T: Command>() -> Self {
        Self {
            description: T::DESCRIPTION,
            required_arguments: T::REQUIRED_ARGUMENTS,
            run: T::run,
        }
    }
}

enum Quit {}

impl Command for Quit {
    const DESCRIPTION: &'static str = "quits the editor";

    fn run(cx: Context, _args: &[&str]) -> Result {
        cx.editor.quit();
        Ok(())
    }
}

enum Edit {}

impl Command for Edit {
    const DESCRIPTION: &'static str = "open a file";
    const REQUIRED_ARGUMENTS: usize = 1;

    fn run(cx: Context, args: &[&str]) -> Result {
        let name = String::from(args[0]);
        let path = PathBuf::from(&name).canonicalize()?;
        let reader = File::open(&path)?;
        let buffer = Buffer {
            path: Some(path),
            name,
            content: Rope::from_reader(reader)?,
            history: VecDeque::new(),
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
    }
}

enum Write {}

impl Command for Write {
    const DESCRIPTION: &'static str = "write the current buffer contents to disk";

    fn run(cx: Context, _args: &[&str]) -> Result {
        let buffer = &cx.editor.buffers[cx.editor.windows[cx.window].buffer];
        let path = buffer
            .path
            .as_ref()
            .context("cannot save a scratch buffer")?;
        let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
        buffer.content.write_to(&mut file)?;
        Ok(())
    }
}
