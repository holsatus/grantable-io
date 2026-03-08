use core::sync::atomic::Ordering;

use maitake_sync::blocking::Mutex;
use portable_atomic::AtomicBool;

#[derive(Debug)]
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
            *inner = Some(error);
            self.has_error.store(true, Ordering::Release);
        });
    }

    pub(crate) fn take(&self) -> Option<E> {
        if !self.has_error.load(Ordering::Acquire) {
            return None;
        }

        self.error.with_lock(|inner| {
            self.has_error.store(false, Ordering::Release);
            inner.take()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_take_then_set() {
        static ERROR: AtomicError<&'static str> = AtomicError::new();

        assert_eq!(ERROR.take(), None);

        ERROR.set("The first");

        assert_eq!(ERROR.take(), Some("The first"));
        assert_eq!(ERROR.take(), None);

        ERROR.set("The forgotten");
        ERROR.set("The finisher");

        assert_eq!(ERROR.take(), Some("The finisher"));
        assert_eq!(ERROR.take(), None);
    }
}
