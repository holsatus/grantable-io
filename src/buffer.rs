use core::{
    marker::PhantomData,
    ptr::NonNull,
    slice::{from_raw_parts, from_raw_parts_mut},
};

use portable_atomic::{
    AtomicBool, AtomicUsize,
    Ordering::{AcqRel, Acquire, Release},
};

#[derive(Debug)]
/// An atomic "tracking" structure for safely granting
/// read and write access to a contiguous slice of memory.
pub(crate) struct AtomicBuffer {
    /// Whether this instance has been initialized
    initialized: AtomicBool,

    /// Where the next byte will be written
    writer: AtomicUsize,

    /// Where the next byte will be read from
    reader: AtomicUsize,

    /// Used in the inverted case to mark the end of the
    /// readable streak. Otherwise will == sizeof::<self.buf>().
    /// Writer is responsible for placing this at the correct
    /// place when entering an inverted condition, and Reader
    /// is responsible for moving it back to sizeof::<self.buf>()
    /// when exiting the inverted condition
    wrapped: AtomicUsize,

    /// Used by the Writer to remember what bytes are currently
    /// allowed to be written to, but are not yet ready to be
    /// read from
    reserve: AtomicUsize,

    /// Is there an active read grant?
    read_in_progress: AtomicBool,

    /// Is there an active write grant?
    write_in_progress: AtomicBool,
}

impl Default for AtomicBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicBuffer {
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

            // Owned by the writer
            reserve: AtomicUsize::new(0),

            // Owned by the reader
            read_in_progress: AtomicBool::new(false),

            // Owned by the writer
            write_in_progress: AtomicBool::new(false),
        }
    }

    /// Attempt to initialize the [`AtomicBuffer`] into [`Consumer`] and [`Producer`]
    /// halves. If buffer has already been initialized, `None` will be returned.
    pub fn init<'a>(&'a self, buf: &'a mut [u8]) -> Option<(Producer<'a>, Consumer<'a>)> {
        if self.initialized.swap(true, AcqRel) {
            return None;
        }

        // SAFETY: We just checked above that it was not already initialized
        Some((
            Producer {
                buffer: NonNull::from(&*buf),
                atomic: self,
                pd: PhantomData,
            },
            Consumer {
                buffer: NonNull::from(&*buf),
                atomic: self,
                pd: PhantomData,
            },
        ))
    }
}

/// `Producer` is the primary interface for pushing data into a [`crate::SerialPort`].
#[derive(Debug)]
pub struct Producer<'a> {
    buffer: NonNull<[u8]>,
    atomic: &'a AtomicBuffer,
    pd: PhantomData<&'a mut [u8]>,
}

unsafe impl Send for Producer<'_> {}

impl<'a> Producer<'a> {
    /// Request a writable contiguous section of memory of at least 1 byte.
    ///
    /// Returns `None` if no space is currently available for writing.
    pub fn get_grant(&mut self) -> Option<ProduceGrant<'a>> {
        let atomic = &self.atomic;

        if atomic.write_in_progress.swap(true, AcqRel) {
            panic!("Attempted to double-grant a write");
        }

        let write = atomic.writer.load(Acquire);
        let read = atomic.reader.load(Acquire);
        let max = self.buffer.len();

        let (start, grant) = if write < read {
            // --- Inverted case ---
            let space = read - write - 1;
            if space == 0 {
                atomic.write_in_progress.store(false, Release);
                return None;
            }
            (write, space)
        } else {
            // --- Non-inverted case ---
            let space_at_end = max - write;
            // Space at the start is from index 0 up to `read`.
            // We must leave one byte empty, so `write` never equals `read`.
            let space_at_start = if read == 0 { 0 } else { read - 1 };

            if space_at_start > space_at_end {
                // Grant from the start is larger. Wrap around.
                atomic.wrapped.store(write, Release);
                (0, space_at_start)
            } else if space_at_end > 0 {
                // Grant from the end is larger or equal. Use it.
                (write, space_at_end)
            } else {
                // No space anywhere.
                atomic.write_in_progress.store(false, Release);
                return None;
            }
        };

        // Safe write, only viewed by this task
        atomic.reserve.store(start + grant, Release);

        // This is sound, as `NonNull` is `#[repr(transparent)]
        let grant = unsafe { &mut self.buffer.as_mut()[start..(start + grant)] };

        Some(ProduceGrant {
            grant,
            atomic: self.atomic,
        })
    }
}

/// `Consumer` is the primary interface for reading data from a `BBBuffer`.
#[derive(Debug)]
pub struct Consumer<'a> {
    buffer: NonNull<[u8]>,
    atomic: &'a AtomicBuffer,
    pd: PhantomData<&'a mut [u8]>,
}

unsafe impl Send for Consumer<'_> {}

