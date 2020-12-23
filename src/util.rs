#![allow(unsafe_code, unused)]

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

pub(crate) struct MaybeSend<T, U> {
    _send: PhantomData<T>,
    inner: U,
}

impl<T: Send, U> MaybeSend<T, U> {
    #[inline]
    pub(crate) fn new(inner: U) -> Self {
        Self {
            _send: PhantomData,
            inner,
        }
    }
}

impl<T, U> MaybeSend<T, U> {
    #[inline]
    pub(crate) unsafe fn new_unchecked(inner: U) -> Self {
        Self {
            _send: PhantomData,
            inner,
        }
    }
}

impl<T, U> Deref for MaybeSend<T, U> {
    type Target = U;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T, U> DerefMut for MaybeSend<T, U> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

unsafe impl<T: Send, U> Send for MaybeSend<T, U> {}
