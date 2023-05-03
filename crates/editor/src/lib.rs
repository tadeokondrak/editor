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

pub type WindowId = TypedHandle<Window>;
pub type BufferId = TypedHandle<Buffer>;
pub type SelectionId = TypedHandle<Selection>;

pub struct StateRef<'a> {
    pub windows: &'a mut TypedHandleMap<Window>,
    pub buffers: &'a mut TypedHandleMap<Buffer>,
    pub open_tabs: &'a mut Vec<WindowId>,
    pub focused_tab: &'a mut usize,
    pub last_screen_height: &'a mut Option<u16>,
    pub pending_message: &'a mut Option<(Importance, String)>,
    pub want_quit: &'a mut bool,
}

pub struct Window {
    pub buffer: BufferId,
    pub mode: Mode,
    pub selections: TypedHandleMap<Selection>,
    pub primary_selection: SelectionId,
    pub command: String,
    pub top: Line,
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

pub struct Context<'a, 'b> {
    pub editor: &'a mut StateRef<'b>,
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

pub fn run_command(state: &mut StateRef, args: &[&str]) -> Result<()> {
    let name = args.first().copied().context("no command given")?;
    let cmd = COMMANDS
        .iter()
        .find(|desc| desc.name == name || desc.aliases.contains(&name))
        .ok_or_else(|| format_err!("command '{}' doesn't exist", name))?;
    (cmd.run)(
        Context {
            window: state.open_tabs[*state.focused_tab],
            editor: state,
        },
        &args[1..],
    )
}

#[allow(dead_code)]
fn move_to(state: &mut StateRef, movement: Movement, selecting: bool) -> Result<(), MovementError> {
    let window_id = state.open_tabs[*state.focused_tab];
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    for selection in window.selections.iter_mut() {
        selection.move_to(&buffer.content, movement, selecting)?
    }
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Debug, Copy, Clone)]
pub enum Action {
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

pub fn do_action(state: &mut StateRef, action: Action) -> Result<()> {
    match action {
        Action::Editor_PreviousTab => {
            *state.focused_tab = (*state.focused_tab - 1) % state.open_tabs.len();
            Ok(())
        }
        Action::Editor_NextTab => {
            *state.focused_tab = (*state.focused_tab + 1) % state.open_tabs.len();
            Ok(())
        }
        Action::Buffer_Undo => {
            undo(state, state.open_tabs[*state.focused_tab]);
            Ok(())
        }
        Action::Buffer_Redo => {
            redo(state, state.open_tabs[*state.focused_tab]);
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
            let window_id = state.open_tabs[*state.focused_tab];
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
                            let height = usize::from(*height);
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
            state.windows[state.open_tabs[*state.focused_tab]].mode = mode;
            Ok(())
        }
        Action::Command_Character(c) => {
            state.windows[state.open_tabs[*state.focused_tab]]
                .command
                .push(c);
            Ok(())
        }
        Action::Command_Clear => {
            state.windows[state.open_tabs[*state.focused_tab]]
                .command
                .clear();
            Ok(())
        }
        Action::Command_Tab => {
            // TODO
            Ok(())
        }
        Action::Command_Return => {
            let command = take(&mut state.windows[state.open_tabs[*state.focused_tab]].command);
            state.windows[state.open_tabs[*state.focused_tab]].mode = Mode::Normal;
            let command = shlex(&command)
                .ok_or_else(|| format_err!("failed to parse command '{}'", command))?;
            trace!("command: {:?}", command);
            let command = command.iter().map(|x| &**x).collect::<Vec<&str>>();
            run_command(state, &command)?;
            Ok(())
        }
        Action::Command_Backspace => {
            if state.windows[state.open_tabs[*state.focused_tab]]
                .command
                .pop()
                .is_none()
            {
                let mode: Mode = Mode::Normal;
                state.windows[state.open_tabs[*state.focused_tab]].mode = mode;
            }
            Ok(())
        }
    }
}

pub fn show_message(state: &mut StateRef, importance: Importance, message: String) {
    *state.pending_message = Some((importance, message));
}

pub fn quit(state: &mut StateRef) {
    *state.want_quit = true;
}

pub fn undo(state: &mut StateRef, window_id: WindowId) {
    let window = &mut state.windows[window_id];
    let buffer = &mut state.buffers[window.buffer];
    match buffer.history.undo(&mut buffer.content) {
        Ok(()) => {
            let window_id = state.open_tabs[*state.focused_tab];
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

pub fn redo(_state: &mut StateRef, _window_id: WindowId) {
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
            *cx.editor.focused_tab = focused_tab;
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