impl<'a> Consumer<'a> {
    /// Obtains a contiguous slice of committed bytes. This slice may not
    /// contain ALL available bytes, if the writer has wrapped around. The
    /// remaining bytes will be available after all readable bytes are
    /// released
    pub fn get_grant(&mut self) -> Option<ConsumeGrant<'a>> {
        let atomic = &self.atomic;

        if atomic.read_in_progress.swap(true, AcqRel) {
            panic!("Attempted to double-grant a read");
        }

        let write = atomic.writer.load(Acquire);
        let last = atomic.wrapped.load(Acquire);
        let mut read = atomic.reader.load(Acquire);

        // Resolve the inverted case or end of read
        if (read == last) && (write < read) {
            atomic.reader.store(0, Release);
            read = 0;
        }

        // Get largest available grant
        let is_inverted = write < read;
        let grant = if is_inverted {
            last - read
        } else {
            write - read
        };

        if grant == 0 {
            atomic.read_in_progress.store(false, Release);
            return None;
        }

        // This is sound, as `NonNull` is `#[repr(transparent)]
        let grant = unsafe { &mut self.buffer.as_mut()[read..(read + grant)] };

        Some(ConsumeGrant {
            grant,
            atomic: self.atomic,
        })
    }
}

/// A structure representing a contiguous region of memory that
/// may be written to, and potentially "committed" to the queue.
///
/// NOTE: If the grant is dropped without explicitly commiting
/// the contents, then no bytes will be comitted for writing.
#[derive(Debug)]
pub struct ProduceGrant<'a> {
    grant: &'a mut [u8],
    atomic: &'a AtomicBuffer,
}

unsafe impl Send for ProduceGrant<'_> {}

impl ProduceGrant<'_> {
    /// Copy the largest possible amount of bytes to the grant
    /// from the given buffer. Whichever is shorter decides the number
    /// of bytes written. The return value is the amount copied.
    pub(crate) fn copy_max_from(&mut self, buf: &[u8]) -> usize {
        // Maximum number of bytes that can be copied contiguously
        let amount = self.buf().len().min(buf.len());

        // Copy `amount` bytes from `grant` to `buf`
        self.buf_mut()[..amount].copy_from_slice(&buf[..amount]);

        // The number copied
        amount
    }

    /// Finalizes this writable grant and makes `used` bytes of written data
    /// available for subsequent reading grants. This consumes the grant.
    pub fn commit(mut self, used: usize) {
        self.commit_inner(used);
        core::mem::forget(self);
    }

    /// Obtain access to the inner buffer for reading.
    pub fn buf(&self) -> &[u8] {
        unsafe { from_raw_parts(self.grant.as_ptr() as *const u8, self.grant.len()) }
    }

    /// Obtain mutable access to the read grant.
    pub fn buf_mut(&mut self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.grant.as_ptr() as *mut u8, self.grant.len()) }
    }

    #[inline(always)]
    fn commit_inner(&mut self, used: usize) {
        let atomic = &self.atomic;

        // If there is no grant in progress, return early.
        if !atomic.write_in_progress.load(Acquire) {
            return;
        }

        // Saturate the grant commit
        let len = self.grant.len();
        let used = len.min(used);

        let write = atomic.writer.load(Acquire);
        atomic.reserve.fetch_sub(len - used, AcqRel);

        let max = self.grant.len();
        let last = atomic.wrapped.load(Acquire);
        let new_write = atomic.reserve.load(Acquire);

        if (new_write < write) && (write != max) {
            // We have already wrapped, but we are skipping some bytes at the end of the ring.
            // Mark `last` where the write pointer used to be to hold the line here
            atomic.wrapped.store(write, Release);
        } else if new_write > last {
            // We're about to pass the last pointer, which was previously the artificial
            // end of the ring. Now that we've passed it, we can "unlock" the section
            // that was previously skipped.
            //
            // Since new_write is strictly larger than last, it is safe to move this as
            // the other thread will still be halted by the (about to be updated) write
            // value
            atomic.wrapped.store(max, Release);
        }
        // else: If new_write == last, either:
        // * last == max, so no need to write, OR
        // * If we write in the end chunk again, we'll update last to max next time
        // * If we write to the start chunk in a wrap, we'll update last when we
        //     move write backwards

        // Write must be updated AFTER last, otherwise read could think it was
        // time to invert early!
        atomic.writer.store(new_write, Release);

        // Allow subsequent grants
        atomic.write_in_progress.store(false, Release);
    }
}

// Ensure grant is released if no explicit call to `GrantW::release` is called.
impl Drop for ProduceGrant<'_> {
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
pub struct ConsumeGrant<'a> {
    grant: &'a mut [u8],
    atomic: &'a AtomicBuffer,
}

unsafe impl Send for ConsumeGrant<'_> {}

