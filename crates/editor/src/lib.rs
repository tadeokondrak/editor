pub mod location;

use anyhow::{format_err, Context as _, Result};
use handy::typed::{TypedHandle, TypedHandleMap};
use location::{Line, Movement, MovementError, Position, Selection};
use log::trace;
use ropey::Rope;
use shlex::split as shlex;
use std::{
    collections::VecDeque,
    fmt::Debug,
    fs::{File, OpenOptions},
    mem::take,
    path::PathBuf,
};

pub type WindowId = TypedHandle<WindowData>;
pub type BufferId = TypedHandle<BufferData>;
pub type SelectionId = TypedHandle<Selection>;

pub struct EditorData {
    pub windows: TypedHandleMap<WindowData>,
    pub buffers: TypedHandleMap<BufferData>,
    pub open_tabs: Vec<WindowId>,
    pub focused_tab: usize,
    pub last_screen_height: Option<u16>,
    pub pending_message: Option<(Importance, String)>,
    pub want_quit: bool,
}

pub struct WindowData {
    pub buffer: BufferId,
    pub mode: Mode,
    pub selections: TypedHandleMap<Selection>,
    pub primary_selection: SelectionId,
    pub command: String,
    pub top: Line,
}

pub struct BufferData {
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
    pub editor: &'a mut EditorData,
    pub window: WindowId,
}

pub struct CommandDesc {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    #[allow(dead_code)]
    pub description: &'static str,
    #[allow(dead_code)]
    pub required_arguments: usize,
    pub run: fn(cx: Context, args: &[&str]) -> Result<()>,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Copy, Clone)]
pub enum Action {
    Editor(EditorAction),
    Buffer(BufferAction),
    Window(WindowAction),
    Command(CommandAction),
}

#[derive(Debug, Copy, Clone)]
pub enum EditorAction {
    Quit,
    PreviousTab,
    NextTab,
}

#[derive(Debug, Copy, Clone)]
pub enum BufferAction {
    Undo,
    Redo,
}

#[derive(Debug, Copy, Clone)]
pub enum WindowAction {
    InsertAtSelectionStart(char),
    InsertAtSelectionEnd(char),
    Delete,
    Move(Movement),
    ShiftStart(Movement),
    ShiftEnd(Movement),
    ScrollPageUp,
    ScrollPageDown,
    ScrollHalfPageUp,
    ScrollHalfPageDown,
    OrderSelections,
    SwitchToMode(Mode),
}

#[derive(Debug, Copy, Clone)]
pub enum CommandAction {
    Character(char),
    Clear,
    Tab,
    Return,
    Backspace,
}

