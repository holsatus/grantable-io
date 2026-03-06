use core::{
    marker::PhantomData,
    num::NonZeroUsize,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    slice::from_raw_parts_mut,
};

use portable_atomic::{
    AtomicBool, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};

#[derive(Debug)]
/// An atomic "tracking" structure for safely granting
/// read and write access to contiguous slices of memory.
pub struct BufferState {
    /// Whether this instance has been initialized
    initialized: AtomicBool,

    /// Where the next byte will be written
    writer: AtomicUsize,

    /// Where the next byte will be read from
    reader: AtomicUsize,

    /// Where the writer has wrapped around if writer < reader
    wrapped: AtomicUsize,

    /// Is there an active read grant?
    read_in_progress: AtomicBool,

    /// Is there an active write grant?
    write_in_progress: AtomicBool,
}

impl Default for BufferState {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferState {
    /// Create a new instance of an [`AtomicBuffer`].
    pub const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),

            // Owned by the writer
            writer: AtomicUsize::new(0),

            // Owned by the reader
            reader: AtomicUsize::new(0),

            // Cooperatively owned
            wrapped: AtomicUsize::new(0),

            // Owned by the reader
            read_in_progress: AtomicBool::new(false),

            // Owned by the writer
            write_in_progress: AtomicBool::new(false),
        }
    }

    /// Get the number of writeable bytes in the buffer of length `len`.
    pub fn writable_bytes(&self, len: usize) -> usize {
        let wrapped = self.wrapped.load(Acquire);
        let reader = self.reader.load(Acquire);
        let writer = self.writer.load(Relaxed);

        if wrapped == 0 {
            len.saturating_sub(writer) + reader
        } else {
            reader.saturating_sub(writer)
        }
    }

    /// Get the number of readable bytes in the buffer.
    pub fn readable_bytes(&self) -> usize {
        let wrapped = self.wrapped.load(Acquire);
        let writer = self.writer.load(Acquire);
        let reader = self.reader.load(Relaxed);

        if wrapped == 0 {
            writer.saturating_sub(reader)
        } else if reader == wrapped {
            writer
        } else {
            wrapped.saturating_sub(reader) + writer
        }
    }

    /// Attempt to initialize the [`AtomicBuffer`] into [`Consumer`] and [`Producer`]
    /// halves. If buffer has already been initialized, `None` will be returned.
    pub fn init(&self, buf: &mut [u8]) -> Option<(BufferWriter<'_>, BufferReader<'_>)> {
        if self.initialized.swap(true, Acquire) {
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

/// `Writer` is the primary interface for pushing data into a [`crate::GrantableIo`].
#[derive(Debug)]
pub struct BufferWriter<'a> {
    buffer: NonNull<[u8]>,
    state: &'a BufferState,
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
        let state = self.state;

        if state.write_in_progress.swap(true, Acquire) {
            debug_assert!(false, "Attempted to double-grant a write");
            return None;
        }

        let wrapped = state.wrapped.load(Acquire);
        let reader = state.reader.load(Acquire);
        let writer = state.writer.load(Relaxed);

        let (start, grant_len) = if wrapped == 0 {
            let space_at_end = self.buffer.len() - writer;
            let space_at_start = reader;

            // Wrap around if space at start is larger
            if space_at_start > space_at_end && buf_len.get() > space_at_end {
                (0, space_at_start)
            } else {
                (writer, space_at_end)
            }
        } else {
            (writer, reader.saturating_sub(writer))
        };

        // Return if we were not granted anything
        if grant_len == 0 {
            state.write_in_progress.store(false, Release);
            return None;
        }

        // Construct *unique* mutable slice to the grant
        let grant_buf = unsafe {
            let base_ptr = self.buffer.cast::<u8>();
            let grant_ptr = base_ptr.add(start).as_ptr();
            from_raw_parts_mut(grant_ptr, grant_len)
        };

        Some(WriterGrant {
            buffer: NonNull::from(grant_buf),
            state: self.state,
            writer,
            start,
            _p: PhantomData,
        })
    }
}

/// `Reader` is the primary interface for reading data from a [`crate::GrantableIo`]
#[derive(Debug)]
pub struct BufferReader<'a> {
    buffer: NonNull<[u8]>,
    state: &'a BufferState,
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
    /// contain ALL available bytes, if the writer has wrapped around. The
    /// remaining bytes will be available after all readable bytes are
    /// consumed
    pub fn try_get_reader_grant(&mut self) -> Option<ReaderGrant<'a>> {
        let state = &self.state;

        if state.read_in_progress.swap(true, Acquire) {
            debug_assert!(false, "Attempted to double-grant a read");
            return None;
        }

        let wrapped = state.wrapped.load(Acquire);
        let writer = state.writer.load(Acquire);
        let reader = state.reader.load(Relaxed);

        let (start, grant_len) = if wrapped == 0 {
            (reader, writer.saturating_sub(reader))
        } else if reader != wrapped {
            (reader, wrapped.saturating_sub(reader))
        } else {
            (0, writer)
        };

        // Return if we were not granted anything
        if grant_len == 0 {
            state.read_in_progress.store(false, Release);
            return None;
        }

        // Construct *unique* mutable slice to the grant
        let grant_buf = unsafe {
            let base_ptr = self.buffer.cast::<u8>();
            let grant_ptr = base_ptr.add(start).as_ptr();
            from_raw_parts_mut(grant_ptr, grant_len)
        };

        Some(ReaderGrant {
            buffer: NonNull::from(grant_buf),
            state: self.state,
            wrapped,
            reader,
            _p: PhantomData,
        })
    }
}

