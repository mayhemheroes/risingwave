// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::pin::Pin;
use std::task::Poll;

use async_stack_trace::StackTrace;
use either::Either;
use futures::stream::{select_with_strategy, BoxStream, PollNext, SelectWithStrategy};
use futures::{Stream, StreamExt};
use futures_async_stream::{stream, try_stream};
use pin_project::pin_project;
use risingwave_common::bail;
use risingwave_source::*;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::executor::error::{StreamExecutorError, StreamExecutorResult};
use crate::executor::Barrier;

type SourceReaderMessage =
    Either<StreamExecutorResult<Barrier>, StreamExecutorResult<StreamChunkWithState>>;
type SourceReaderArm = BoxStream<'static, SourceReaderMessage>;
type SourceReaderStreamInner =
    SelectWithStrategy<SourceReaderArm, SourceReaderArm, impl FnMut(&mut ()) -> PollNext, ()>;

#[pin_project]
pub(super) struct SourceReaderStream {
    #[pin]
    inner: SourceReaderStreamInner,

    /// When the source chunk reader stream is paused, it will be stored into this field.
    paused: Option<SourceReaderArm>,
}

impl SourceReaderStream {
    /// Receive barriers from barrier manager with the channel, error on channel close.
    #[try_stream(ok = Barrier, error = StreamExecutorError)]
    async fn barrier_receiver(mut rx: UnboundedReceiver<Barrier>) {
        while let Some(barrier) = rx.recv().stack_trace("source_recv_barrier").await {
            yield barrier;
        }
        bail!("barrier reader closed unexpectedly");
    }

    /// Receive chunks and states from the source reader, hang up on error.
    #[try_stream(ok = StreamChunkWithState, error = StreamExecutorError)]
    async fn source_chunk_reader(mut reader: Box<SourceStreamReaderImpl>) {
        loop {
            match reader.next().stack_trace("source_recv_chunk").await {
                Ok(chunk) => yield chunk,
                Err(err) => {
                    error!("hang up stream reader due to polling error: {}", err);
                    futures::future::pending().stack_trace("source_error").await
                }
            }
        }
    }

    #[stream(item = T)]
    async fn paused_source<T>() {
        yield futures::future::pending()
            .stack_trace("source_paused")
            .await
    }

    /// Convert this reader to a stream.
    pub fn new(
        barrier_receiver: UnboundedReceiver<Barrier>,
        source_chunk_reader: Box<SourceStreamReaderImpl>,
    ) -> Self {
        let barrier_receiver = Self::barrier_receiver(barrier_receiver);
        let source_chunk_reader = Self::source_chunk_reader(source_chunk_reader);

        let inner = select_with_strategy(
            barrier_receiver.map(Either::Left).boxed(),
            source_chunk_reader.map(Either::Right).boxed(),
            // We prefer barrier on the left hand side over source chunks.
            |_: &mut ()| PollNext::Left,
        );

        Self {
            inner,
            paused: None,
        }
    }

    /// Replace the source chunk reader with a new one for given `stream`. Used for split change.
    pub fn replace_source_chunk_reader(&mut self, reader: Box<SourceStreamReaderImpl>) {
        if self.paused.is_some() {
            panic!("should not replace source chunk reader when paused");
        }
        *self.inner.get_mut().1 = Self::source_chunk_reader(reader).map(Either::Right).boxed();
    }

    /// Pause the source stream.
    pub fn pause_source(&mut self) {
        if self.paused.is_some() {
            panic!("already paused");
        }
        let source_chunk_reader =
            std::mem::replace(self.inner.get_mut().1, Self::paused_source().boxed());
        let _ = self.paused.insert(source_chunk_reader);
    }

    /// Resume the source stream, panic if the source is not paused before.
    pub fn resume_source(&mut self) {
        let source_chunk_reader = self.paused.take().expect("not paused");
        let _ = std::mem::replace(self.inner.get_mut().1, source_chunk_reader);
    }
}

impl Stream for SourceReaderStream {
    type Item = SourceReaderMessage;

    fn poll_next(
        self: Pin<&mut Self>,
        ctx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(ctx)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use futures::{pin_mut, FutureExt};
    use risingwave_common::array::StreamChunk;
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn test_pause_and_resume() {
        let (barrier_tx, barrier_rx) = mpsc::unbounded_channel();

        let table_source = TableSourceV2::new(vec![]);
        let source_reader =
            SourceStreamReaderImpl::TableV2(table_source.stream_reader(vec![]).await.unwrap());

        let stream = SourceReaderStream::new(barrier_rx, Box::new(source_reader));
        pin_mut!(stream);

        macro_rules! next {
            () => {
                stream.next().now_or_never().flatten()
            };
        }

        // Write a chunk, and we should receive it.
        table_source.write_chunk(StreamChunk::default()).unwrap();
        assert_matches!(next!().unwrap(), Either::Right(_));
        // Write a barrier, and we should receive it.
        barrier_tx.send(Barrier::default()).unwrap();
        assert_matches!(next!().unwrap(), Either::Left(_));

        // Pause the stream.
        stream.pause_source();

        // Write a barrier.
        barrier_tx.send(Barrier::default()).unwrap();
        // Then write a chunk.
        table_source.write_chunk(StreamChunk::default()).unwrap();

        // We should receive the barrier.
        assert_matches!(next!().unwrap(), Either::Left(_));
        // We shouldn't receive the chunk.
        assert!(next!().is_none());

        // Resume the stream.
        stream.resume_source();
        // Then we can receive the chunk sent when the stream is paused.
        assert_matches!(next!().unwrap(), Either::Right(_));
    }
}