impl EditorData {
    pub fn new() -> EditorData {
        let mut windows = TypedHandleMap::new();
        let mut buffers = TypedHandleMap::new();
        let scratch_buffer = buffers.insert(BufferData {
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
        let focused_window = windows.insert(WindowData {
            buffer: scratch_buffer,
            mode: Mode::Normal,
            selections,
            primary_selection,
            command: String::new(),
            top: Line::from_one_based(1),
        });
        EditorData {
            windows,
            buffers,
            open_tabs: vec![focused_window],
            focused_tab: 0,
            last_screen_height: None,
            pending_message: None,
            want_quit: false,
        }
    }
}

impl Default for EditorData {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run_command(state: &mut EditorData, args: &[&str]) -> Result<()> {
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
fn move_to(
    state: &mut EditorData,
    movement: Movement,
    selecting: bool,
) -> Result<(), MovementError> {
    let window_id = state.open_tabs[state.focused_tab];
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    for selection in window.selections.iter_mut() {
        selection.move_to(&buffer.content, movement, selecting)?
    }
    Ok(())
}

pub fn perform_editor_action(state: &mut EditorData, action: EditorAction) -> Result<()> {
    match action {
        EditorAction::Quit => {
            state.want_quit = true;
        }
        EditorAction::PreviousTab => {
            state.focused_tab = (state.focused_tab - 1) % state.open_tabs.len();
        }
        EditorAction::NextTab => {
            state.focused_tab = (state.focused_tab + 1) % state.open_tabs.len();
        }
    }
    Ok(())
}

pub fn perform_buffer_action(state: &mut EditorData, action: BufferAction) -> Result<()> {
    let window_id = state.open_tabs[state.focused_tab];
    match action {
        BufferAction::Undo => {
            undo(state, window_id);
        }
        BufferAction::Redo => {
            redo(state, window_id);
        }
    }
    Ok(())
}

pub fn perform_window_action(state: &mut EditorData, action: WindowAction) -> Result<()> {
    let window_id = state.open_tabs[state.focused_tab];
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    for selection in window.selections.iter_mut() {
        match action {
            WindowAction::InsertAtSelectionStart(c) => {
                selection.start.insert_char(buffer, c);
            }
            WindowAction::InsertAtSelectionEnd(c) => {
                selection.end.insert_char(buffer, c);
            }
            WindowAction::Delete => {
                selection.remove_from(buffer);
            }
            WindowAction::Move(movement) => {
                selection.end.move_to(&buffer.content, movement)?;
                selection.start = selection.end;
            }
            WindowAction::ShiftStart(movement) => {
                selection.start.move_to(&buffer.content, movement)?;
            }
            WindowAction::ShiftEnd(movement) => {
                selection.end.move_to(&buffer.content, movement)?;
            }
            WindowAction::ScrollPageUp
            | WindowAction::ScrollPageDown
            | WindowAction::ScrollHalfPageUp
            | WindowAction::ScrollHalfPageDown => {
                if let Some(height) = state.last_screen_height {
                    let height = usize::from(height);
                    let movement = match action {
                        WindowAction::ScrollPageUp => Movement::Up(height),
                        WindowAction::ScrollPageDown => Movement::Down(height),
                        WindowAction::ScrollHalfPageUp => Movement::Up(height / 2),
                        WindowAction::ScrollHalfPageDown => Movement::Down(height / 2),
                        _ => unreachable!(),
                    };
                    selection.end.move_to(&buffer.content, movement)?;
                    selection.start = selection.end;
                }
            }
            WindowAction::OrderSelections => {
                selection.order();
            }
            WindowAction::SwitchToMode(mode) => {
                window.mode = mode;
            }
        }
    }
    Ok(())
}

pub fn perform_command_action(state: &mut EditorData, action: CommandAction) -> Result<()> {
    match action {
        CommandAction::Character(c) => {
            state.windows[state.open_tabs[state.focused_tab]]
                .command
                .push(c);
            Ok(())
        }
        CommandAction::Clear => {
            state.windows[state.open_tabs[state.focused_tab]]
                .command
                .clear();
            Ok(())
        }
        CommandAction::Tab => {
            // TODO
            Ok(())
        }
        CommandAction::Return => {
            let command = take(&mut state.windows[state.open_tabs[state.focused_tab]].command);
            state.windows[state.open_tabs[state.focused_tab]].mode = Mode::Normal;
            let command = shlex(&command)
                .ok_or_else(|| format_err!("failed to parse command '{}'", command))?;
            trace!("command: {:?}", command);
            let command = command.iter().map(|x| &**x).collect::<Vec<&str>>();
            run_command(state, &command)?;
            Ok(())
        }
        CommandAction::Backspace => {
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

pub fn perform_action(state: &mut EditorData, action: Action) -> Result<()> {
    match action {
        Action::Editor(editor_action) => perform_editor_action(state, editor_action),
        Action::Buffer(buffer_action) => perform_buffer_action(state, buffer_action),
        Action::Window(window_action) => perform_window_action(state, window_action),
        Action::Command(command_action) => perform_command_action(state, command_action),
    }
}

pub fn show_message(state: &mut EditorData, importance: Importance, message: String) {
    state.pending_message = Some((importance, message));
}

pub fn undo(state: &mut EditorData, window_id: WindowId) {
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

pub fn redo(_state: &mut EditorData, _window_id: WindowId) {
    todo!()
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
        run: |cx, _args| perform_editor_action(cx.editor, EditorAction::Quit),
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
            let buffer = BufferData {
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
            let window = WindowData {
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