/// A structure representing a contiguous region of memory that
/// may be written to, and potentially "committed" to the queue.
///
/// NOTE: If the grant is dropped without explicitly commiting
/// the contents, then no bytes will be comitted for writing.
#[derive(Debug)]
pub struct WriterGrant<'a> {
    buffer: NonNull<[u8]>,
    state: &'a BufferState,
    writer: usize,
    start: usize,
    _p: PhantomData<&'a mut [u8]>,
}

unsafe impl Send for WriterGrant<'_> {}

impl Deref for WriterGrant<'_> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { self.buffer.as_ref() }
    }
}

impl DerefMut for WriterGrant<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.buffer.as_mut() }
    }
}

impl WriterGrant<'_> {
    /// Copy the largest possible amount of bytes to the grant
    /// from the given buffer. Whichever is shorter decides the number
    /// of bytes written. The return value is the amount copied.
    pub fn copy_max_from(&mut self, buf: &[u8]) -> usize {
        // Maximum number of bytes that can be copied contiguously
        let amount = self.buffer.len().min(buf.len());

        // Copy `amount` bytes from `grant` to `buf`
        self[..amount].copy_from_slice(&buf[..amount]);

        // The number copied
        amount
    }

    /// Finalizes this writable grant and makes `used` bytes of written data
    /// available for subsequent reading grants. This consumes the grant.
    pub fn commit(mut self, used: usize) {
        self.commit_inner(used);
        core::mem::forget(self);
    }

    #[inline(always)]
    fn commit_inner(&mut self, used: usize) {
        let atomic = self.state;

        // Saturate the grant commit
        let used = self.buffer.len().min(used);
        
        // Determine wheter to move the write pointer
        let next_writer = if self.start != 0 {
            self.writer + used
        } else if used == 0 {
            self.writer
        } else {
            used
        };
        
        atomic.writer.store(next_writer, Release);

        // Commit wrapped mode if we moved the pointer back
        if next_writer < self.writer {
            atomic.wrapped.store(self.writer, Release);
        }

        // Allow subsequent grants
        atomic.write_in_progress.store(false, Release);
    }
}

// Ensure grant is consumed if no explicit call to `WriterGrant::commit` is called.
impl Drop for WriterGrant<'_> {
    fn drop(&mut self) {
        self.commit_inner(0);
    }
}

/// A structure representing a contiguous region of memory that
/// may be read from, and potentially "consumed" (or cleared)
/// from the queue
///
/// NOTE: If the grant is dropped without explicitly releasing
/// the contents, then no bytes will be consumed as read.
#[derive(Debug)]
pub struct ReaderGrant<'a> {
    buffer: NonNull<[u8]>,
    state: &'a BufferState,
    wrapped: usize,
    reader: usize,
    _p: PhantomData<&'a mut [u8]>,
}

unsafe impl Send for ReaderGrant<'_> {}

impl Deref for ReaderGrant<'_> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { self.buffer.as_ref() }
    }
}

impl DerefMut for ReaderGrant<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.buffer.as_mut() }
    }
}

impl ReaderGrant<'_> {
    /// Copy the largest possible amount of bytes from the grant
    /// to the given buffer. Whichever is shorter decides the number
    /// of bytes written. The return value is the amount copied.
    pub fn copy_max_into(&mut self, buf: &mut [u8]) -> usize {
        // Maximum number of bytes that can be copied contiguously
        let amount = self.buffer.len().min(buf.len());

        // Copy `amount` bytes from `grant` to `buf`
        buf[..amount].copy_from_slice(&self[..amount]);

        // The number copied
        amount
    }

    /// Finalizes this readable grant and makes `used` bytes of read data
    /// available for subsequent writing grants. This consumes the grant.
    pub fn consume(mut self, used: usize) {
        self.consume_inner(used);
        core::mem::forget(self);
    }

    #[inline(always)]
    fn consume_inner(&mut self, used: usize) {
        let state = self.state;

        // Saturate the grant consume
        let used = self.buffer.len().min(used);

        // Determine where to move the read pointer
        let next_reader = self.reader + used;
        if self.wrapped == 0 || next_reader < self.wrapped {
            state.reader.store(next_reader, Release);
        } else {
            state.reader.store(0, Release);
            state.wrapped.store(0, Release);
        }

        // Allow subsequent grants
        state.read_in_progress.store(false, Release);
    }
}

