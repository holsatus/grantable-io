use core::{
    marker::PhantomData,
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    slice::from_raw_parts_mut,
    sync::atomic::Ordering::{Acquire, Release},
};

use crate::loom::loom_const_fn;
use crate::loom::sync::atomic::{AtomicBool, AtomicUsize};

#[derive(Debug)]
/// An atomic coordination structure for safely granting
/// read and write access to contiguous slices of memory.
pub struct AtomicState {
    /// Whether this instance has been initialized
    initialized: AtomicBool,

    /// Cursor marking where the writer last wrapped around; `0` means "not wrapped".
    ///
    /// This cursor is written by *both* halves, so its ownership rules are strict.
    /// A half may only write to `wrapped` while its captured snapshot says it owns it.
    ///
    /// - `Owned by the writer-half iff. wrapped == 0`
    ///     - readable is `[reader, writer)`; free is `[writer, N)` and `[0, reader)`.
    /// - Owned by the reader-half iff. `wrapped != 0`:
    ///     - readable is `[reader, wrapped)` and `[0, writer)` free is `[writer, reader)`.
    ///
    ///
    /// Ordering: [`AtomicState::cursors`] loads `wrapped` **first**, and every
    /// handoff publishes its primary cursor (`reader`/`writer`) before `wrapped`
    /// (all `Release`/`Acquire`), so observing a new `wrapped` implies observing
    /// the cursor published with it.
    wrapped: AtomicUsize,

    /// Where the next byte will be written
    ///
    /// - Owned by the writer-half
    writer: AtomicUsize,

    /// Where the next byte will be read from
    ///
    /// - Owned by the reader-half
    reader: AtomicUsize,

    /// Is there an active read grant?
    read_granted: AtomicBool,

    /// Is there an active write grant?
    write_granted: AtomicBool,
}

impl Default for AtomicState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, PartialEq)]
struct Cursors {
    wrapped: usize,
    writer: usize,
    reader: usize,
}

impl AtomicState {
    loom_const_fn! {
        /// Create a new instance of an [`AtomicState`].
        pub const fn new() -> Self {
            Self {
                initialized: AtomicBool::new(false),
                writer: AtomicUsize::new(0),
                reader: AtomicUsize::new(0),
                wrapped: AtomicUsize::new(0),
                read_granted: AtomicBool::new(false),
                write_granted: AtomicBool::new(false),
            }
        }
    }

    /// Fetch the latest batch of atomic cursors
    #[inline(always)]
    fn cursors(&self) -> Cursors {
        Cursors {
            // The wrapped value MUST be loaded first!
            wrapped: self.wrapped.load(Acquire),
            writer: self.writer.load(Acquire),
            reader: self.reader.load(Acquire),
        }
    }

    /// Get an estimate of the number of writeable bytes in a buffer of length
    /// `len`. The writeable bytes are not necessarily contiguous.
    pub fn writable_bytes(&self, len: usize) -> usize {
        let c = self.cursors();

        let bytes = if c.wrapped == 0 {
            len.saturating_sub(c.writer) + c.reader
        } else {
            c.reader.saturating_sub(c.writer)
        };
        bytes.min(len)
    }

    /// Get an estimate of the number of readable bytes in a buffer of length
    /// `len`. The readable bytes are not necessarily contiguous.
    pub fn readable_bytes(&self, len: usize) -> usize {
        let c = self.cursors();

        let bytes = if c.wrapped == 0 {
            c.writer.saturating_sub(c.reader)
        } else {
            c.wrapped.saturating_sub(c.reader) + c.writer
        };
        bytes.min(len)
    }

    /// Attempt to initialize the [`BufferState`] into [`BufferWriter`] and [`BufferReader`]
    /// halves. If buffer has already been initialized, `None` will be returned.
    pub fn init<'a>(&'a self, buf: &'a mut [u8]) -> Option<(BufferWriter<'a>, BufferReader<'a>)> {
        if self.initialized.swap(true, Release) {
            return None;
        }

