use super::buffer::{Consumer, GrantR, Producer};

use super::{SerialState, grant::GrantWriter};

pub struct AppProducer<'a, E> {
    pub(super) producer: Producer<'a>,
    pub(super) state: &'a SerialState<E>,
}

impl<E> AppProducer<'_, E> {
    pub async fn grant_writer(&mut self) -> Result<GrantWriter<'_, E>, E> {
        loop {
            let subscription = self.state.wait_writer.subscribe().await;

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }

            match self.producer.get_grant() {
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
        self.grant_writer()
            .await
            .map(|grant| grant.copy_max_from(buf))
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

pub struct AppConsumer<'a, E> {
    pub(super) consumer: Consumer<'a>,
    pub(super) state: &'a SerialState<E>,
    pub(super) grant: Option<GrantR<'a>>,
}

impl<'a, E> AppConsumer<'a, E> {
    async fn get_grant(&mut self) -> Result<&mut GrantR<'a>, E> {
        loop {
            let subscription = self.state.wait_reader.subscribe().await;

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }

            // Release any existing grant, double-grants are illegal anyway
            self.grant = None;

            match self.consumer.get_grant() {
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
        self.get_grant().await.map(|grant| grant.buf())
    }

    fn consume(&mut self, amt: usize) {
        if let Some(grant) = self.grant.take() {
            grant.release(amt);
        }
    }
}

// ---- embedded-io-async impls ---- //

mod impl_embedded_io_async {

    use crate::{AppConsumer, AppProducer};
    use embedded_io_async::{BufRead, Error, ErrorType, Read, Write};

    impl<E: Error> ErrorType for AppProducer<'_, E> {
        type Error = E;
    }

    impl<E: Error> Write for AppProducer<'_, E> {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            AppProducer::write(self, buf).await
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl<E: Error> ErrorType for AppConsumer<'_, E> {
        type Error = E;
    }

    impl<E: Error> Read for AppConsumer<'_, E> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            AppConsumer::read(self, buf).await
        }
    }

    impl<E: Error> BufRead for AppConsumer<'_, E> {
        async fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
            AppConsumer::fill_buf(self).await
        }

        fn consume(&mut self, amt: usize) {
            AppConsumer::consume(self, amt)
        }
    }
}

mod test {

    #[test]
    fn test_buf_read() {
        use crate::SerialPort;
        use embedded_io_async::Write;
        use core::convert::Infallible;

        const BUF: &[u8] = b"_foo_bar_baz";

        let serial_port = SerialPort::<20, Infallible>::new();
        let (mut hard, mut soft) = serial_port.claim_reader();

        futures_executor::block_on(async {
            hard.write_all(BUF.as_ref()).await.unwrap();

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
