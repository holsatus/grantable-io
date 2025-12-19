#![no_std]

use core::cell::UnsafeCell;
use maitake_sync::WaitCell;
use portable_atomic::AtomicBool;

mod dev;
pub use dev::{DeviceReader, DeviceWriter};

mod app;
pub use app::{Reader, Writer};

mod buffer;
use buffer::{AtomicBuffer, BufferReader, BufferWriter};

mod error;
use error::AtomicError;

#[derive(Debug)]
struct State<E> {
    pub(crate) error: AtomicError<E>,
    pub(crate) atomic: AtomicBuffer,
    pub(crate) wait_writer: WaitCell,
    pub(crate) wait_reader: WaitCell,
}

impl<E> State<E> {
    const fn new() -> Self {
        Self {
            error: AtomicError::new(),
            atomic: AtomicBuffer::new(),
            wait_writer: WaitCell::new(),
            wait_reader: WaitCell::new(),
        }
    }
}

/// A structure that owns the shared state and the buffer for serial communication.
pub struct GrantableIo<const N: usize, E> {
    state: State<E>,
    buffer: UnsafeCell<[u8; N]>,
    initialized: AtomicBool,
}

unsafe impl<const N: usize, E> Send for GrantableIo<N, E> {}
unsafe impl<const N: usize, E> Sync for GrantableIo<N, E> {}

impl<const N: usize, E> GrantableIo<N, E> {
    /// Creates a new, uninitialized SerialPort.
    pub const fn new() -> Self {
        assert!(N > 0, "The value of `N` must be non-zero");
        Self {
            state: State::new(),
            buffer: UnsafeCell::new([0; N]),
            initialized: AtomicBool::new(false),
        }
    }

    fn try_claim_inner(&self) -> Option<(BufferWriter<'_>, BufferReader<'_>)> {
        use core::sync::atomic::Ordering::AcqRel;

        // Ensure this is only called once
        if self.initialized.swap(true, AcqRel) {
            return None;
        }

        // SAFETY: Due to the above check, we will only ever claim the
        // buffer once, and use it with this particular buffer state.
        let buffer = unsafe { &mut *self.buffer.get() };

        self.state.atomic.init(buffer)
    }

    #[track_caller]
    pub fn claim_reader(&self) -> (DeviceWriter<'_, E>, Reader<'_, E>) {
        self.try_claim_reader()
            .expect("SerialPort already claimed, cannot be claimed again")
    }

    /// Try to claim this [`GrantableIo`] as a reading stream.
    pub fn try_claim_reader(&self) -> Option<(DeviceWriter<'_, E>, Reader<'_, E>)> {
        let (producer, consumer) = self.try_claim_inner()?;

        Some((
            DeviceWriter {
                producer,
                state: &self.state,
            },
            Reader {
                consumer,
                state: &self.state,
                grant: None,
            },
        ))
    }

    #[track_caller]
    pub fn claim_writer(&self) -> (DeviceReader<'_, E>, Writer<'_, E>) {
        self.try_claim_writer()
            .expect("SerialPort already claimed, cannot be claimed again")
    }

    pub fn try_claim_writer(&self) -> Option<(DeviceReader<'_, E>, Writer<'_, E>)> {
        let (producer, consumer) = self.try_claim_inner()?;

        Some((
            DeviceReader {
                consumer,
                state: &self.state,
            },
            Writer {
                producer,
                state: &self.state,
                grant: None,
            },
        ))
    }
}

#[cfg(feature = "embedded-io-async-070")]
pub use embedded_io_async_070 as embedded_io_async;

#[cfg(feature = "embedded-io-async-061")]
pub use embedded_io_async_061 as embedded_io_async;

#[cfg(feature = "embedded-io-async-060")]
pub use embedded_io_async_060 as embedded_io_async;
