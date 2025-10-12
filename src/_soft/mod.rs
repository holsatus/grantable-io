use super::buffer::{Consumer, GrantR, Producer};
use super::{SerialState, grant::GrantWriter};

pub struct SoftWriter<'a, E> {
    pub(super) producer: Producer<'a>,
    pub(super) state: &'a SerialState<E>,
}

impl <E> SoftWriter<'_, E> {
    pub async fn grant_writer(&mut self) -> Result<GrantWriter<'_, E>, E> {
        loop {
            let subscription = self.state.wait_writer.subscribe().await;

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }

            match self.producer.grant_max_remaining() {
                Ok(grant_inner) => {
                    return Ok(GrantWriter {
                        state: self.state,
                        inner: grant_inner,
                    });
                }

                Err(super::buffer::Error::GrantInProgress) => {
                    panic!("Double-grants are illegal!");
                }

                // No bytes available to read, wait for subscription
                Err(super::buffer::Error::InsufficientSize) => {
                    let res = subscription.await;
                    debug_assert!(res.is_ok());
                }
            }
        }
    }

    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, E> {
        self.grant_writer().await.map(|grant|grant.copy_max_from(buf))
    }

    pub async fn flush(&mut self) -> Result<(), E> {
        Ok(()) // TODO: Imlpement forwarding flush
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
}

pub struct SoftReader<'a, E> {
    pub(super) consumer: Consumer<'a>,
    pub(super) state: &'a SerialState<E>,
    pub(super) grant: Option<GrantR<'a>>,
}

impl<'a, E> SoftReader<'a, E> {
    async fn get_grant(&mut self) -> Result<&mut GrantR<'a>, E> {
        loop {
            let subscription = self.state.wait_reader.subscribe().await;

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }

            // Release any existing grant, double-grants are illegal anyway
            self.grant = None;

            match self.consumer.read() {
                Ok(g) => return Ok(self.grant.insert(g)),
                _ => _ = subscription.await,
            }
        }
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, E> {
        let available = self.fill_buf().await?;
        let bytes = buf.len().min(available.len());
        buf[..bytes].copy_from_slice(&available[..bytes]);
        self.consume(bytes);

        Ok(bytes)
    }

    async fn fill_buf(&mut self) -> Result<&[u8], E> {
        self.get_grant().await.map(|grant|grant.buf())
    }

    fn consume(&mut self, amt: usize) {
        if let Some(grant) = self.grant.take() {
            grant.release(amt);
        }
    }
}

// ---- embedded-io-async impls ---- //

pub mod impl_embedded_io_async {
    use embedded_io_async::{BufRead, Error, ErrorType, Read, Write};

    use crate::{SoftReader, SoftWriter};


    impl <E: Error> ErrorType for SoftWriter<'_, E> {
        type Error = E;
    }

    impl <E: Error> Write for SoftWriter<'_, E> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            SoftWriter::write(self, buf).await
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(()) // TODO Implement something for this?
        }
    }

    impl <E: Error> ErrorType for SoftReader<'_, E> {
        type Error = E;
    }

    impl <E: Error> Read for SoftReader<'_, E> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            SoftReader::read(self, buf).await
        }
    }

    impl <E: Error> BufRead for SoftReader<'_, E> {
        async fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
            SoftReader::fill_buf(self).await
        }

        fn consume(&mut self, amt: usize) {
            SoftReader::consume(self, amt)
        }
    }
}

mod test {

    #[test]
    fn test_buf_read() {
        use crate::new_serial_reader;
        use embedded_io_async::Write;

        const BUF: &[u8] = b"_foo_bar_baz";

        let mut reader = new_serial_reader!(20);

        futures_executor::block_on(async {
            reader.hardware.write_all(BUF.as_ref()).await.unwrap();

            let mut reader = reader.software;

            let mut buf = [0u8; 4];
            assert_eq!(reader.read(buf.as_mut()).await.unwrap(), 4);
            assert_eq!(&buf, b"_foo");

            let mut buf = [0u8; 4];
            assert_eq!(reader.read(buf.as_mut()).await.unwrap(), 4);
            assert_eq!(&buf, b"_bar");

            let mut buf = [0u8; 4];
            assert_eq!(reader.read(buf.as_mut()).await.unwrap(), 4);
            assert_eq!(&buf, b"_baz");
        })
    }
}
