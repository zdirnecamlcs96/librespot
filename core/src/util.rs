use crate::Error;
use byteorder::{BigEndian, ByteOrder};
use futures_core::ready;
use futures_util::{future, FutureExt, Sink, SinkExt};
use hmac::digest::Digest;
use sha1::Sha1;
use std::time::{Duration, Instant};
use std::{
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{task::JoinHandle, time::timeout};

/// Returns a future that will flush the sink, even if flushing is temporarily completed.
/// Finishes only if the sink throws an error.
pub(crate) fn keep_flushing<'a, T, S: Sink<T> + Unpin + 'a>(
    mut s: S,
) -> impl Future<Output = S::Error> + 'a {
    future::poll_fn(move |cx| match s.poll_flush_unpin(cx) {
        Poll::Ready(Err(e)) => Poll::Ready(e),
        _ => Poll::Pending,
    })
}

pub struct CancelOnDrop<T>(pub JoinHandle<T>);

impl<T> Future for CancelOnDrop<T> {
    type Output = <JoinHandle<T> as Future>::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_unpin(cx)
    }
}

impl<T> Drop for CancelOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct TimeoutOnDrop<T: Send + 'static> {
    handle: Option<JoinHandle<T>>,
    timeout: tokio::time::Duration,
}

impl<T: Send + 'static> TimeoutOnDrop<T> {
    pub fn new(handle: JoinHandle<T>, timeout: tokio::time::Duration) -> Self {
        Self {
            handle: Some(handle),
            timeout,
        }
    }

    pub fn take(&mut self) -> Option<JoinHandle<T>> {
        self.handle.take()
    }
}

impl<T: Send + 'static> Future for TimeoutOnDrop<T> {
    type Output = <JoinHandle<T> as Future>::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let r = ready!(self
            .handle
            .as_mut()
            .expect("Polled after ready")
            .poll_unpin(cx));
        self.handle = None;
        Poll::Ready(r)
    }
}

impl<T: Send + 'static> Drop for TimeoutOnDrop<T> {
    fn drop(&mut self) {
        let mut handle = if let Some(handle) = self.handle.take() {
            handle
        } else {
            return;
        };

        if (&mut handle).now_or_never().is_some() {
            // Already finished
            return;
        }

        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(timeout(self.timeout, CancelOnDrop(handle)));
            }
            Err(_) => {
                // Not in tokio context, can't spawn
                handle.abort();
            }
        }
    }
}

pub trait Seq {
    fn next(&self) -> Self;
}

macro_rules! impl_seq {
    ($($ty:ty)*) => { $(
        impl Seq for $ty {
            fn next(&self) -> Self { (*self).wrapping_add(1) }
        }
    )* }
}

impl_seq!(u8 u16 u32 u64 usize);

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SeqGenerator<T: Seq>(T);

impl<T: Seq> SeqGenerator<T> {
    pub fn new(value: T) -> Self {
        SeqGenerator(value)
    }

    pub fn get(&mut self) -> T {
        let value = self.0.next();
        mem::replace(&mut self.0, value)
    }
}

pub fn solve_hash_cash(
    ctx: &[u8],
    prefix: &[u8],
    length: i32,
    dst: &mut [u8],
) -> Result<Duration, Error> {
    // after a certain number of seconds, the challenge expires
    const TIMEOUT: u64 = 5; // seconds
    let now = Instant::now();

    let md = Sha1::digest(ctx);

    let mut counter: i64 = 0;
    let target: i64 = BigEndian::read_i64(&md[12..20]);

    let suffix = loop {
        if now.elapsed().as_secs() >= TIMEOUT {
            return Err(Error::deadline_exceeded(format!(
                "{TIMEOUT} seconds expired"
            )));
        }

        let suffix = [(target + counter).to_be_bytes(), counter.to_be_bytes()].concat();

        let mut hasher = Sha1::new();
        hasher.update(prefix);
        hasher.update(&suffix);
        let md = hasher.finalize();

        if BigEndian::read_i64(&md[12..20]).trailing_zeros() >= (length as u32) {
            break suffix;
        }

        counter += 1;
    };

    dst.copy_from_slice(&suffix);

    Ok(now.elapsed())
}
