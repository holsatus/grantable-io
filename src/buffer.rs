use core::{
    marker::PhantomData,
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    slice::from_raw_parts_mut,
    sync::atomic::Ordering::{Acquire, Release},
};

use portable_atomic::{AtomicBool, AtomicUsize};

#[derive(Debug)]
/// An atomic coordination structure for safely granting
/// read and write access to contiguous slices of memory.
pub struct AtomicState {
    /// Whether this instance has been initialized
    initialized: AtomicBool,

    /// Where the next byte will be written
    ///
    /// - Owned by the writer-half
    writer: AtomicUsize,

    /// Where the next byte will be read from
    ///
    /// - Owned by the reader-half
    reader: AtomicUsize,

    /// Where the writer has wrapped around if != 0
    ///
    /// - Owned by writer-half if `wrapped == 0`
    /// - Owned by reader-half if `wrapped != 0`
    wrapped: AtomicUsize,

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

struct Cursors {
    wrapped: usize,
    writer: usize,
    reader: usize,
}

impl AtomicState {
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

    /// Create a new instance of an [`BufferState`].
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

    /// Get the number of writeable bytes in the buffer of length `len`.
    ///
    /// Not all bytes may not be contiguous
    pub fn writable_bytes(&self, len: usize) -> usize {
        let c = self.cursors();

        if c.wrapped == 0 {
            len.saturating_sub(c.writer) + c.reader
        } else {
            c.reader.saturating_sub(c.writer)
        }
    }

    /// Get the number of readable bytes in the buffer.
    ///
    /// Not all bytes may not be contiguous
    pub fn readable_bytes(&self) -> usize {
        let c = self.cursors();

        if c.wrapped == 0 {
            c.writer.saturating_sub(c.reader)
        } else {
            c.wrapped.saturating_sub(c.reader) + c.writer
        }
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

unsafe impl Send for BufferWriter<'_> {}

impl<'a> BufferWriter<'a> {
    pub fn writable_bytes(&self) -> usize {
        self.state.writable_bytes(self.buffer.len())
    }

    pub fn readable_bytes(&self) -> usize {
        self.state.readable_bytes()
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
                (0, space_at_start)
            } else {
                (c.writer, space_at_end)
            }
        } else {
            (c.writer, c.reader.saturating_sub(c.writer))
        };

        debug_assert!(start + grant_len <= self.buffer.len());

        // Return if we were not granted anything
        if grant_len == 0 {
            return None;
        }

        // The guard above can only pass if there are no outstanding grants, which
        // is the only place except for this function where the flag is modified.
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
}

/// [`BufferReader`] is the primary interface for reading data from a [`crate::GrantableIo`]
#[derive(Debug)]
pub struct BufferReader<'a> {
    buffer: NonNull<[u8]>,
    state: &'a AtomicState,
}

unsafe impl Send for BufferReader<'_> {}

impl<'a> BufferReader<'a> {
    pub fn writable_bytes(&self) -> usize {
        self.state.writable_bytes(self.buffer.len())
    }

    pub fn readable_bytes(&self) -> usize {
        self.state.readable_bytes()
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

        debug_assert!(start + grant_len <= self.buffer.len());

        if grant_len == 0 {
            return None;
        }

        // The guard above can only pass if there are no outstanding grants, which
        // is the only place except for this function where the flag is modified.
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

        // If we were previously caught up with the != 0 wrapped value,
        // the current read consumes from the beginning of the buffer.
        if self.wrapped != 0 && self.reader == self.wrapped {
            // Consuming from start segment
            s.reader.store(used, Release);
            s.wrapped.store(0, Release);
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

#[cfg(test)]
mod tests {

    use rand::RngExt;
    use std::{num::NonZeroUsize, time::Duration};

    use super::AtomicState;

    fn non_zero(num: usize) -> NonZeroUsize {
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
    fn small_write_read() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // Nothing to read initially
        assert!(cons.try_get_reader_grant().is_none());

        // Write to the buffer
        let payload = [1, 2, 3, 4];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        writer.commit(bytes);

        // Read from the buffer
        let mut packet = [0u8; 16];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        reader.consume(bytes);

        // Bytes must match
        assert_eq!(&packet[..bytes], &payload);

        // Nothing to read after
        assert!(cons.try_get_reader_grant().is_none());
    }

    #[test]
    fn multi_threaded_contention() {
        let mut buffer = [0u8; 512];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut rng = rand::rng();
                let mut total_count = 0;
                while total_count < 10_000 {
                    if let Some(mut writer) = prod.try_get_writer_grant(NonZeroUsize::MAX) {
                        let elements = Vec::from_iter(
                            (total_count..(total_count + writer.len()))
                                .map(|num| (num % 256) as u8),
                        );
                        let bytes =
                            writer.copy_max_from(&elements[..rng.random_range(0..writer.len())]);
                        writer.commit(bytes);
                        total_count += bytes;
                    }
                }
                println!("Thread 1 finished (total_count: {total_count})");
            });

            scope.spawn(|| {
                let mut rng = rand::rng();
                let mut total_count = 0;
                let mut buffer = [0u8; 250];
                while total_count < 10_000 {
                    if let Some(mut reader) = cons.try_get_reader_grant() {
                        let bytes = reader.copy_max_into(&mut buffer[..rng.random_range(100..250)]);

                        // Ensure global stream order stays consistent under contention.
                        for (offset, byte) in buffer[..bytes].iter().copied().enumerate() {
                            let expected = ((total_count + offset) % 256) as u8;
                            assert_eq!(
                                byte,
                                expected,
                                "mismatch at read index {}",
                                total_count + offset
                            );
                        }

                        reader.consume(bytes);
                        total_count += bytes;
                    }
                }
                println!("Thread 2 finished (total_count: {total_count})");
            });

            std::thread::sleep(Duration::from_millis(500));
            dbg!(&state);
        });
    }

