use super::State;
use super::buffer::{ConsumeGrant, Consumer, Producer};
use crate::buffer::ProduceGrant;

pub struct AppProducer<'a, E> {
    pub(super) producer: Producer<'a>,
    pub(super) state: &'a State<E>,
    pub(super) grant: Option<ProduceGrant<'a>>,
}

impl<'a, E> AppProducer<'a, E> {
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, E> {
        let bytes = self.get_produce_grant().await?.copy_max_from(buf);
        self.commit(bytes);
        Ok(bytes)
    }

    pub async fn buf_mut(&mut self) -> Result<&mut [u8], E> {
        Ok(self.get_produce_grant().await?.buf_mut())
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

    async fn get_produce_grant(&mut self) -> Result<&mut ProduceGrant<'a>, E> {
        if let Some(error) = self.state.error.take() {
            return Err(error);
        }

        if self.grant.is_some() {
            return Ok(self.grant.as_mut().unwrap());
        }

        loop {
            let subscription = self.state.wait_writer.subscribe().await;

            if let Some(grant) = self.producer.get_grant() {
                return Ok(self.grant.insert(grant));
            }

            let res = subscription.await;
            debug_assert!(res.is_ok());

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }
        }
    }
}

pub struct AppConsumer<'a, E> {
    pub(super) consumer: Consumer<'a>,
    pub(super) state: &'a State<E>,
    pub(super) grant: Option<ConsumeGrant<'a>>,
}

impl<'a, E> AppConsumer<'a, E> {
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, E> {
        let bytes = self.get_consume_grant().await?.copy_max_into(buf);
        self.release(bytes);
        Ok(bytes)
    }

    pub async fn fill_buf(&mut self) -> Result<&[u8], E> {
        self.get_consume_grant().await.map(|grant| grant.buf())
    }

    pub async fn fill_buf_mut(&mut self) -> Result<&mut [u8], E> {
        self.get_consume_grant().await.map(|grant| grant.buf_mut())
    }

    pub fn release(&mut self, bytes: usize) {
        if let Some(grant) = self.grant.take() {
            grant.release(bytes);
        }
        self.state.wait_writer.wake();
    }

    async fn get_consume_grant(&mut self) -> Result<&mut ConsumeGrant<'a>, E> {
        if let Some(error) = self.state.error.take() {
            return Err(error);
        }

        if self.grant.is_some() {
            return Ok(self.grant.as_mut().unwrap());
        }

        loop {
            let subscription = self.state.wait_reader.subscribe().await;

            if let Some(grant) = self.consumer.get_grant() {
                return Ok(self.grant.insert(grant));
            }

            let res = subscription.await;
            debug_assert!(res.is_ok());

            if let Some(error) = self.state.error.take() {
                return Err(error);
            }
        }
    }
}

// ---- embedded-io-async impls ---- //

#[cfg(feature = "_any_embedded_io_async")]
mod impl_embedded_io_async {

    use crate::embedded_io_async::{BufRead, Error, ErrorType, Read, Write};
    use crate::{AppConsumer, AppProducer};

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

        fn consume(&mut self, bytes: usize) {
            AppConsumer::release(self, bytes)
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
