# Grantable IO

This library provides circular buffers which can asynchronously be written to and read from via non-overlapping grants. It also effectively provides a way to the type of the underlying IO device, converting it into the common `Reader<'static, E>` and `Writer<'static, E>`.

It has been designed specificaly with embedded devices in mind, and is suited for `no_std` environments and DMA-backed serial devices.

```rust
use grantable_io::{GrantableIo, Reader, Writer};
use embedded_io_async::{Error, ErrorKind as E};

static BUF_RX1: GrantableIo<512, E> = GrantableIo::new();
static BUF_TX1: GrantableIo<256, E> = GrantableIo::new();

/// Initialize the Usart device, claiming a set of buffers,
/// and return the corresponding reader+writer handles.
pub fn uart1_init(spawner: Spawner, uart: Uart) 
-> Option<(Reader<'static, E>, Writer<'static, E>)> 
{
    let (mut device_tx, reader) = BUF_RX1.claim_reader()?;
    let (mut device_rx, writer) = BUF_TX1.claim_writer()?;

    spawner.spawn(uart_runner(uart, device_rx, device_tx).ok()?);
    
    (reader, writer)
}

/// The task which "glues" the Uart together with the device-
/// facing ends of the buffers, running them indefinitely.
#[embassy_executor::task]
async fn uart_runner(
    uart: Uart, 
    device_rx: DeviceReader<'static, E>, 
    device_tx: DeviceWriter<'static, E>,
) {
    let (rx, tx) = uart.setup();

    let map_err = |error| <_ as Error>::kind(&error);

    common::embassy_futures::join::join(
        device_rx.embedded_io_connect(tx, map_err),
        device_tx.embedded_io_connect(rx, map_err),
    ).await;

    defmt::warn!("Uart disconnected unexpectedly")
}
```