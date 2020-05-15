use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut, Index, IndexMut},
};

pub trait Id: Sized {
    fn id(self) -> usize;
}

pub struct IdVec<I: Id, T>(Vec<T>, PhantomData<I>);

impl<I: Id, T> Index<I> for IdVec<I, T> {
    type Output = T;

    fn index(&self, index: I) -> &Self::Output {
        &self.0[index.id()]
    }
}

impl<I: Id, T> IndexMut<I> for IdVec<I, T> {
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.0[index.id()]
    }
}

impl<I: Id, T> Deref for IdVec<I, T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<I: Id, T> DerefMut for IdVec<I, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<I: Id, T> From<Vec<T>> for IdVec<I, T> {
    fn from(vec: Vec<T>) -> Self {
        Self(vec, PhantomData)
    }
}