        // Only create a pointer from the exclusive reference once
        let ptr = NonNull::from(buf);

        Some((
            BufferWriter {
                buffer: ptr,
                state: self,
            },
            BufferReader {
                buffer: ptr,
                state: self,
            },
        ))
    }
}

/// [`BufferWriter`] is the primary interface for pushing data into a [`crate::GrantableIo`].
#[derive(Debug)]
pub struct BufferWriter<'a> {
    buffer: NonNull<[u8]>,
    state: &'a AtomicState,
}

// SAFETY: `BufferWriter` holds a raw pointer into the shared buffer plus a
// shared reference to `AtomicState`. It is the unique owner of the writer half
// (it is created once and never aliased), and its grants only ever cover free
// space, so moving it to another thread is sound.
unsafe impl Send for BufferWriter<'_> {}

impl<'a> BufferWriter<'a> {
    pub fn writable_bytes(&self) -> usize {
        self.state.writable_bytes(self.buffer.len())
    }

    pub fn readable_bytes(&self) -> usize {
        self.state.readable_bytes(self.buffer.len())
    }

    /// Request a writable contiguous section of memory of at least 1 byte.
    ///
    /// Returns `None` if no space is currently available for writing.
    pub fn try_get_writer_grant(&mut self, buf_len: NonZeroUsize) -> Option<WriterGrant<'a>> {
        if self.state.write_granted.load(Acquire) {
            return None;
        }

        let c = self.state.cursors();

        let (start, grant_len) = if c.wrapped == 0 {
            let space_at_end = self.buffer.len() - c.writer;
            let space_at_start = c.reader;

            // Wrap around if space at start is
            // larger and the buffer will not fit at end
            if space_at_start > space_at_end && buf_len.get() > space_at_end {
                // A wrap is only possible once the buffer is non-empty.
                debug_assert!(c.writer > 0, "wrap-around selected with writer == 0");
                (0, space_at_start)
            } else {
                (c.writer, space_at_end)
            }
        } else {
            (c.writer, c.reader.saturating_sub(c.writer))
        };

        debug_assert!(
            start + grant_len <= self.buffer.len(),
            "The granted region was out of bounds!"
        );

        // Return if we were not granted anything
        if grant_len == 0 {
            return None;
        }

        // The guard at the top can only pass if there are no outstanding grants,
        // which is the only place except for this function where the flag is modified.
        self.state.write_granted.store(true, Release);

        // Construct *unique* mutable slice to the grant
        let buffer = unsafe {
            let base_ptr = self.buffer.cast::<u8>();
            let grant_ptr = base_ptr.add(start).as_ptr();
            from_raw_parts_mut(grant_ptr, grant_len)
        };

        Some(WriterGrant {
            granted: NonNull::from(buffer),
            state: self.state,
            writer: c.writer,
            at_start: start == 0,
            _p: PhantomData,
        })
    }

    #[cfg(test)]
    pub fn try_write(&mut self, buf: &[u8]) -> Option<usize> {
        let buf_len = NonZeroUsize::new(buf.len())?;
        let mut writer = self.try_get_writer_grant(buf_len)?;
        let bytes = writer.copy_max_from(buf);
        writer.commit(bytes);
        Some(bytes)
    }
}

/// [`BufferReader`] is the primary interface for reading data from a [`crate::GrantableIo`]
#[derive(Debug)]
pub struct BufferReader<'a> {
    buffer: NonNull<[u8]>,
    state: &'a AtomicState,
}

// SAFETY: `BufferReader` holds a raw pointer into the shared buffer plus a
// shared reference to `AtomicState`. It is the unique owner of the writer half
// (it is created once and never aliased), and its grants only ever cover free
// space, so moving it to another thread is sound.
unsafe impl Send for BufferReader<'_> {}

impl<'a> BufferReader<'a> {
    pub fn writable_bytes(&self) -> usize {
        self.state.writable_bytes(self.buffer.len())
    }

