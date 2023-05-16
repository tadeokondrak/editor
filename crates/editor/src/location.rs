use crate::BufferData;
use ropey::{Rope, RopeSlice};
use std::{mem::swap, ops::Range};
use thiserror::Error;

macro_rules! newtype_impl {
    ($type:ty) => {
        impl $type {
            pub fn from_zero_based(i: usize) -> Self {
                Self::from_one_based(i + 1)
            }

            pub fn from_one_based(i: usize) -> Self {
                Self(i)
            }

            pub fn zero_based(self) -> usize {
                self.one_based() - 1
            }

            pub fn one_based(self) -> usize {
                self.0
            }
        }
    };
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct LineIndex(pub usize);

newtype_impl!(LineIndex);

impl LineIndex {
    #[allow(dead_code)]
    pub fn range_of(self, rope: &Rope) -> Range<usize> {
        self.char_of(rope)..self.char_of(rope) + self.slice_of(rope).len_chars()
    }

    pub fn slice_of(self, rope: &Rope) -> RopeSlice<'_> {
        rope.line(self.zero_based())
    }

    pub fn char_of(self, rope: &Rope) -> usize {
        rope.line_to_char(self.zero_based())
    }

    #[allow(dead_code)]
    pub fn remove_from(self, _buffer: &mut BufferData) {
        todo!()
    }

    pub fn is_first(self) -> bool {
        self.one_based() == 1
    }

    pub fn is_last(self, rope: &Rope) -> bool {
        self.one_based() == rope.len_lines()
    }

    pub fn is_empty(self, rope: &Rope) -> bool {
        self.slice_of(rope).len_chars() == 0
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct ColumnIndex(pub usize);

newtype_impl!(ColumnIndex);

impl ColumnIndex {
    pub fn is_first(self) -> bool {
        self.one_based() == 1
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Position {
    pub line: LineIndex,
    pub column: ColumnIndex,
}

impl Position {
    pub fn file_start() -> Self {
        Self {
            line: LineIndex::from_one_based(1),
            column: ColumnIndex::from_one_based(1),
        }
    }

    pub fn char_of(self, rope: &Rope) -> usize {
        self.line.char_of(rope) + self.column.zero_based()
    }

    pub fn is_valid(self, rope: &Rope) -> bool {
        self.column.one_based() <= self.line.slice_of(rope).len_chars()
    }

    #[allow(dead_code)]
    pub fn is_full_line(self, rope: &Rope) -> bool {
        self.line.slice_of(rope).len_chars() == self.column.zero_based()
    }

    pub fn insert_char(self, buffer: &mut BufferData, c: char) {
        buffer.content.insert_char(self.char_of(&buffer.content), c);
    }

    pub fn validate(&mut self, rope: &Rope) {
        if !self.is_valid(rope) {
            if self.line.is_empty(rope) {
                if !self.line.is_first() {
                    self.move_to(rope, Movement::Up(1)).unwrap();
                    self.move_to(rope, Movement::LineEnd).unwrap();
                } else {
                    assert_eq!(rope.len_chars(), 0);
                    self.line = LineIndex::from_one_based(1);
                    self.column = ColumnIndex::from_one_based(1);
                    panic!("{}", MovementError::SelectionEmpty);
                }
            } else {
                self.move_to(rope, Movement::LineEnd).unwrap();
            }
        }
    }

    pub fn validate_fix(&mut self, buffer: &mut BufferData) {
        if !self.is_valid(&buffer.content) {
            if self.line.is_empty(&buffer.content) {
                if !self.line.is_first() {
                    self.move_to(&buffer.content, Movement::Up(1)).unwrap();
                    self.move_to(&buffer.content, Movement::LineEnd).unwrap();
                } else {
                    assert_eq!(buffer.content.len_chars(), 0);
                    self.line = LineIndex::from_one_based(1);
                    self.column = ColumnIndex::from_one_based(1);
                    self.insert_char(buffer, '\n');
                }
            } else {
                self.move_to(&buffer.content, Movement::LineEnd).unwrap();
            }
        }
    }

    pub fn move_to(&mut self, rope: &Rope, movement: Movement) -> Result<(), MovementError> {
        match movement {
            Movement::Left(n) => {
                if n == 0 {
                    return Ok(());
                }
                // TODO: remove the loop
                let mut moved = false;
                for _ in 0..n {
                    self.validate(rope);
                    if self.column.is_first() {
                        if !self.line.is_first() {
                            self.move_to(rope, Movement::Up(1))?;
                            self.move_to(rope, Movement::LineEnd)?;
                            moved = true;
                        } else {
                            return Err(MovementError::NoPrevLine);
                        }
                    } else {
                        self.column.0 -= 1;
                        moved = true;
                    }
                }
                if !moved {
                    return Err(MovementError::NoPrevLine);
                }
            }
            Movement::Right(n) => {
                if n == 0 {
                    return Ok(());
                }
                // TODO: remove the loop
                let mut moved = false;
                for _ in 0..n {
                    self.validate(rope);
                    if self.column.one_based() == self.line.slice_of(rope).len_chars() {
                        self.move_to(rope, Movement::Down(1))?;
                        self.move_to(rope, Movement::LineStart)?;
                        moved = true;
                    } else {
                        self.column.0 += 1;
                        moved = true;
                    }
                }
                if !moved {
                    return Err(MovementError::NoNextLine);
                }
            }
            Movement::Up(n) => {
                if n == 0 {
                    return Ok(());
                }
                let n = n.min(self.line.zero_based());
                if n == 0 {
                    return Err(MovementError::NoPrevLine);
                }
                self.line.0 -= n;
            }
            Movement::Down(n) => {
                if n == 0 {
                    return Ok(());
                }
                // TODO: remove the loop
                let mut moved = false;
                for _ in 0..n {
                    if !self.line.is_last(rope)
                        && LineIndex(self.line.0 + 1).slice_of(rope).len_chars() > 0
                    {
                        self.line.0 += 1;
                        moved = true;
                    } else {
                        break;
                    }
                }
                if !moved {
                    return Err(MovementError::NoNextLine);
                }
            }
            Movement::LineStart => {
                self.column = ColumnIndex::from_one_based(1);
            }
            Movement::LineEnd => {
                self.column = ColumnIndex::from_one_based(self.line.slice_of(rope).len_chars());
            }
            Movement::FileStart => {
                self.line = LineIndex::from_one_based(1);
                self.move_to(rope, Movement::LineStart)?;
            }
            Movement::FileEnd => {
                let last = LineIndex::from_one_based(rope.len_lines());
                if !last.is_empty(rope) {
                    self.line = last;
                } else {
                    self.line = LineIndex(last.0 - 1);
                }
                self.move_to(rope, Movement::LineStart)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Selection {
    pub start: Position,
    pub end: Position,
}

impl Selection {
    pub fn range_of(mut self, rope: &Rope) -> Range<usize> {
        self.order();
        self.start.char_of(rope)..self.end.char_of(rope) + 1
    }

    pub fn slice_of(self, rope: &Rope) -> RopeSlice<'_> {
        rope.slice(self.range_of(rope))
    }

    pub fn order(&mut self) {
        if self.start > self.end {
            self.flip();
        }
    }

    #[allow(dead_code)]
    pub fn ordered(mut self) -> Self {
        self.order();
        self
    }

    pub fn contains(mut self, other: Position) -> bool {
        self.order();
        other >= self.start && other <= self.end
    }

    pub fn flip(&mut self) {
        swap(&mut self.start, &mut self.end);
    }

    #[allow(dead_code)]
    pub fn flipped(mut self) -> Self {
        self.flip();
        self
    }

    #[allow(dead_code)]
    pub fn is_ordered(self) -> bool {
        let ordered = self.ordered();
        self.start <= ordered.end
    }

    pub fn valid(mut self, rope: &Rope) -> Self {
        self.start.validate(rope);
        self.end.validate(rope);
        self
    }

    pub fn validate(&mut self, rope: &Rope) {
        self.start.validate(rope);
        self.end.validate(rope);
    }

    pub fn validate_fix(&mut self, buffer: &mut BufferData) {
        self.start.validate_fix(buffer);
        self.end.validate_fix(buffer);
    }

    pub fn remove_from(&mut self, buffer: &mut BufferData) {
        self.validate(&buffer.content);
        self.order();
        let range = self.range_of(&buffer.content);
        buffer.content.remove(range);
        self.end = self.start;
        self.validate_fix(buffer);
        // TODO: the file must be terminated by a final newline
    }

    pub fn move_to(
        &mut self,
        rope: &Rope,
        movement: Movement,
        should_drag: bool,
    ) -> Result<(), MovementError> {
        self.end.move_to(rope, movement)?;
        if !should_drag {
            self.start = self.end;
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Movement {
    Left(usize),
    Right(usize),
    Up(usize),
    Down(usize),
    LineStart,
    LineEnd,
    FileStart,
    FileEnd,
}

#[derive(Debug, Error, Copy, Clone)]
pub enum MovementError {
    #[error("selection is empty")]
    SelectionEmpty,
    #[error("no previous line")]
    NoPrevLine,
    #[error("no next line")]
    NoNextLine,
}
