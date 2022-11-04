use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::project_slice;
use crate::FuturesUnorderedBounded;
use futures_util::{stream::Fuse, Stream, StreamExt};
use pin_project_lite::pin_project;

impl<T: ?Sized + Stream> BufferedStreamExt for T {}

/// An extension trait for `Stream`s that provides a variety of convenient
/// combinator functions.
pub trait BufferedStreamExt: Stream {
    /// An adaptor for creating a buffered list of pending futures (unordered).
    ///
    /// If this stream's item can be converted into a future, then this adaptor
    /// will buffer up to `n` futures and then return the outputs in the order
    /// in which they complete. No more than `n` futures will be buffered at
    /// any point in time, and less than `n` may also be buffered depending on
    /// the state of each future.
    ///
    /// The returned stream will be a stream of each future's output.
    ///
    /// This method is only available when the `std` or `alloc` feature of this
    /// library is activated, and it is activated by default.
    ///
    /// # Examples
    ///
    /// ```
    /// # futures::executor::block_on(async {
    /// use futures::channel::oneshot;
    /// use futures::stream::{self, StreamExt};
    /// use futures_buffered::BufferedStreamExt;
    ///
    /// let (send_one, recv_one) = oneshot::channel();
    /// let (send_two, recv_two) = oneshot::channel();
    ///
    /// let stream_of_futures = stream::iter(vec![recv_one, recv_two]);
    /// let mut buffered = stream_of_futures.buffered_unordered(10);
    ///
    /// send_two.send(2i32)?;
    /// assert_eq!(buffered.next().await, Some(Ok(2i32)));
    ///
    /// send_one.send(1i32)?;
    /// assert_eq!(buffered.next().await, Some(Ok(1i32)));
    ///
    /// assert_eq!(buffered.next().await, None);
    /// # Ok::<(), i32>(()) }).unwrap();
    /// ```
    fn buffered_unordered(self, n: usize) -> BufferUnordered<Self>
    where
        Self::Item: Future,
        Self: Sized,
    {
        BufferUnordered {
            stream: StreamExt::fuse(self),
            in_progress_queue: FuturesUnorderedBounded::new(n),
        }
    }
}

pin_project!(
    /// Stream for the [`buffered_unordered`](BufferedStreamExt::buffered_unordered)
    /// method.
    #[must_use = "streams do nothing unless polled"]
    pub struct BufferUnordered<S: Stream> {
        #[pin]
        stream: Fuse<S>,
        in_progress_queue: FuturesUnorderedBounded<S::Item>,
    }
);

impl<St> Stream for BufferUnordered<St>
where
    St: Stream,
    St::Item: Future,
{
    type Item = <St::Item as Future>::Output;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // First up, try to spawn off as many futures as possible by filling up
        // our queue of futures.

        while let Some(i) = this.in_progress_queue.slots.pop() {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(fut)) => {
                    project_slice(this.in_progress_queue.inner.as_mut(), i)
                        .project()
                        .slot
                        .set(Some(fut));
                    this.in_progress_queue.shared.ready.push(i);
                }
                Poll::Ready(None) | Poll::Pending => {
                    this.in_progress_queue.slots.push(i);
                    break;
                }
            }
        }

        // Attempt to pull the next value from the in_progress_queue
        match this.in_progress_queue.poll_next_unpin(cx) {
            x @ (Poll::Pending | Poll::Ready(Some(_))) => return x,
            Poll::Ready(None) => {}
        }

        // If more values are still coming from the stream, we're not done yet
        if this.stream.is_done() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let queue_len = self.in_progress_queue.len();
        let (lower, upper) = self.stream.size_hint();
        let lower = lower.saturating_add(queue_len);
        let upper = match upper {
            Some(x) => x.checked_add(queue_len),
            None => None,
        };
        (lower, upper)
    }
}