    pub fn readable_bytes(&self) -> usize {
        self.state.readable_bytes(self.buffer.len())
    }

    /// Obtains a contiguous slice of committed bytes. This slice may not
    /// contain ALL available bytes, if the writer has wrapped around.
    pub fn try_get_reader_grant(&mut self) -> Option<ReaderGrant<'a>> {
        if self.state.read_granted.load(Acquire) {
            return None;
        }

        let c = self.state.cursors();

        let (start, grant_len) = if c.wrapped == 0 {
            (c.reader, c.writer.saturating_sub(c.reader))
        } else if c.reader != c.wrapped {
            (c.reader, c.wrapped.saturating_sub(c.reader))
        } else {
            (0, c.writer)
        };

        debug_assert!(
            start + grant_len <= self.buffer.len(),
            "The granted region was out of bounds!"
        );

        if grant_len == 0 {
            return None;
        }

        // The guard at the top can only pass if there are no outstanding grants,
        // which is the only place except for this function where the flag is modified.
        self.state.read_granted.store(true, Release);

        let buffer = unsafe {
            let base_ptr = self.buffer.cast::<u8>();
            let grant_ptr = base_ptr.add(start).as_ptr();
            from_raw_parts_mut(grant_ptr, grant_len)
        };

        Some(ReaderGrant {
            granted: NonNull::from(buffer),
            state: self.state,
            wrapped: c.wrapped,
            reader: c.reader,
            _p: PhantomData,
        })
    }

    #[cfg(test)]
    pub fn try_read(&mut self, buf: &mut [u8]) -> Option<usize> {
        let mut reader = self.try_get_reader_grant()?;
        let bytes = reader.copy_max_into(buf);
        reader.consume(bytes);
        Some(bytes)
    }
}

/// A granted contiguous region of memory that may be written
/// to and 'committed' so the reader can read from it.
#[derive(Debug)]
pub struct WriterGrant<'a> {
    granted: NonNull<[u8]>,
    state: &'a AtomicState,
    at_start: bool,
    writer: usize,
    _p: PhantomData<&'a mut [u8]>,
}

// SAFETY: a grant is a raw pointer to a freshly-derived unique slice over the
// granted region plus a state reference. At most one writer grant exists at a
// time (enforced by `write_granted`), so moving it to another thread is sound.
unsafe impl Send for WriterGrant<'_> {}

impl WriterGrant<'_> {
    /// Copy the largest possible amount of bytes to the grant
    /// from the given buffer. Whichever is shorter decides the number
    /// of bytes written. The return value is the amount copied.
    pub fn copy_max_from(&mut self, buf: &[u8]) -> usize {
        // Maximum number of bytes that can be copied contiguously
        let amount = self.granted.len().min(buf.len());

        // Copy `amount` bytes from `grant` to `buf`
        self[..amount].copy_from_slice(&buf[..amount]);

        // The number copied
        amount
    }

    /// Finalizes this writable grant and makes `used` bytes of written data
    /// available for subsequent reading grants. This consumes the grant.
    pub fn commit(self, used: usize) {
        let Some(used) = NonZeroUsize::new(used) else {
            return;
        };

        // Saturate the amount to commit
        let used = self.granted.len().min(used.get());
        let s = self.state;

        // Determine where to move the write cursor
        if self.at_start {
            s.writer.store(used, Release);
            s.wrapped.store(self.writer, Release);
        } else {
            s.writer.store(self.writer + used, Release);
        };

        drop(self) // Redundant, but for clarity
    }
}

impl Drop for WriterGrant<'_> {
    fn drop(&mut self) {
        self.state.write_granted.store(false, Release);
    }
}

impl Deref for WriterGrant<'_> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { self.granted.as_ref() }
    }
}

impl DerefMut for WriterGrant<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.granted.as_mut() }
    }
}

