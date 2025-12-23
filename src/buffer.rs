use core::{
    marker::PhantomData,
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

    /// Attempt to initialize the [`AtomicBuffer`] into [`Consumer`] and [`Producer`]
    /// halves. If buffer has already been initialized, `None` will be returned.
    pub fn init<'a>(&'a self, buf: &'a mut [u8]) -> Option<(BufferWriter<'a>, BufferReader<'a>)> {
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
    /// Request a writable contiguous section of memory of at least 1 byte.
    ///
    /// Returns `None` if no space is currently available for writing.
    pub fn get_writer_grant(&mut self) -> Option<WriterGrant<'a>> {
        let state = &self.state;

        if state.write_in_progress.swap(true, Acquire) {
            debug_assert!(false, "Attempted to double-grant a read");
            return None;
        }

        let write = state.writer.load(Relaxed);
        let read = state.reader.load(Acquire);

        let is_inverted = write < read;
        let (start, grant_len) = if is_inverted {
            (write, read - write - 1)
        } else {
            let space_at_end = self.buffer.len() - write;
            let space_at_start = read.saturating_sub(1);

            // Wrap around if space at start is larger
            if space_at_start > space_at_end {
                state.wrapped.store(write, Release);
                (0, space_at_start)
            } else {
                (write, space_at_end)
            }
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
            start_offset: start,
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
    /// Obtains a contiguous slice of committed bytes. This slice may not
    /// contain ALL available bytes, if the writer has wrapped around. The
    /// remaining bytes will be available after all readable bytes are
    /// released
    pub fn get_reader_grant(&mut self) -> Option<ReaderGrant<'a>> {
        let state = &self.state;

        if state.read_in_progress.swap(true, Acquire) {
            debug_assert!(false, "Attempted to double-grant a read");
            return None;
        }

        let writer = state.writer.load(Acquire);
        let wrapped = state.wrapped.load(Acquire);
        let mut reader = state.reader.load(Relaxed);

        // Resolve the inverted case or end of read
        if (reader == wrapped) && writer < reader {
            state.reader.store(0, Release);
            reader = 0;
        }

        // Get largest available grant
        let is_inverted = writer < reader;
        let grant_len = if is_inverted {
            wrapped - reader
        } else {
            writer - reader
        };

        // Return if we were not granted anything
        if grant_len == 0 {
            state.read_in_progress.store(false, Release);
            return None;
        }

        // Construct *unique* mutable slice to the grant
        let grant_buf = unsafe {
            let base_ptr = self.buffer.cast::<u8>();
            let grant_ptr = base_ptr.add(reader).as_ptr();

            from_raw_parts_mut(grant_ptr, grant_len)
        };

        Some(ReaderGrant {
            buffer: NonNull::from(grant_buf),
            state: self.state,
            start_offset: reader,
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
    start_offset: usize,
    _p: PhantomData<&'a mut [u8]>,
}

unsafe impl Send for WriterGrant<'_> {}

impl<'a> Deref for WriterGrant<'a> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { self.buffer.as_ref() }
    }
}

impl<'a> DerefMut for WriterGrant<'a> {
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
        let amount = self.len().min(buf.len());

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
        let used = self.len().min(used);

        // Move the write index forward by used count
        atomic.writer.store(self.start_offset + used, Release);

        // Allow subsequent grants
        atomic.write_in_progress.store(false, Release);
    }
}

// Ensure grant is released if no explicit call to `GrantW::release` is called.
impl Drop for WriterGrant<'_> {
    fn drop(&mut self) {
        self.commit_inner(0);
    }
}

/// A structure representing a contiguous region of memory that
/// may be read from, and potentially "released" (or cleared)
/// from the queue
///
/// NOTE: If the grant is dropped without explicitly releasing
/// the contents, then no bytes will be released as read.
#[derive(Debug)]
pub struct ReaderGrant<'a> {
    buffer: NonNull<[u8]>,
    state: &'a BufferState,
    start_offset: usize,
    _p: PhantomData<&'a mut [u8]>,
}

unsafe impl Send for ReaderGrant<'_> {}

impl<'a> Deref for ReaderGrant<'a> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        unsafe { self.buffer.as_ref() }
    }
}

impl<'a> DerefMut for ReaderGrant<'a> {
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
        let amount = self.len().min(buf.len());

        // Copy `amount` bytes from `grant` to `buf`
        buf[..amount].copy_from_slice(&self[..amount]);

        // The number copied
        amount
    }

    /// Finalizes this readable grant and makes `used` bytes of read data
    /// available for subsequent writing grants. This consumes the grant.
    pub fn release(mut self, used: usize) {
        self.release_inner(used);
        core::mem::forget(self);
    }

    #[inline(always)]
    fn release_inner(&mut self, used: usize) {
        let state = self.state;

        // Saturate the grant release
        let used = self.len().min(used);

        // This should be fine, purely incrementing
        state.reader.store(self.start_offset + used, Release);

        // Allow subsequent grants
        state.read_in_progress.store(false, Release);
    }
}

// Ensure grant is released if no explicit call to `GrantR::release` is called.
impl Drop for ReaderGrant<'_> {
    fn drop(&mut self) {
        self.release_inner(0);
    }
}

#[cfg(test)]
mod tests {

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

        assert!(cons.get_reader_grant().is_none());

        let payload = [1u8; 2];
        let mut writer = prod.get_writer_grant().unwrap();
        let bytes = writer.copy_max_from(&payload);
        writer.commit(bytes);

        let mut packet = [0u8; 16];
        let mut reader = cons.get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        reader.release(bytes);

        assert_eq!(&packet[..bytes], &payload);
    }

    #[test]
    fn wrapping_write_read() {
        let mut buffer = [0u8; 8];
        let state = BufferState::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        assert!(cons.get_reader_grant().is_none());

        // Initial bytes [0,0,0,0,0,0,0,0]

        let payload = [1, 2, 3, 4, 5, 6];
        let mut writer = prod.get_writer_grant().unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(bytes, 6);
        writer.commit(bytes);
        // Written 6 bytes [W,W,W,W,W,W,0,0]

        let mut packet = [0u8; 4];
        let mut reader = cons.get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(&packet[..bytes], &[1, 2, 3, 4]);
        reader.release(bytes);
        // Read 4 bytes [r,r,r,r,W,W,0,0]

        let mut reader = cons.get_reader_grant().unwrap();
        assert_eq!(&*reader, &[5, 6]);
        // Hold on to the reading grant

        let payload = [7, 8, 9, 10, 11, 12];
        let mut writer = prod.get_writer_grant().unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(&writer[..bytes], &[7, 8, 9], "buffer: {:?}", state);
        writer.commit(bytes);
        // Written 3 bytes [W,W,W,r,W,W,0,0]

        let mut packet = [0u8; 16];
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(bytes, 2, "buffer: {:?}", state);
        reader.release(bytes);
        // Read last 2 bytes [W,W,W,r,r,r,0,0]

        let mut reader = cons.get_reader_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(bytes, 3, "buffer: {:?}", state);
        reader.release(bytes);
        // Read first 3 bytes [r,r,r,r,r,r,0,0]

        let writer = prod.get_writer_grant().unwrap();
        assert_eq!(&*writer, &[4, 5, 6, 0, 0])
    }
}
