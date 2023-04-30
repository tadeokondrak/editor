use crate::Buffer;
use ropey::{Rope, RopeSlice};
use std::{
    mem::swap,
    num::NonZeroUsize,
    ops::{Add, AddAssign, Range, Sub, SubAssign},
};
use thiserror::Error;

macro_rules! newtype_impl {
    ($type:ty) => {
        impl $type {
            pub fn from_zero_based(i: usize) -> Self {
                Self::from_one_based(i + 1)
            }

            pub fn from_one_based(i: usize) -> Self {
                Self(NonZeroUsize::new(i).unwrap())
            }

            pub fn zero_based(self) -> usize {
                self.one_based() - 1
            }

            pub fn one_based(self) -> usize {
                self.0.get()
            }
        }

        impl<T> Add<T> for $type
        where
            usize: Add<T>,
            <usize as Add<T>>::Output: Into<usize>,
        {
            type Output = Self;
            fn add(self, other: T) -> Self {
                Self::from_one_based(self.one_based().add(other.into()).into())
            }
        }

        impl<T> AddAssign<T> for $type
        where
            usize: AddAssign<T>,
        {
            fn add_assign(&mut self, other: T) {
                let mut this = self.one_based();
                this.add_assign(other);
                *self = Self::from_one_based(this);
            }
        }

        impl<T> Sub<T> for $type
        where
            usize: Sub<T>,
            <usize as Sub<T>>::Output: Into<usize>,
        {
            type Output = Self;
            fn sub(self, other: T) -> Self {
                Self::from_one_based(self.one_based().sub(other.into()).into())
            }
        }

        impl<T> SubAssign<T> for $type
        where
            usize: SubAssign<T>,
        {
            fn sub_assign(&mut self, other: T) {
                let mut this = self.one_based();
                this.sub_assign(other);
                *self = Self::from_one_based(this);
            }
        }
    };
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Line(NonZeroUsize);

newtype_impl!(Line);

impl Line {
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
    pub fn remove_from(self, _buffer: &mut Buffer) {
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
pub struct Column(NonZeroUsize);

newtype_impl!(Column);

impl Column {
    pub fn is_first(self) -> bool {
        self.one_based() == 1
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Position {
    pub line: Line,
    pub column: Column,
}

impl Position {
    pub fn file_start() -> Self {
        Self {
            line: Line::from_one_based(1),
            column: Column::from_one_based(1),
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

    pub fn insert_char(self, buffer: &mut Buffer, c: char) {
        buffer.history.insert_char(&mut buffer.content, self, c);
    }

    pub fn validate(&mut self, rope: &Rope) {
        if !self.is_valid(rope) {
            if self.line.is_empty(rope) {
                if !self.line.is_first() {
                    self.move_to(rope, Movement::Up(1)).unwrap();
                    self.move_to(rope, Movement::LineEnd).unwrap();
                } else {
                    assert_eq!(rope.len_chars(), 0);
                    self.line = Line::from_one_based(1);
                    self.column = Column::from_one_based(1);
                    panic!("{}", MovementError::SelectionEmpty);
                }
            } else {
                self.move_to(rope, Movement::LineEnd).unwrap();
            }
        }
    }

    pub fn validate_fix(&mut self, buffer: &mut Buffer) {
        if !self.is_valid(&buffer.content) {
            if self.line.is_empty(&buffer.content) {
                if !self.line.is_first() {
                    self.move_to(&buffer.content, Movement::Up(1)).unwrap();
                    self.move_to(&buffer.content, Movement::LineEnd).unwrap();
                } else {
                    assert_eq!(buffer.content.len_chars(), 0);
                    self.line = Line::from_one_based(1);
                    self.column = Column::from_one_based(1);
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
                        self.column -= 1;
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
                        self.column += 1;
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
                self.line -= n;
            }
            Movement::Down(n) => {
                if n == 0 {
                    return Ok(());
                }
                // TODO: remove the loop
                let mut moved = false;
                for _ in 0..n {
                    if !self.line.is_last(rope) && (self.line + 1).slice_of(rope).len_chars() > 0 {
                        self.line += 1;
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
                self.column = Column::from_one_based(1);
            }
            Movement::LineEnd => {
                self.column = Column::from_one_based(self.line.slice_of(rope).len_chars());
            }
            Movement::FileStart => {
                self.line = Line::from_one_based(1);
                self.move_to(rope, Movement::LineStart)?;
            }
            Movement::FileEnd => {
                let last = Line::from_one_based(rope.len_lines());
                if !last.is_empty(rope) {
                    self.line = last;
                } else {
                    self.line = last - 1;
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

    pub fn validate_fix(&mut self, buffer: &mut Buffer) {
        self.start.validate_fix(buffer);
        self.end.validate_fix(buffer);
    }

    pub fn remove_from(&mut self, buffer: &mut Buffer) {
        self.validate(&buffer.content);
        self.order();
        buffer.history.remove_selection(&mut buffer.content, *self);
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