/// A granted contiguous region of memory that may be read
/// from and 'consumed' so the writer can write to it agian.
#[derive(Debug)]
pub struct ReaderGrant<'a> {
    granted: NonNull<[u8]>,
    state: &'a AtomicState,
    wrapped: usize,
    reader: usize,
    _p: PhantomData<&'a mut [u8]>,
}

// SAFETY: a grant is a raw pointer to a freshly-derived unique slice over the
// granted region plus a state reference. At most one writer grant exists at a
// time (enforced by `read_granted`), so moving it to another thread is sound.
unsafe impl Send for ReaderGrant<'_> {}

impl ReaderGrant<'_> {
    /// Copy the largest possible amount of bytes from the grant
    /// to the given buffer. Whichever is shorter decides the number
    /// of bytes written. The return value is the amount copied.
    pub fn copy_max_into(&mut self, buf: &mut [u8]) -> usize {
        // Maximum number of bytes that can be copied contiguously
        let amount = self.granted.len().min(buf.len());

        // Copy `amount` bytes from `grant` to `buf`
        buf[..amount].copy_from_slice(&self[..amount]);

        // The number copied
        amount
    }

    /// Finalizes this readable grant and makes `used` bytes of read data
    /// available for subsequent writing grants. This consumes the grant.
    pub fn consume(self, used: usize) {
        let Some(used) = NonZeroUsize::new(used) else {
            return;
        };

        // Saturate the grant consume
        let used = self.granted.len().min(used.get());
        let s = self.state;

        // If we were previously caught up with the wrapped value,
        // the current read consumes from the beginning of the buffer.
        if self.reader == self.wrapped {
            // Consuming from start segment
            s.reader.store(used, Release);
            if self.wrapped != 0 {
                s.wrapped.store(0, Release);
            }
        } else {
            // Non-wrapped progress
            let next_reader = self.reader + used;
            if self.wrapped == 0 || next_reader < self.wrapped {
                s.reader.store(next_reader, Release);
            } else {
                // Finished end of buffer up to wrapped
                s.reader.store(0, Release);
                s.wrapped.store(0, Release);
            }
        }

        drop(self) // Redundant, but for clarity
    }
}

impl Drop for ReaderGrant<'_> {
    fn drop(&mut self) {
        self.state.read_granted.store(false, Release);
    }
}

impl Deref for ReaderGrant<'_> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { self.granted.as_ref() }
    }
}

