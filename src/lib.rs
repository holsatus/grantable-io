use std::cell::UnsafeCell;

use maitake_sync::WaitCell;

mod error;
use error::AtomicErrorKind;

mod grant;
pub use grant::{GrantReader, GrantWriter};

mod hard;
pub use hard::{HardReader, HardWriter};

mod soft;
use portable_atomic::AtomicBool;
pub use soft::{BufReadExt, SoftReader, SoftWriter};

mod buffer;
pub use buffer::BufferState;

/// Internal state to back a [`SerialWriter`] or [`SerialReader`] connection
///
/// The usage requires a unique statically allocated instance [`SerialState`]
/// instance.
///
/// # Example
/// ```rust
/// use portable_io::{SerialState, SerialWriter, StaticBuffer};
///
/// static STATE: SerialState = SerialState::new();
/// static BUFFER: StaticBuffer<128> = StaticBuffer::new();
/// let serial = SerialWriter::new(&STATE, BUFFER.take()).unwrap();
/// ```
///
/// Alternatively, consider using the macro
///
/// ```rust
/// let serial = portable_io::new_serial_writer!(128);
///
/// // split into reading and writing halves
/// let (writer, reader) = serial.split();
/// ```
pub struct SerialState {
    buffer: BufferState,
    wait_writer: WaitCell,
    wait_reader: WaitCell,
    error: AtomicErrorKind,
}

impl SerialState {
    /// Creates a new [`SerialState`] instance.
    ///
    /// This initializes the internal buffer and synchronization primitives required
    /// for serial communication.
    ///
    /// # Example
    ///
    /// ```rust
    /// use portable_io::SerialState;
    ///
    /// static STATE: SerialState = SerialState::new();
    /// ```
    pub const fn new() -> Self {
        Self {
            buffer: BufferState::new(),
            wait_writer: WaitCell::new(),
            wait_reader: WaitCell::new(),
            error: AtomicErrorKind::new(),
        }
    }
}

impl Default for SerialState {
    fn default() -> Self {
        SerialState::new()
    }
}

/// A [`SerialWriter`] is used to buffer an async TX serial connection.
///
/// This struct provides access to both the hardware-facing and software-facing
/// ends of the serial connection. It allows splitting into separate hardware
/// and software writers.
///
/// # Example
///
/// ```rust
/// use portable_io::{SerialState, SerialWriter, StaticBuffer};
///
/// static STATE: SerialState = SerialState::new();
/// static BUFFER: StaticBuffer<128> = StaticBuffer::new();
/// let serial = SerialWriter::new(&STATE, BUFFER.take()).unwrap();
///
/// // Split into hardware and software writers
/// let (hardware, software) = serial.split();
/// ```
pub struct SerialWriter<'a> {
    pub software: soft::SoftWriter<'a>,
    pub hardware: hard::HardReader<'a>,
}

impl<'a> SerialWriter<'a> {
    /// Creates a new [`SerialWriter`] instance.
    ///
    /// This initializes the hardware and software writers using the provided
    /// [`SerialState`].
    ///
    /// # Parameters
    ///
    /// - `state`: A reference to a statically allocated [`SerialState`] instance.
    ///
    /// # Returns
    ///
    /// - `Some(SerialWriter)` if the buffer could be successfully split.
    /// - `None` if the buffer could not be split.
    ///
    /// # Example
    ///
    /// ```rust
    /// use portable_io::{SerialState, SerialWriter, StaticBuffer};
    ///
    /// static STATE: SerialState = SerialState::new();
    /// static BUFFER: StaticBuffer<128> = StaticBuffer::new();
    /// let serial = SerialWriter::new(&STATE, BUFFER.take()).unwrap();
    /// ```
    pub fn new(state: &'a SerialState, buf: &'a mut [u8]) -> Option<SerialWriter<'a>> {
        let (producer, consumer) = state.buffer.init(buf)?;

        let software = soft::SoftWriter { producer, state };
        let hardware = hard::HardReader { consumer, state };

        Some(SerialWriter { software, hardware })
    }

    /// Splits the [`SerialWriter`] into its hardware and software components.
    ///
    /// This allows separate access to the hardware-facing and software-facing
    /// ends of the serial connection.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - `SoftWriter`: The software-facing writer.
    /// - `HardReader`: The hardware-facing reader.
    ///
    /// # Example
    ///
    /// ```rust
    /// use portable_io::{SerialState, SerialWriter, StaticBuffer};
    ///
    /// static STATE: SerialState = SerialState::new();
    /// static BUFFER: StaticBuffer<128> = StaticBuffer::new();
    /// let serial = SerialWriter::new(&STATE, BUFFER.take()).unwrap();
    ///
    /// let (hardware, software) = serial.split();
    /// ```
    pub fn split(self) -> (soft::SoftWriter<'a>, hard::HardReader<'a>) {
        let SerialWriter { software, hardware } = self;
        (software, hardware)
    }
}

