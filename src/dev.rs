use super::buffer::{BufferReader, BufferWriter};

use crate::buffer::{ReaderGrant, WriterGrant};
#[cfg(feature = "_any_embedded_io_async")]
use crate::embedded_io_async::{Read, Write};

use super::State;

/// Connects to the device-facing writing interface.
pub struct DeviceWriter<'a, E> {
    pub(super) producer: BufferWriter<'a>,
    pub(super) state: &'a State<E>,
}

impl<'a, E> DeviceWriter<'a, E> {
    /// Write a non-zero number of bytes from the provided buffer into the stream.
    /// 
    /// Note: This wakes the reader automatically, so no need to call `wake_reader`
    pub async fn write(&mut self, buf: &[u8]) -> usize {
        let mut grant = self.get_writer_grant().await;
        let bytes = grant.copy_max_from(buf);
        grant.commit(bytes);
        self.wake_reader();
        bytes
    }

    /// Write all bytes from the provided buffer into the stream.
    /// 
    /// Note: This wakes the reader automatically, so no need to call `wake_reader`
    pub async fn write_all(&mut self, buf: &[u8]) {
        let mut buf = buf;
        while !buf.is_empty() {
            let bytes = self.write(buf).await;
            buf = &buf[bytes..];
        }
    }

    /// Insert an error into the stream.
    /// 
    /// Note: This wakes the reader automatically, so no need to call `wake_reader`
    pub fn insert_error(&mut self, error: E) {
        self.state.error.set(error);
        self.state.wait_reader.wake();
    }

    /// Get a grant to write into
    pub async fn get_writer_grant(&mut self) -> WriterGrant<'a> {
        loop {
            let subscriber = self.state.wait_writer.subscribe().await;

            if let Some(grant) = self.producer.get_grant() {
                return grant
            }

            _ = subscriber.await;
        }
    }

    pub fn wake_reader(&mut self) {
        self.state.wait_reader.wake();
    }

    /// Connect this writer to another [`embedded_io_async::Read`], such that
    /// all bytes received through `reader` will be copied to this writers buffer.
    ///
    /// This will loop forever, or until the reader returns an EOF conditions,
    /// represented by a 0 byte read.
    #[cfg(feature = "_any_embedded_io_async")]
    pub async fn embedded_io_connect<R: Read<Error = E>>(&mut self, reader: R) {
        self.embedded_io_connect_mapped(reader, |error| error).await
    }

    /// Connect this writer to another [`embedded_io_async::Read`], such that
    /// all bytes received through `reader` will be copied to this writers buffer.
    ///
    /// This will loop forever, or until the reader returns an EOF conditions,
    /// represented by a 0 byte read.
    #[cfg(feature = "_any_embedded_io_async")]
    pub async fn embedded_io_connect_mapped<R: Read>(&mut self, mut reader: R, map_err: impl Fn(R::Error) -> E) {
        loop {
            let mut grant = self.get_writer_grant().await;
            match reader.read(grant.buf_mut()).await {
                Ok(0) => break,
                Ok(bytes) => {
                    grant.commit(bytes);
                    self.wake_reader();
                }
                Err(error) => {
                    grant.commit(0);
                    self.insert_error(map_err(error));
                }
            }
        }
    }
}

/// Connects to the device-facing reading interface.
pub struct DeviceReader<'a, E> {
    pub(super) consumer: BufferReader<'a>,
    pub(super) state: &'a State<E>,
}

impl<'a, E> DeviceReader<'a, E> {
    pub async fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut grant = self.get_reader_grant().await;
        let bytes = grant.copy_max_into(buf);
        grant.release(bytes);
        self.wake_writer();
        bytes
    }

    pub fn insert_error(&mut self, error: E) {
        self.state.error.set(error);
        self.wake_writer();
    }

    /// Get a grant to read from
    pub async fn get_reader_grant(&mut self) -> ReaderGrant<'a> {
        loop {
            let subscriber = self.state.wait_reader.subscribe().await;

            if let Some(grant) = self.consumer.get_grant() {
                return grant
            }

            _ = subscriber.await;
        }
    }

    /// Wake the writing end to notify it that 
    pub fn wake_writer(&mut self) {
        self.state.wait_writer.wake();
    }

    /// Connect this reader to another [`embedded_io_async::Write`], such that
    /// all bytes received through this reader will be written to `writer`.
    ///
    /// This will loop forever, *or* until the `writer` reaches an EOF condition.
    #[cfg(feature = "_any_embedded_io_async")]
    pub async fn embedded_io_connect<W: Write<Error = E>>(&mut self, writer: W) {
        self.embedded_io_connect_mapped(writer, |error| error).await
    }

    /// Connect this reader to another [`embedded_io_async::Write`], such that
    /// all bytes received through this reader will be written to `writer`.
    ///
    /// This will loop forever, *or* until the `writer` reaches an EOF condition.
    #[cfg(feature = "_any_embedded_io_async")]
    pub async fn embedded_io_connect_mapped<W: Write>(&mut self, mut writer: W, map_err: impl Fn(W::Error) -> E) {
        loop {
            let grant = self.get_reader_grant().await;

            match writer.write(grant.buf()).await {
                Ok(0) => break,
                Ok(bytes) => {
                    grant.release(bytes);
                    self.wake_writer();
                }
                Err(error) => {
                    grant.release(0);
                    self.insert_error(map_err(error));
                }
            }
        }
    }
}

// ---- embedded-io-async impls ---- //

#[cfg(feature = "_any_embedded_io_async")]
mod impl_embedded_io_async {
    use crate::embedded_io_async::{ErrorType, Read, Write};

    use crate::{DeviceReader, DeviceWriter};

    impl<E> ErrorType for DeviceWriter<'_, E> {
        type Error = core::convert::Infallible;
    }

    impl<E> Write for DeviceWriter<'_, E> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            Ok(DeviceWriter::write(self, buf).await)
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl<E> ErrorType for DeviceReader<'_, E> {
        type Error = core::convert::Infallible;
    }

    impl<E> Read for DeviceReader<'_, E> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            Ok(DeviceReader::read(self, buf).await)
        }
    }
}
