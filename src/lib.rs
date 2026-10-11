#![cfg_attr(not(test), no_std)]

// At most one `embedded-io-async` line may be selected at a time. Emit a clear
// error rather than letting Cargo fail with "depends on ... multiple times".
#[cfg(all(feature = "embedded-io-async-07", feature = "embedded-io-async-06"))]
compile_error!(
    "features `embedded-io-async-07` and `embedded-io-async-06` are mutually \
     exclusive; enable exactly one"
);

use core::{cell::UnsafeCell, num::NonZeroUsize};

mod loom;

use loom::loom_const_fn;
use maitake_sync::WaitCell;
use portable_atomic::AtomicBool;

mod dev;
pub use dev::{DeviceReader, DeviceWriter};

mod app;
pub use app::{Reader, Writer};

mod buffer;
use buffer::{AtomicState, BufferReader, BufferWriter};

mod error;
use error::AtomicError;

#[derive(Debug)]
struct State<E> {
    pub(crate) error: AtomicError<E>,
    pub(crate) state: AtomicState,
    pub(crate) wait_writer: WaitCell,
    pub(crate) wait_reader: WaitCell,
}

impl<E> State<E> {
    loom_const_fn! {
        const fn new() -> Self {
            Self {
                error: AtomicError::new(),
                state: AtomicState::new(),
                wait_writer: WaitCell::new(),
                wait_reader: WaitCell::new(),
            }
        }
    }
}

/// A structure that owns the shared state and the buffer for serial communication.
#[repr(C, align(4))]
pub struct GrantableIo<const N: usize, E> {
    buffer: UnsafeCell<[u8; N]>,
    initialized: AtomicBool,
    state: State<E>,
}

unsafe impl<const N: usize, E: Send> Send for GrantableIo<N, E> {}
unsafe impl<const N: usize, E: Send> Sync for GrantableIo<N, E> {}

impl<const N: usize, E> GrantableIo<N, E> {
    loom_const_fn! {
        /// Creates a new, uninitialized GrantableIo.
        pub const fn new() -> Self {
            assert!(N > 0, "The value of `N` must be non-zero");
            Self {
                buffer: UnsafeCell::new([0; N]),
                initialized: AtomicBool::new(false),
                state: State::new(),
            }
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

        self.state.state.init(buffer)
    }

    #[track_caller]
    pub fn claim_reader(&self) -> (DeviceWriter<'_, E>, Reader<'_, E>) {
        self.try_claim_reader()
            .expect("GrantableIo already claimed, cannot be claimed again")
    }

    /// Try to claim this [`GrantableIo`] as a reading stream.
    pub fn try_claim_reader(&self) -> Option<(DeviceWriter<'_, E>, Reader<'_, E>)> {
        let (writer, reader) = self.try_claim_inner()?;

        Some((
            DeviceWriter {
                writer,
                state: &self.state,
            },
            Reader {
                reader,
                state: &self.state,
                grant: None,
            },
        ))
    }

    #[track_caller]
    pub fn claim_writer(&self) -> (DeviceReader<'_, E>, Writer<'_, E>) {
        self.try_claim_writer()
            .expect("GrantableIo already claimed, cannot be claimed again")
    }

    pub fn try_claim_writer(&self) -> Option<(DeviceReader<'_, E>, Writer<'_, E>)> {
        let (writer, reader) = self.try_claim_inner()?;

        Some((
            DeviceReader {
                reader,
                state: &self.state,
            },
            Writer {
                writer,
                state: &self.state,
                grant: None,
            },
        ))
    }
}

/// Wrap-around threshold for writer grants, as a number of bytes.
const BUF_LEN_GRACE: NonZeroUsize = NonZeroUsize::new(32).unwrap();

#[cfg(feature = "embedded-io-async-07")]
pub use embedded_io_async_07 as embedded_io_async;

#[cfg(feature = "embedded-io-async-06")]
pub use embedded_io_async_06 as embedded_io_async;
