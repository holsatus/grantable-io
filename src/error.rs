use core::sync::atomic::Ordering;

use maitake_sync::blocking::Mutex;
use portable_atomic::AtomicBool;

pub(crate) struct AtomicError<E> {
    has_error: AtomicBool,
    error: Mutex<Option<E>>,
}

impl<E> AtomicError<E> {
    pub(crate) const fn new() -> Self {
        AtomicError {
            has_error: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    pub(crate) fn set(&self, error: E) {
        self.error.with_lock(|inner| {
            if inner.is_none() {
                *inner = Some(error);
                self.has_error.store(true, Ordering::Release);
            }
        });
    }

    pub(crate) fn take(&self) -> Option<E> {
        if self.has_error.fetch_and(false, Ordering::Acquire) {
            self.error.with_lock(|inner| inner.take())
        } else {
            None
        }
    }
}