/// Creates a new [`SerialWriter`] instance with a statically allocated [`SerialState`].
///
/// This macro simplifies the creation of a [`SerialWriter`] by automatically
/// allocating the required [`SerialState`] instance.
///
/// # Parameters
///
/// - `$n`: The size of the buffer.
///
/// # Example
///
/// ```rust
/// let serial = portable_io::new_serial_writer!(128);
///
/// // Split into hardware and software writers
/// let (hardware, software) = serial.split();
/// ```
#[macro_export]
macro_rules! new_serial_writer {
    ($n:literal) => {{
        use $crate::{SerialState, SerialWriter, StaticBuffer};
        static STATE: SerialState = SerialState::new();
        static BUFFER: StaticBuffer<$n> = StaticBuffer::new();
        SerialWriter::new(&STATE, BUFFER.take()).unwrap()
    }};
}

/// A [`SerialReader`] is used to buffer an async RX serial connection.
///
/// This struct provides access to both the hardware-facing and software-facing
/// ends of the serial connection. It allows splitting into separate hardware
/// and software readers.
///
/// # Example
///
/// ```rust
/// use portable_io::{SerialState, SerialReader, StaticBuffer};
///
/// static STATE: SerialState = SerialState::new();
/// static BUFFER: StaticBuffer<128> = StaticBuffer::new();
/// let serial = SerialReader::new(&STATE, BUFFER.take()).unwrap();
///
/// // Split into hardware and software readers
/// let (hardware, software) = serial.split();
/// ```
pub struct SerialReader<'a> {
    pub hardware: hard::HardWriter<'a>,
    pub software: soft::SoftReader<'a>,
}

impl<'a> SerialReader<'a> {
    /// Creates a new [`SerialReader`] instance.
    ///
    /// This initializes the hardware and software readers using the provided
    /// [`SerialState`].
    ///
    /// # Parameters
    ///
    /// - `state`: A reference to a statically allocated [`SerialState`] instance.
    ///
    /// # Returns
    ///
    /// - `Some(SerialReader)` if the buffer could be successfully split.
    /// - `None` if the buffer could not be split.
    ///
    /// # Example
    ///
    /// ```rust
    /// use portable_io::{SerialState, SerialReader, StaticBuffer};
    ///
    /// static STATE: SerialState = SerialState::new();
    /// static BUFFER: StaticBuffer<128> = StaticBuffer::new();
    /// let serial = SerialReader::new(&STATE, BUFFER.take()).unwrap();
    /// ```
    pub fn new(state: &'a SerialState, buf: &'a mut [u8]) -> Option<SerialReader<'a>> {
        let (producer, consumer) = state.buffer.init(buf)?;

        let hardware = hard::HardWriter { producer, state };

        let software = soft::SoftReader {
            consumer,
            state,
            grant: None,
        };

        Some(SerialReader { hardware, software })
    }

    /// Splits the [`SerialReader`] into its hardware and software components.
    ///
    /// This allows separate access to the hardware-facing and software-facing
    /// ends of the serial connection.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - `HardWriter`: The hardware-facing writer.
    /// - `SoftReader`: The software-facing reader.
    ///
    /// # Example
    ///
    /// ```rust
    /// use portable_io::{SerialState, SerialReader, StaticBuffer};
    ///
    /// static STATE: SerialState = SerialState::new();
    /// static BUFFER: StaticBuffer<128> = StaticBuffer::new();
    /// let serial = SerialReader::new(&STATE, BUFFER.take()).unwrap();
    ///
    /// let (hardware, software) = serial.split();
    /// ```
    pub fn split(self) -> (hard::HardWriter<'a>, soft::SoftReader<'a>) {
        let SerialReader { hardware, software } = self;
        (hardware, software)
    }
}

/// Creates a new [`SerialReader`] instance with a statically allocated [`SerialState`].
///
/// This macro simplifies the creation of a [`SerialReader`] by automatically
/// allocating the required [`SerialState`] instance.
///
/// # Parameters
///
/// - `$n`: The size of the buffer.
///
/// # Example
///
/// ```rust
/// let serial = portable_io::new_serial_reader!(128);
///
/// // Split into hardware and software readers
/// let (hardware, software) = serial.split();
/// ```
#[macro_export]
macro_rules! new_serial_reader {
    ($n:literal) => {{
        use $crate::{SerialReader, SerialState, StaticBuffer};
        static STATE: SerialState = SerialState::new();
        static BUFFER: StaticBuffer<$n> = StaticBuffer::new();
        SerialReader::new(&STATE, BUFFER.take()).unwrap()
    }};
}

/// Used for statically allocating a buffer that requires a mutable reference
pub struct StaticBuffer<const N: usize> {
    claimed: AtomicBool,
    buf: UnsafeCell<[u8; N]>,
}

unsafe impl<const N: usize> Send for StaticBuffer<N> {}
unsafe impl<const N: usize> Sync for StaticBuffer<N> {}

impl<const N: usize> Default for StaticBuffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> StaticBuffer<N> {
    pub const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            buf: UnsafeCell::new([0u8; N]),
        }
    }

    pub fn take(&self) -> &mut [u8; N] {
        self.try_take()
            .expect("`StaticBuffer` is already taken, it can't be taken twice")
    }

    pub fn try_take(&self) -> Option<&mut [u8; N]> {
        use core::sync::atomic::Ordering::{Acquire, Relaxed};

        // SAFETY: We check that the value is not yet taken and marked it as taken.
        self.claimed
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_ok()
            .then(|| unsafe { &mut *self.buf.get() })
    }
}
