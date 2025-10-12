#![no_std]

use core::cell::UnsafeCell;
use portable_atomic::AtomicBool;
use maitake_sync::WaitCell;

mod grant;
pub use grant::{GrantReader, GrantWriter};

mod dev;
pub use dev::{DevProducer, DevConsumer};

mod app;
pub use app::{AppConsumer, AppProducer};

mod buffer;
use buffer::{AtomicBuffer, Consumer, Producer};

mod error;
use error::AtomicError;

struct SerialState<E> {
    pub(crate) buffer: AtomicBuffer,
    pub(crate) error: AtomicError<E>,
    pub(crate) wait_writer: WaitCell,
    pub(crate) wait_reader: WaitCell,
}

impl<E> SerialState<E> {
    const fn new() -> Self {
        Self {
            buffer: AtomicBuffer::new(),
            error: AtomicError::new(),
            wait_writer: WaitCell::new(),
            wait_reader: WaitCell::new(),
        }
    }
}

/// A structure that owns the shared state and the buffer for serial communication.
pub struct SerialPort<const N: usize, E> {
    state: SerialState<E>,
    buffer: UnsafeCell<[u8; N]>,
    initialized: AtomicBool,
}

unsafe impl<const N: usize, E> Send for SerialPort<N, E> {}
unsafe impl<const N: usize, E> Sync for SerialPort<N, E> {}

impl<const N: usize, E> SerialPort<N, E> {
    /// Creates a new, uninitialized SerialPort.
    pub const fn new() -> Self {
        assert!(N > 0, "The value of `N` must be non-zero");        
        Self {
            state: SerialState::new(),
            buffer: UnsafeCell::new([0; N]),
            initialized: AtomicBool::new(false),
        }
    }

    fn try_claim_inner(&self) -> Option<(Producer<'_>, Consumer<'_>)> {
        use core::sync::atomic::Ordering::AcqRel;

        // Ensure this is only called once
        if self.initialized.swap(true, AcqRel) {
            return None;
        }

        // SAFETY: Due to the above check, we will only ever claim the
        // buffer once, and use it with this particular buffer state.
        let buffer = unsafe { &mut *self.buffer.get() };

        self.state.buffer.init(buffer)
    }

    #[track_caller]
    pub fn claim_reader(&self) -> (DevProducer<'_, E>, AppConsumer<'_, E>) {
        self.try_claim_reader()
            .expect("SerialPort already claimed, cannot be claimed again")
    }

    pub fn try_claim_reader(&self) -> Option<(DevProducer<'_, E>, AppConsumer<'_, E>)> {
        let (producer, consumer) = self.try_claim_inner()?;

        Some((
            DevProducer {
                producer,
                state: &self.state,
            },
            AppConsumer {
                consumer,
                state: &self.state,
                grant: None,
            },
        ))
    }

    #[track_caller]
    pub fn claim_writer(&self) -> (DevConsumer<'_, E>, AppProducer<'_, E>) {
        self.try_claim_writer()
            .expect("SerialPort already claimed, cannot be claimed again")
    }

    pub fn try_claim_writer(&self) -> Option<(DevConsumer<'_, E>, AppProducer<'_, E>)> {
        let (producer, consumer) = self.try_claim_inner()?;

        Some((
            DevConsumer {
                consumer,
                state: &self.state,
            },
            AppProducer {
                producer,
                state: &self.state,
            },
        ))
    }
}
