//! Concurrent end-to-end tests over the public API.
//!
//! These exercise the full pipeline (async `DeviceWriter`/`Reader` plus the
//! `WaitCell` wakeups) with real OS threads, and are intended to also run under
//! Miri (`cargo +nightly miri test`), which checks the `unsafe` buffer aliasing
//! and detects data races. The deterministic, exhaustive regression for the
//! `wrapped` lost-update lives in `src/buffer.rs` under `cfg(loom)`; the tests
//! here are the complementary "real world" checks.
//!
//! Loom cannot model these (they use the std thread pool and futures executor),
//! so the file is excluded from `--cfg loom` builds.

#![cfg(not(loom))]

use core::convert::Infallible;
use grantable_io::GrantableIo;

/// Deterministic byte stream: byte at global offset `k` is `k % 251`.
fn pattern(k: usize) -> u8 {
    (k % 251) as u8
}

static BUF: GrantableIo<16, Infallible> = GrantableIo::new();

/// Faithful producer/consumer round-trip: the reader must observe exactly the
/// bytes the writer produced, in order. This is the "does it actually work"
/// test and runs under both the normal test runner and Miri.
#[test]
fn spsc_roundtrip() {
    // Keep the Miri run small; it is orders of magnitude slower.
    let total = if cfg!(miri) { 128 } else { 500_000 };

    let (mut dev, mut app) = BUF.claim_reader();

    let producer = std::thread::spawn(move || {
        futures_executor::block_on(async {
            let mut written = 0usize;
            while written < total {
                let start = written;
                let n = (total - written).min(37);
                let data: Vec<u8> = (0..n).map(|i| pattern(start + i)).collect();
                dev.write_all(&data).await;
                written += n;
            }
        });
    });

    let mut got = 0usize;
    let mut buf = [0u8; 64];
    futures_executor::block_on(async {
        while got < total {
            let n = app.read(&mut buf).await.unwrap();
            assert!(n > 0, "read returned 0 before EOF");
            for (i, &byte) in buf[..n].iter().enumerate() {
                assert_eq!(byte, pattern(got + i), "corrupt byte at {}", got + i);
            }
            got += n;
        }
    });

    producer.join().unwrap();
    assert_eq!(got, total);
}

/// Targeted stress/regression for the `wrapped` lost update.
///
/// A tiny buffer plus a fast spinning `try_read` consumer makes the writer wrap
/// frequently; this reliably corrupts the stream (or deadlocks) on the buggy
/// implementation. Each iteration uses a fresh buffer because `GrantableIo` is
/// single-use.
///
/// This is inherently timing-dependent: run it with `--release --test-threads=1`
/// for a deterministic-enough signal (`cargo test --release --test concurrency
/// -- --test-threads=1`). The exhaustive, deterministic regression is the Loom
/// test in `src/buffer.rs`; this test is the real-world complement. It is
/// skipped under Miri (which cannot detect a lost update) and its spin would be
/// pathologically slow there.
#[test]
#[cfg_attr(miri, ignore = "covered deterministically by the loom test")]
fn wrap_race_regression() {
    const TOTAL: usize = 200_000;
    const ITERS: usize = 50;

    for _ in 0..ITERS {
        let gio: &'static GrantableIo<3, Infallible> =
            Box::leak(Box::new(GrantableIo::<3, Infallible>::new()));
        let (mut dev, mut app) = gio.claim_reader();

        let producer = std::thread::spawn(move || {
            futures_executor::block_on(async {
                let mut written = 0usize;
                while written < TOTAL {
                    let start = written;
                    let n = (TOTAL - written).min(53);
                    let data: Vec<u8> = (0..n).map(|i| pattern(start + i)).collect();
                    dev.write_all(&data).await;
                    written += n;
                }
            });
        });

        let mut got = 0usize;
        let mut b = [0u8; 97];
        let mut spins: u64 = 0;
        while got < TOTAL {
            match app.try_read(&mut b).unwrap() {
                0 => {
                    spins += 1;
                    assert!(spins < 200_000_000, "reader made no progress (deadlock)");
                    std::hint::spin_loop();
                }
                n => {
                    spins = 0;
                    for (i, &byte) in b[..n].iter().enumerate() {
                        assert_eq!(byte, pattern(got + i), "corrupt byte at {}", got + i);
                    }
                    got += n;
                }
            }
        }

        producer.join().unwrap();
    }
}
