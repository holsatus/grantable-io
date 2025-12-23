use super::State;
use super::buffer::{BufferReader, BufferWriter, ReaderGrant};
use crate::buffer::WriterGrant;

pub struct Writer<'a, E> {
    pub(super) writer: BufferWriter<'a>,
    pub(super) state: &'a State<E>,
    pub(super) grant: Option<WriterGrant<'a>>,
}

impl<'a, E> Writer<'a, E> {
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, E> {
        let grant = self.get_writer_grant().await?;
        let bytes = grant.copy_max_from(buf);
        self.commit(bytes);
        Ok(bytes)
    }

    pub async fn buf_mut(&mut self) -> Result<&mut [u8], E> {
        let grant = self.get_writer_grant().await?;
        Ok(&mut *grant)
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), E> {
        let mut buf = buf;
        while !buf.is_empty() {
            match self.write(buf).await {
                Ok(0) => panic!("write() returned Ok(0)"),
                Ok(n) => buf = &buf[n..],
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn commit(&mut self, bytes: usize) {
        if let Some(grant) = self.grant.take() {
            grant.commit(bytes);
        }
        self.state.wait_reader.wake();
    }

    async fn get_writer_grant(&mut self) -> Result<&mut WriterGrant<'a>, E> {
        // No need to check error before using pre-existing grant.
        if self.grant.is_some() {
            return Ok(self.grant.as_mut().unwrap());
        }

        loop {
            let subscription = self.state.wait_writer.subscribe().await;

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }

            if let Some(grant) = self.writer.get_writer_grant() {
                return Ok(self.grant.insert(grant));
            }

            _ = subscription.await;
        }
    }
}

pub struct Reader<'a, E> {
    pub(super) reader: BufferReader<'a>,
    pub(super) state: &'a State<E>,
    pub(super) grant: Option<ReaderGrant<'a>>,
}

impl<'a, E> Reader<'a, E> {
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, E> {
        let grant = self.get_reader_grant().await?;
        let bytes = grant.copy_max_into(buf);
        self.release(bytes);
        Ok(bytes)
    }

    pub async fn fill_buf(&mut self) -> Result<&[u8], E> {
        let grant = self.get_reader_grant().await?;
        Ok(&*grant)
    }

    pub async fn fill_buf_mut(&mut self) -> Result<&mut [u8], E> {
        let grant = self.get_reader_grant().await?;
        Ok(&mut *grant)
    }

    pub fn release(&mut self, bytes: usize) {
        if let Some(grant) = self.grant.take() {
            grant.release(bytes);
        }
        self.state.wait_writer.wake();
    }

    async fn get_reader_grant(&mut self) -> Result<&mut ReaderGrant<'a>, E> {
        // No need to check error before using pre-existing grant.
        if self.grant.is_some() {
            return Ok(self.grant.as_mut().unwrap());
        }

        loop {
            let subscription = self.state.wait_reader.subscribe().await;

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }

            if let Some(grant) = self.reader.get_reader_grant() {
                return Ok(self.grant.insert(grant));
            }

            _ = subscription.await;
        }
    }
}

// ---- embedded-io-async impls ---- //

#[cfg(feature = "_any_embedded_io_async")]
mod impl_embedded_io_async {

    use crate::embedded_io_async::{BufRead, Error, ErrorType, Read, Write};
    use crate::{Reader, Writer};

    impl<E: Error> ErrorType for Writer<'_, E> {
        type Error = E;
    }

    impl<E: Error> Write for Writer<'_, E> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            Writer::write(self, buf).await
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl<E: Error> ErrorType for Reader<'_, E> {
        type Error = E;
    }

    impl<E: Error> Read for Reader<'_, E> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            Reader::read(self, buf).await
        }
    }

    impl<E: Error> BufRead for Reader<'_, E> {
        async fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
            Reader::fill_buf(self).await
        }

        fn consume(&mut self, bytes: usize) {
            Reader::release(self, bytes)
        }
    }
}

mod test {

    #[test]
    fn test_buf_read() {
        use crate::GrantableIo;
        use core::convert::Infallible;

        const BUF: &[u8] = b"_foo_bar_baz";

        let serial_port = GrantableIo::<20, Infallible>::new();
        let (mut hard, mut soft) = serial_port.claim_reader();

        futures_executor::block_on(async {
            hard.write_all(BUF.as_ref()).await;

            let mut buf = [0u8; 4];

            assert_eq!(soft.read(buf.as_mut()).await.unwrap(), 4);
            assert_eq!(&buf, b"_foo");

            assert_eq!(soft.read(buf.as_mut()).await.unwrap(), 4);
            assert_eq!(&buf, b"_bar");

            assert_eq!(soft.read(buf.as_mut()).await.unwrap(), 4);
            assert_eq!(&buf, b"_baz");
        })
    }
}
