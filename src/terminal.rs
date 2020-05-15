use std::ops::RangeInclusive;
use termion::cursor;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct Point {
    pub x: u16,
    pub y: u16,
}

impl Point {
    pub fn goto(self) -> cursor::Goto {
        cursor::Goto(self.x, self.y)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct Rect {
    pub start: Point,
    pub end: Point,
}

impl Rect {
    pub fn width(self) -> u16 {
        self.end.x - self.start.x
    }

    pub fn height(self) -> u16 {
        self.end.y - self.start.y
    }

    pub fn range_x(self) -> RangeInclusive<u16> {
        self.start.x..=self.end.x
    }

    pub fn range_y(self) -> RangeInclusive<u16> {
        self.start.y..=self.end.y
    }
}
