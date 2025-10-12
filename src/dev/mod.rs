use super::buffer::{Consumer, Producer};
use embedded_io_async::{ErrorType, Read, Write};
use maitake_sync::WaitCell;

use super::State;

pub struct DevProducer<'a, E> {
    pub(super) producer: Producer<'a>,
    pub(super) state: &'a State<E>,
}

impl<'a, E> DevProducer<'a, E> {
    pub async fn write(&mut self, buf: &[u8]) -> usize {
        let mut grant = self.get_writer_grant().await;
        let bytes = grant.copy_max_from(buf);
        grant.commit(bytes);
        self.state.wait_reader.wake();
        bytes
    }

    pub fn insert_error(&mut self, error: E) {
        self.state.error.set(error);
        self.state.wait_reader.wake();
    }

    /// Connect this writer to another [`embedded_io_async::Read`], such that
    /// all bytes received through `reader` will be copied to this writers buffer.
    ///
    /// This will loop forever, or until the reader returns an EOF conditions,
    /// represented by a 0 byte read.
    pub async fn connect<R: Read<Error = E>>(&mut self, mut reader: R) {
        loop {
            let mut grant = self.get_writer_grant().await;
            match reader.read(grant.buf_mut()).await {
                Ok(0) => break,
                Ok(bytes) => {
                    grant.commit(bytes);
                    self.state.wait_reader.wake();
                }
                Err(error) => {
                    grant.commit(0);
                    self.insert_error(error);
                    self.state.wait_reader.wake();
                }
            }
        }
    }

    fn get_writer_grant(&mut self) -> WriterGrantFuture<'_, 'a> {
        WriterGrantFuture {
            wait: &self.state.wait_writer,
            producer: &mut self.producer,
        }
    }
}

pub struct DevConsumer<'a, E> {
    pub(super) consumer: Consumer<'a>,
    pub(super) state: &'a State<E>,
}

impl<'a, E> DevConsumer<'a, E> {
    pub async fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut grant = self.get_reader_grant().await;
        let bytes = grant.copy_max_into(buf);
        grant.release(bytes);
        self.state.wait_writer.wake();
        bytes
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
            let grant = self.get_reader_grant().await;

            match writer.write(grant.buf()).await {
                Ok(0) => break,
                Ok(bytes) => {
                    grant.release(bytes);
                    self.state.wait_writer.wake();
                }
                Err(error) => {
                    grant.release(0);
                    self.insert_error(error);
                    self.state.wait_writer.wake();
                }
            }
        }
    }

    fn get_reader_grant(&mut self) -> ReaderGrantFuture<'_, 'a> {
        ReaderGrantFuture {
            wait: &self.state.wait_reader,
            consumer: &mut self.consumer,
        }
    }
}

pub struct WriterGrantFuture<'s, 'a> {
    pub(crate) wait: &'a WaitCell,
    pub(crate) producer: &'s mut Producer<'a>,
}

pub struct ReaderGrantFuture<'s, 'a> {
    pub(crate) wait: &'a WaitCell,
    pub(crate) consumer: &'s mut Consumer<'a>,
}

mod impl_futures {
    use crate::{
        buffer::{ReaderGrant, WriterGrant},
        dev::{ReaderGrantFuture, WriterGrantFuture},
    };
    use core::{
        pin::Pin,
        task::{Context, Poll},
    };

    impl<'s, 'a> Future for WriterGrantFuture<'s, 'a> {
        type Output = WriterGrant<'a>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            _ = self.wait.poll_wait(cx);

            match self.producer.get_grant() {
                Some(grant) => Poll::Ready(grant),
                None => Poll::Pending,
            }
        }
    }

    impl<'s, 'a> Future for ReaderGrantFuture<'s, 'a> {
        type Output = ReaderGrant<'a>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            _ = self.wait.poll_wait(cx);

            match self.consumer.get_grant() {
                Some(grant) => Poll::Ready(grant),
                None => Poll::Pending,
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
