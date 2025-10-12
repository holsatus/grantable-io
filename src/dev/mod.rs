use super::buffer::{Consumer, Producer};
use embedded_io_async::{ErrorType, Read, Write};

use super::{
    SerialState,
    grant::{GrantReader, GrantWriter},
};

pub struct DevProducer<'a, E> {
    pub(super) producer: Producer<'a>,
    pub(super) state: &'a SerialState<E>,
}

impl<E> DevProducer<'_, E> {
    pub async fn grant_writer(&mut self) -> GrantWriter<'_, E> {
        loop {
            let subscribtion = self.state.wait_writer.subscribe().await;

            match self.producer.get_grant() {
                Ok(grant_inner) => {
                    return GrantWriter {
                        state: self.state,
                        inner: grant_inner,
                    };
                }

                Err(super::buffer::Error::GrantInProgress) => {
                    panic!("Double-grants are illegal!");
                }

                Err(super::buffer::Error::InsufficientSize) => {
                    let res = subscribtion.await;
                    debug_assert!(res.is_ok());
                }
            }
        }
    }

    pub fn insert_error(&mut self, error: E) {
        self.state.error.set(error);
        self.state.wait_reader.wake();
    }

    pub async fn write(&mut self, buf: &[u8]) -> usize {
        self.grant_writer().await.copy_max_from(buf)
    }

    /// Connect this writer to another [`embedded_io_async::Read`], such that
    /// all bytes received through `reader` will be copied to this writers buffer.
    ///
    /// This will loop forever, or until the reader returns an EOF conditions,
    /// represented by a 0 byte read.
    pub async fn connect<R: Read<Error = E>>(&mut self, mut reader: R) {
        loop {
            let mut grant = self.grant_writer().await;
            match reader.read(grant.buffer_mut()).await {
                Ok(0) => break, // This should not happen
                Ok(bytes) => grant.commit(bytes),
                Err(error) => {
                    drop(grant); // Drop without waking
                    self.insert_error(error);
                }
            }
        }
    }
}

pub struct DevConsumer<'a, E> {
    pub(super) consumer: Consumer<'a>,
    pub(super) state: &'a SerialState<E>,
}

impl<E> DevConsumer<'_, E> {
    pub async fn grant_reader(&mut self) -> GrantReader<'_, E> {
        loop {
            let subscribtion = self.state.wait_reader.subscribe().await;

            match self.consumer.get_grant() {
                Ok(grant_inner) => {
                    return GrantReader {
                        state: self.state,
                        inner: grant_inner,
                    };
                }

                Err(super::buffer::Error::GrantInProgress) => {
                    panic!("Double-grants are illegal!");
                }

                Err(super::buffer::Error::InsufficientSize) => {
                    let res = subscribtion.await;
                    debug_assert!(res.is_ok());
                }
            }
        }
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> usize {
        self.grant_reader().await.copy_max_into(buf)
    }

    pub fn insert_error(&mut self, error: E) {
        self.state.error.set(error);
        self.state.wait_writer.wake();
    }

    /// Connect this reader to another [`embedded_io_async::Write`], such that
    /// all bytes received through this reader will be written to `writer`.
    ///
    /// This will loop forever, *or* until the `writer` reaches an EOF condition.
    pub async fn connect<W: Write<Error = E>>(&mut self, mut writer: W) {
        loop {
            let grant = self.grant_reader().await;

            match writer.write(grant.buffer()).await {
                Ok(0) => break,
                Ok(bytes) => grant.release(bytes),
                Err(error) => {
                    drop(grant); // Drop without waking
                    self.insert_error(error)
                }
            }
        }
    }
}

// ---- embedded-io-async impls ---- //

impl<E> ErrorType for DevConsumer<'_, E> {
    type Error = core::convert::Infallible;
}

impl<E> Write for DevProducer<'_, E> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        Ok(DevProducer::write(self, buf).await)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<E> ErrorType for DevProducer<'_, E> {
    type Error = core::convert::Infallible;
}

impl<E> Read for DevConsumer<'_, E> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        Ok(DevConsumer::read(self, buf).await)
    }
}