    #[test]
    fn wrapping_write_read() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();
        // [0,0,0,0,0,0,0,0]

        let payload = [1, 2, 3, 4, 5, 6];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        writer.commit(bytes);
        // [W,W,W,W,W,W,0,0]

        let mut packet = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(&packet[..bytes], &[1, 2, 3, 4]);
        reader.consume(bytes);
        // [R,R,R,R,W,W,0,0]

        let mut reader = cons.try_get_reader_grant().unwrap();
        assert_eq!(&*reader, &[5, 6]);
        // Get a read grant and hold it

        let payload = [7, 8, 9, 10, 11, 12];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(&writer[..bytes], &[7, 8, 9, 10], "buffer: {:?}", state);
        writer.commit(bytes);
        // [W,W,W,W,W,W,|,0]

        let mut packet = [0u8; 16];
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(&packet[..bytes], &[5, 6]);
        reader.consume(bytes);
        // [W,W,W,W,R,R,0,0]

        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(&packet[..bytes], &[7, 8, 9, 10]);
        reader.consume(bytes);
        // [R,R,R,R,R,R,0,0]

        let writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        assert_eq!(&*writer, &[5, 6, 0, 0]);
    }

    #[test]
    fn writer_grant_does_not_wrap_if_buf_fits_at_end() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // write=6, read=0
        let payload = [1, 2, 3, 4, 5, 6];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(bytes, 6);
        writer.commit(bytes);

        // write=6, read=4 -> space_at_end=2, space_at_start=3
        let mut scratch = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 4);
        reader.consume(bytes);

        // Requested bytes fit at end, so do not wrap early.
        let mut writer = prod.try_get_writer_grant(non_zero(2)).unwrap();
        assert_eq!(writer.len(), 2);
        writer.copy_max_from(&[9, 9]);
        writer.commit(2);

        let mut out = [0u8; 8];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut out);
        assert_eq!(bytes, 4);
        assert_eq!(&out[..bytes], &[5, 6, 9, 9]);
        reader.consume(bytes);
    }

    #[test]
    fn writer_grant_wraps_when_requested_buf_does_not_fit_at_end() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // write=6, read=0
        let payload = [1, 2, 3, 4, 5, 6];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(bytes, 6);
        writer.commit(bytes);

        // write=6, read=4 -> space_at_end=2, space_at_start=3
        let mut scratch = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 4);
        reader.consume(bytes);

        // Requested bytes do not fit at end, so wrap to larger start segment.
        let mut writer = prod.try_get_writer_grant(non_zero(3)).unwrap();
        assert_eq!(writer.len(), 4);
        writer.copy_max_from(&[7, 8, 9]);
        writer.commit(3);

        let mut out = [0u8; 8];

        // First drain tail segment before wrap marker.
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut out);
        assert_eq!(bytes, 2);
        assert_eq!(&out[..bytes], &[5, 6]);
        reader.consume(bytes);

        // Then read wrapped data from the start segment.
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut out);
        assert_eq!(bytes, 3);
        assert_eq!(&out[..bytes], &[7, 8, 9]);
        reader.consume(bytes);
    }

    #[test]
    fn readable_bytes_non_inverted() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3, 4, 5]);
        writer.commit(bytes);

        let mut scratch = [0u8; 2];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 2);
        reader.consume(bytes);

        assert_eq!(state.readable_bytes(), 3);
    }

    #[test]
    fn readable_bytes_inverted_uses_wrapped_tail_and_head() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // write=6, read=0
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(bytes, 6);
        writer.commit(bytes);

        // write=6, read=4
        let mut scratch = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 4);
        reader.consume(bytes);

        // Forces wrap due to requested length not fitting at end.
        let mut writer = prod.try_get_writer_grant(non_zero(3)).unwrap();
        let bytes = writer.copy_max_from(&[7, 8, 9]);
        assert_eq!(bytes, 3);
        writer.commit(bytes);

        // Tail [4..6) => 2 bytes, head [0..3) => 3 bytes
        assert_eq!(state.readable_bytes(), 5);
    }

    #[test]
    fn readable_bytes_normalizes_reader_at_wrap_boundary() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // Build inverted state with wrapped boundary: write=3, read=4, wrapped=6
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(bytes, 6);
        writer.commit(bytes);

        let mut scratch = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 4);
        reader.consume(bytes);

        let mut writer = prod.try_get_writer_grant(non_zero(3)).unwrap();
        let bytes = writer.copy_max_from(&[7, 8, 9]);
        assert_eq!(bytes, 3);
        writer.commit(bytes);

        // Consume tail [4..6), so reader becomes wrapped boundary (6).
        let mut scratch = [0u8; 2];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 2);
        reader.consume(bytes);

        // Only wrapped head [0..3) remains readable.
        assert_eq!(state.readable_bytes(), 3);
    }

    #[test]
    fn wrapped_writer_drop_zero_does_not_lose_tail_data() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // write=6, read=0
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(bytes, 6);
        writer.commit(bytes);

        // write=6, read=4
        let mut scratch = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 4);
        reader.consume(bytes);

        // Force a wrapped writer grant at start [0..4), then drop it without commit.
        {
            let writer = prod.try_get_writer_grant(non_zero(3)).unwrap();
            assert_eq!(writer.len(), 4);
            drop(writer);
        }

        // Dropping a zero-use wrapped grant must not alter visible readable data.
        assert_eq!(state.readable_bytes(), 2, "buffer: {:?}", state);
        let mut out = [0u8; 8];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut out);
        assert_eq!(bytes, 2, "buffer: {:?}", state);
        assert_eq!(&out[..bytes], &[5, 6]);
        reader.consume(bytes);
    }

    #[test]
    fn wrapped_writer_commit_zero_does_not_lose_tail_data() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // write=6, read=0
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(bytes, 6);
        writer.commit(bytes);

        // write=6, read=4
        let mut scratch = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut scratch);
        assert_eq!(bytes, 4);
        reader.consume(bytes);

        // Force a wrapped writer grant at start [0..4), and commit zero bytes.
        let writer = prod
            .try_get_writer_grant(NonZeroUsize::new(3).unwrap())
            .unwrap();
        assert_eq!(writer.len(), 4);
        writer.commit(0);

        // Commit(0) must preserve readable tail bytes.
        assert_eq!(state.readable_bytes(), 2, "buffer: {:?}", state);
        let mut out = [0u8; 8];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut out);
        assert_eq!(bytes, 2, "buffer: {:?}", state);
        assert_eq!(&out[..bytes], &[5, 6]);
        reader.consume(bytes);
    }

    #[test]
    fn commit_and_consume_saturate_at_grant_len() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(bytes, 8);
        writer.commit(usize::MAX);

        assert_eq!(state.readable_bytes(), 8);

        let mut out = [0u8; 8];
        let mut reader = cons.try_get_reader_grant().unwrap();
        assert_eq!(reader.len(), 8);
        let n = reader.copy_max_into(&mut out);
        assert_eq!(n, 8);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
        reader.consume(usize::MAX);

        assert_eq!(state.readable_bytes(), 0);
        assert!(cons.try_get_reader_grant().is_none());
    }

    #[test]
    fn writer_grant_release_on_drop_allows_next_grant() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, _) = state.init(buffer.as_mut()).unwrap();

        let writer0 = prod.try_get_writer_grant(non_zero(1)).unwrap();

        drop(writer0);

        assert!(prod.try_get_writer_grant(non_zero(1)).is_some());
    }

    #[test]
    fn reader_grant_release_on_drop_allows_next_grant() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3]);
        writer.commit(bytes);

        let reader0 = cons.try_get_reader_grant().unwrap();

        drop(reader0);

        assert!(cons.try_get_reader_grant().is_some());
    }

    #[test]
    fn writing_at_start_with_non_zero_writer_must_publish_wrapped() {
        let mut buffer = [0u8; 8];
        let state = AtomicState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        // Move both cursors to a non-zero equal position (empty queue at index 6).
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let n = writer.copy_max_from(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(n, 6);
        writer.commit(n);

        let mut scratch = [0u8; 6];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let n = reader.copy_max_into(&mut scratch);
        assert_eq!(n, 6);
        reader.consume(n);

        // Force start-at-zero grant from non-zero writer (space_end=2, request=3).
        let mut writer = prod.try_get_writer_grant(non_zero(3)).unwrap();
        assert_eq!(writer.len(), 6);
        writer.copy_max_from(&[9, 8, 7]);
        writer.commit(3);

        // Regression: without wrapped publication this incorrectly reports 0.
        assert_eq!(state.readable_bytes(), 3, "buffer: {:?}", state);

        let mut out = [0u8; 8];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let n = reader.copy_max_into(&mut out);
        assert_eq!(n, 3, "buffer: {:?}", state);
        assert_eq!(&out[..n], &[9, 8, 7]);
        reader.consume(n);
    }
}