impl DerefMut for ReaderGrant<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.granted.as_mut() }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{AtomicState, Cursors};

    fn nz(num: usize) -> core::num::NonZeroUsize {
        core::num::NonZeroUsize::new(num).unwrap()
    }

    #[test]
    fn catch_double_init() {
        let state = AtomicState::new();

        let mut buffer0 = [0u8; 8];
        let mut buffer1 = [0u8; 8];

        assert!(state.init(buffer0.as_mut()).is_some());
        assert!(state.init(buffer1.as_mut()).is_none());
    }

    #[test]
    fn refuse_double_grant() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut w, mut r) = state.init(buffer.as_mut()).unwrap();

        let grant0 = w.try_get_writer_grant(nz(8));
        let grant1 = w.try_get_writer_grant(nz(8));
        assert!(grant0.is_some());
        assert!(grant1.is_none());

        drop((grant0, grant1));
        w.try_write(b"abcd").unwrap();

        let grant0 = r.try_get_reader_grant();
        let grant1 = r.try_get_reader_grant();
        assert!(grant0.is_some());
        assert!(grant1.is_none());
    }

    #[test]
    fn write_read_entire_buffer() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut w, mut r) = state.init(buffer.as_mut()).unwrap();

        // Nothing to read initially
        assert!(r.try_get_reader_grant().is_none());
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 0,
                reader: 0
            }
        );

        let payload = b"abcdefgh";
        let mut read_buf = [0u8; 16];

        // Write to the buffer
        w.try_write(payload).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 8,
                reader: 0
            }
        );

        // Read from the buffer
        let bytes = r.try_read(&mut read_buf).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 8,
                reader: 8
            }
        );
        assert_eq!(&read_buf[..bytes], payload);

        // Write to the buffer (again)
        w.try_write(payload).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 8,
                writer: 8,
                reader: 8
            }
        );

        // Read from the buffer
        let bytes = r.try_read(&mut read_buf).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 8,
                reader: 8
            }
        );
        assert_eq!(&read_buf[..bytes], payload);

        // Nothing to read after
        assert!(r.try_get_reader_grant().is_none());
    }

    #[test]
    fn readable_bytes_is_a_lower_bound() {
        // Force a wrapped layout so readable spans both segments, then verify
        // that `readable_bytes()` bytes can all be read without a zero-length
        // read (i.e. without the writer having to produce more).
        let mut buffer = [0u8; 10];
        let state = AtomicState::new();
        let (mut w, mut r) = state.init(buffer.as_mut()).unwrap();

        w.try_write(&[b'a'; 8]).unwrap();
        let mut sink = [0u8; 6];
        assert_eq!(r.try_read(&mut sink).unwrap(), 6);

        w.try_write(&[b'b'; 2]).unwrap(); // fill tail -> writer = 10
        w.try_write(&[b'c'; 6]).unwrap(); // wrap -> writer = 6, wrapped = 10

        let n = r.readable_bytes();
        assert_eq!(n, 10);

        let mut got = 0;
        let mut out = [0u8; 10];
        while got < n {
            let k = r.try_read(&mut out[got..]).unwrap();
            assert!(k > 0, "reader could not read {n} guaranteed bytes");
            got += k;
        }
        assert_eq!(got, n);
    }

    #[test]
    fn writable_bytes_is_total_free_space() {
        // `writable_bytes` counts the total free space across both segments.
        // A single large request can wrap early and strand the tail, so it must
        // be claimed segment-by-segment to realize the whole amount.
        let mut buffer = [0u8; 10];
        let state = AtomicState::new();
        let (mut w, mut r) = state.init(buffer.as_mut()).unwrap();

        w.try_write(&[0u8; 8]).unwrap();
        let mut sink = [0u8; 6];
        assert_eq!(r.try_read(&mut sink).unwrap(), 6);

        // writer = 8, reader = 6 -> free = [8,10) + [0,6) = 8
        assert_eq!(w.writable_bytes(), 8);

        // Tail first, then the wrap into the start segment: all 8 bytes.
        assert_eq!(w.try_write(&[0u8; 2]).unwrap(), 2);
        assert_eq!(w.try_write(&[0u8; 6]).unwrap(), 6);
        assert_eq!(w.writable_bytes(), 0);
    }

    #[test]
    fn small_wrapping_write_read() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut w, mut r) = state.init(buffer.as_mut()).unwrap();

        let mut read_buf = [0u8; 16];

        assert!(r.try_get_reader_grant().is_none());
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 0,
                reader: 0
            }
        );

        // Write to the buffer
        let payload = b"abcdef";
        w.try_write(payload).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 6,
                reader: 0
            }
        );

        // Read from the buffer
        let bytes = r.try_read(&mut read_buf).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 6,
                reader: 6
            }
        );
        assert_eq!(&read_buf[..bytes], payload);

        // Write to the buffer
        let payload = b"abcd";
        w.try_write(payload).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 6,
                writer: 4,
                reader: 6
            }
        );

        // Read from the buffer
        let bytes = r.try_read(&mut read_buf).unwrap();
        assert_eq!(
            state.cursors(),
            Cursors {
                wrapped: 0,
                writer: 4,
                reader: 4
            }
        );
        assert_eq!(&read_buf[..bytes], payload);

        // Nothing to read after
        assert!(r.try_get_reader_grant().is_none());
    }
}