// Ensure grant is consumed if no explicit call to `ReaderGrant::consume` is called.
impl Drop for ReaderGrant<'_> {
    fn drop(&mut self) {
        self.consume_inner(0);
    }
}

#[cfg(test)]
mod tests {

    use std::num::NonZeroUsize;

    use super::BufferState;

    #[test]
    fn catch_double_init() {
        let state = BufferState::new();

        let mut buffer0 = [0u8; 8];
        let mut buffer1 = [0u8; 8];

        assert!(state.init(buffer0.as_mut()).is_some());
        assert!(state.init(buffer1.as_mut()).is_none());
    }

    #[test]
    fn small_write_read() {
        let mut buffer = [0u8; 8];
        let state = BufferState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        assert!(cons.try_get_reader_grant().is_none());

        let payload = [1u8; 2];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        writer.commit(bytes);

        let mut packet = [0u8; 16];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        reader.consume(bytes);

        assert_eq!(&packet[..bytes], &payload);
    }

    #[test]
    fn wrapping_write_read() {
        let mut buffer = [0u8; 8];
        let state = BufferState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        assert!(cons.try_get_reader_grant().is_none());

        // Initial bytes [0,0,0,0,0,0,0,0]

        let payload = [1, 2, 3, 4, 5, 6];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(bytes, 6);
        writer.commit(bytes);
        // Written 6 bytes [W,W,W,W,W,W,0,0]

        let mut packet = [0u8; 4];
        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(&packet[..bytes], &[1, 2, 3, 4]);
        reader.consume(bytes);
        // Read 4 bytes [r,r,r,r,W,W,0,0]

        let mut reader = cons.try_get_reader_grant().unwrap();
        assert_eq!(&*reader, &[5, 6]);
        // Hold on to the reading grant

        let payload = [7, 8, 9, 10, 11, 12];
        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(&writer[..bytes], &[7, 8, 9, 10], "buffer: {:?}", state);
        writer.commit(bytes);
        // Written 4 bytes [W,W,W,W,W,W,0,0]

        let mut packet = [0u8; 16];
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(bytes, 2, "buffer: {:?}", state);
        reader.consume(bytes);
        // Read last 2 bytes [W,W,W,r,r,r,0,0]

        let mut reader = cons.try_get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(bytes, 4, "buffer: {:?}", state);
        reader.consume(bytes);
        // Read first 4 bytes [r,r,r,r,r,r,0,0]

        let writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        assert_eq!(&*writer, &[5, 6, 0, 0])
    }

    #[test]
    fn writer_grant_does_not_wrap_if_buf_fits_at_end() {
        let mut buffer = [0u8; 8];
        let state = BufferState::new();
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
        let mut writer = prod
            .try_get_writer_grant(NonZeroUsize::new(2).unwrap())
            .unwrap();
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
        let state = BufferState::new();
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
        let mut writer = prod
            .try_get_writer_grant(NonZeroUsize::new(3).unwrap())
            .unwrap();
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
        let state = BufferState::new();
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
        let state = BufferState::new();
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
        let mut writer = prod
            .try_get_writer_grant(NonZeroUsize::new(3).unwrap())
            .unwrap();
        let bytes = writer.copy_max_from(&[7, 8, 9]);
        assert_eq!(bytes, 3);
        writer.commit(bytes);

        // Tail [4..6) => 2 bytes, head [0..3) => 3 bytes
        assert_eq!(state.readable_bytes(), 5);
    }

    #[test]
    fn readable_bytes_normalizes_reader_at_wrap_boundary() {
        let mut buffer = [0u8; 8];
        let state = BufferState::new();
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

        let mut writer = prod
            .try_get_writer_grant(NonZeroUsize::new(3).unwrap())
            .unwrap();
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
        let state = BufferState::new();
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
            let writer = prod
                .try_get_writer_grant(NonZeroUsize::new(3).unwrap())
                .unwrap();
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
        let state = BufferState::new();
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
        let state = BufferState::new();
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
        let state = BufferState::new();
        let (mut prod, _) = state.init(buffer.as_mut()).unwrap();

        let writer0 = prod
            .try_get_writer_grant(NonZeroUsize::new(1).unwrap())
            .unwrap();

        drop(writer0);

        assert!(prod
            .try_get_writer_grant(NonZeroUsize::new(1).unwrap())
            .is_some());
    }

    #[test]
    fn reader_grant_release_on_drop_allows_next_grant() {
        let mut buffer = [0u8; 8];
        let state = BufferState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        let mut writer = prod.try_get_writer_grant(NonZeroUsize::MAX).unwrap();
        let bytes = writer.copy_max_from(&[1, 2, 3]);
        writer.commit(bytes);

        let reader0 = cons.try_get_reader_grant().unwrap();

        drop(reader0);

        assert!(cons.try_get_reader_grant().is_some());
    }
}