impl ConsumeGrant<'_> {
    /// Copy the largest possible amount of bytes from the grant
    /// to the given buffer. Whichever is shorter decides the number
    /// of bytes written. The return value is the amount copied.
    pub(crate) fn copy_max_into(&mut self, buf: &mut [u8]) -> usize {
        // Maximum number of bytes that can be copied contiguously
        let amount = self.buf().len().min(buf.len());

        // Copy `amount` bytes from `grant` to `buf`
        buf[..amount].copy_from_slice(&self.buf()[..amount]);

        // The number copied
        amount
    }

    /// Finalizes this readable grant and makes `used` bytes of read data
    /// available for subsequent writing grants. This consumes the grant.
    pub fn release(mut self, used: usize) {
        self.release_inner(used);
        core::mem::forget(self);
    }

    /// Obtain access to the inner buffer for reading
    pub fn buf(&self) -> &[u8] {
        unsafe { from_raw_parts(self.grant.as_ptr() as *const u8, self.grant.len()) }
    }

    /// Obtain mutable access to the read grant
    ///
    /// This is useful if you are performing in-place operations
    /// on an incoming packet, such as decryption
    pub fn buf_mut(&mut self) -> &mut [u8] {
        unsafe { from_raw_parts_mut(self.grant.as_ptr() as *mut u8, self.grant.len()) }
    }

    #[inline(always)]
    fn release_inner(&mut self, used: usize) {
        let atomic = &self.atomic;

        // If there is no grant in progress, return early.
        if !atomic.read_in_progress.load(Acquire) {
            return;
        }

        // Saturate the grant release
        let used = self.grant.len().min(used);

        // This should be fine, purely incrementing
        let _ = atomic.reader.fetch_add(used, Release);

        // Allow subsequent grants
        atomic.read_in_progress.store(false, Release);
    }
}

// Ensure grant is released if no explicit call to `GrantR::release` is called.
impl Drop for ConsumeGrant<'_> {
    fn drop(&mut self) {
        self.release_inner(0);
    }
}

#[cfg(test)]
mod tests {
    use super::AtomicBuffer;

    #[test]
    fn catch_double_init() {
        let state = AtomicBuffer::new();

        let mut buffer0 = [0u8; 8];
        let mut buffer1 = [0u8; 8];

        assert!(state.init(buffer0.as_mut()).is_some());
        assert!(state.init(buffer1.as_mut()).is_none());
    }

    #[test]
    fn small_write_read() {
        let mut buffer = [0u8; 8];
        let state = AtomicBuffer::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        assert!(cons.get_grant().is_none());

        let payload = [1u8; 2];
        let mut writer = prod.get_grant().unwrap();
        let bytes = writer.copy_max_from(&payload);
        writer.commit(bytes);

        let mut packet = [0u8; 16];
        let mut reader = cons.get_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        reader.release(bytes);

        assert_eq!(&packet[..bytes], &payload);
    }

    #[test]
    fn wrapping_write_read() {
        let mut buffer = [0u8; 8];
        let state = AtomicBuffer::new();
        let (mut prod, mut cons) = state.init(buffer.as_mut()).unwrap();

        assert!(cons.get_grant().is_none());

        // Initial bytes [0,0,0,0,0,0,0,0]

        let payload = [1, 2, 3, 4, 5, 6];
        let mut writer = prod.get_grant().unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(bytes, 6);
        writer.commit(bytes);
        // Written 6 bytes [W,W,W,W,W,W,0,0]

        let mut packet = [0u8; 4];
        let mut reader = cons.get_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(&packet[..bytes], &[1, 2, 3, 4]);
        reader.release(bytes);
        // Read 4 bytes [r,r,r,r,W,W,0,0]

        let mut reader = cons.get_grant().unwrap();
        assert_eq!(reader.buf(), &[5, 6]);
        // Hold on to the reading grant

        let payload = [7, 8, 9, 10, 11, 12];
        let mut writer = prod.get_grant().unwrap();
        let bytes = writer.copy_max_from(&payload);
        assert_eq!(&writer.buf()[..bytes], &[7, 8, 9], "buffer: {:?}", state);
        writer.commit(bytes);
        // Written 3 bytes [W,W,W,r,W,W,0,0]

        let mut packet = [0u8; 16];
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(bytes, 2, "buffer: {:?}", state);
        reader.release(bytes);
        // Read last 2 bytes [W,W,W,r,r,r,0,0]

        let mut reader = cons.get_grant().unwrap();
        let bytes = reader.copy_max_into(&mut packet);
        assert_eq!(bytes, 3, "buffer: {:?}", state);
        reader.release(bytes);
        // Read first 3 bytes [r,r,r,r,r,r,0,0]

        let writer = prod.get_grant().unwrap();
        assert_eq!(writer.buf(), &[4, 5, 6, 0, 0])
    }
}