/// Loom model tests.
///
/// Run with:
///
/// ```text
/// RUSTFLAGS="--cfg loom" cargo test --lib -- --test-threads=1
/// ```
///
/// Under `--cfg loom`, `AtomicState` uses Loom's instrumented atomics, so Loom
/// exhaustively explores the thread interleavings of the two halves within the
/// configured bounds. These are the authoritative, deterministic regression
/// tests for the `wrapped` ownership contract.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::AtomicState;
    use crate::loom::{model, thread};
    use core::num::NonZeroUsize;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    /// Regression test for a previously observed lost `wrapped` update.
    ///
    /// Before the guard in [`super::ReaderGrant::consume`], the reader cleared
    /// `wrapped` even when it had captured `wrapped == 0`, racing with a
    /// concurrent writer wrap. The two halves then disagree about how many bytes
    /// are readable; the writer can overwrite bytes the reader has not yet seen
    /// and the inconsistent cursor state can deadlock both halves.
    ///
    /// The invariant checked is exact: the cursor state's own
    /// [`AtomicState::readable_bytes`] must equal the bytes committed minus the
    /// bytes consumed, tracked independently by each thread.
    #[test]
    fn wrapped_not_clobbered_by_reader_consume() {
        model(|| {
            const N: usize = 1;

            // Tests may leak: Loom re-runs this closure per schedule and we need
            // `'static` borrows for `loom::thread::spawn`.
            let state: &'static AtomicState = Box::leak(Box::new(AtomicState::new()));
            let buf: &'static mut [u8] = Box::leak(vec![0u8; N].into_boxed_slice());
            let (mut writer, mut reader) = state.init(buf).unwrap();

            let wt = thread::spawn(move || {
                let mut produced = 0usize;
                // A wrap only becomes possible after the reader has drained, so
                // retry a bounded number of times to let Loom interleave.
                for _ in 0..4 {
                    if let Some(mut grant) = writer.try_get_writer_grant(nz(1)) {
                        let n = grant.copy_max_from(&[0xAB]);
                        grant.commit(n);
                        produced += n;
                    }
                }
                produced
            });

            let rt = thread::spawn(move || {
                let mut consumed = 0usize;
                for _ in 0..4 {
                    if let Some(mut grant) = reader.try_get_reader_grant() {
                        let mut scratch = [0u8; N];
                        let n = grant.copy_max_into(&mut scratch);
                        grant.consume(n);
                        consumed += n;
                    }
                }
                consumed
            });

            let produced = wt.join().unwrap();
            let consumed = rt.join().unwrap();

            assert_eq!(
                state.readable_bytes(N),
                produced - consumed,
                "cursor state disagrees with committed-minus-consumed bytes \
                 (lost update on `wrapped`)"
            );
        });
    }

    /// Same invariant, but with a slightly larger buffer and multi-byte grants
    /// so more wrap sequences are explored.
    #[test]
    fn wrapped_invariant_holds_under_wrap_churn() {
        model(|| {
            const N: usize = 3;

            let state: &'static AtomicState = Box::leak(Box::new(AtomicState::new()));
            let buf: &'static mut [u8] = Box::leak(vec![0u8; N].into_boxed_slice());
            let (mut writer, mut reader) = state.init(buf).unwrap();

            let wt = thread::spawn(move || {
                let mut produced = 0usize;
                for _ in 0..5 {
                    if let Some(mut grant) = writer.try_get_writer_grant(nz(2)) {
                        let n = grant.copy_max_from(&[1, 2]);
                        grant.commit(n);
                        produced += n;
                    }
                }
                produced
            });

            let rt = thread::spawn(move || {
                let mut consumed = 0usize;
                for _ in 0..5 {
                    if let Some(mut grant) = reader.try_get_reader_grant() {
                        let mut scratch = [0u8; N];
                        let n = grant.copy_max_into(&mut scratch);
                        grant.consume(n);
                        consumed += n;
                    }
                }
                consumed
            });

            let produced = wt.join().unwrap();
            let consumed = rt.join().unwrap();

            assert_eq!(state.readable_bytes(N), produced - consumed);
        });
    }
}
