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

use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use risingwave_common::array::{Op, Row, StreamChunk};
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::Schema;
use risingwave_common::types::Datum;
use risingwave_common::util::ordered::{OrderedRow, OrderedRowDeserializer};
use risingwave_common::util::sort_util::{OrderPair, OrderType};
use risingwave_storage::table::streaming_table::state_table::StateTable;
use risingwave_storage::StateStore;

use super::error::StreamExecutorResult;
use super::managed_state::top_n::ManagedTopNStateNew;
use super::top_n::{generate_internal_key, TopNCache};
use super::top_n_executor::{generate_output, TopNExecutorBase, TopNExecutorWrapper};
use super::{Executor, ExecutorInfo, PkIndices, PkIndicesRef};

pub type GroupTopNExecutor<S> = TopNExecutorWrapper<InnerGroupTopNExecutorNew<S>>;

impl<S: StateStore> GroupTopNExecutor<S> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input: Box<dyn Executor>,
        order_pairs: Vec<OrderPair>,
        offset_and_limit: (usize, Option<usize>),
        pk_indices: PkIndices,
        total_count: usize,
        executor_id: u64,
        key_indices: Vec<usize>,
        group_by: Vec<usize>,
        state_table: StateTable<S>,
    ) -> StreamExecutorResult<Self> {
        let info = input.info();
        let schema = input.schema().clone();

        Ok(TopNExecutorWrapper {
            input,
            inner: InnerGroupTopNExecutorNew::new(
                info,
                schema,
                order_pairs,
                offset_and_limit,
                pk_indices,
                total_count,
                executor_id,
                key_indices,
                group_by,
                state_table,
            )?,
        })
    }
}

pub struct InnerGroupTopNExecutorNew<S: StateStore> {
    info: ExecutorInfo,

    /// Schema of the executor.
    schema: Schema,

    /// `LIMIT XXX`. None means no limit.
    limit: Option<usize>,

    /// `OFFSET XXX`. `0` means no offset.
    offset: usize,

    /// The primary key indices of the `LocalTopNExecutor`
    pk_indices: PkIndices,

    /// The internal key indices of the `LocalTopNExecutor`
    internal_key_indices: PkIndices,

    /// The order of internal keys of the `LocalTopNExecutor`
    internal_key_order_types: Vec<OrderType>,

    /// We are interested in which element is in the range of [offset, offset+limit).
    managed_state: ManagedTopNStateNew<S>,

    /// which column we used to group the data.
    group_by: Vec<usize>,

    /// group key -> cache for this group
    caches: HashMap<Vec<Datum>, TopNCache>,

    #[expect(dead_code)]
    /// Indices of the columns on which key distribution depends.
    key_indices: Vec<usize>,
}

impl<S: StateStore> InnerGroupTopNExecutorNew<S> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input_info: ExecutorInfo,
        schema: Schema,
        order_pairs: Vec<OrderPair>,
        offset_and_limit: (usize, Option<usize>),
        pk_indices: PkIndices,
        total_count: usize,
        executor_id: u64,
        key_indices: Vec<usize>,
        group_by: Vec<usize>,
        state_table: StateTable<S>,
    ) -> StreamExecutorResult<Self> {
        let (internal_key_indices, internal_key_data_types, internal_key_order_types) =
            generate_internal_key(&order_pairs, &pk_indices, &schema);

        let ordered_row_deserializer =
            OrderedRowDeserializer::new(internal_key_data_types, internal_key_order_types.clone());

        let managed_state =
            ManagedTopNStateNew::<S>::new(total_count, state_table, ordered_row_deserializer);

        Ok(Self {
            info: ExecutorInfo {
                schema: input_info.schema,
                pk_indices: input_info.pk_indices,
                identity: format!("TopNExecutorNew {:X}", executor_id),
            },
            schema,
            offset: offset_and_limit.0,
            limit: offset_and_limit.1,
            managed_state,
            pk_indices,
            internal_key_indices,
            internal_key_order_types,
            key_indices,
            group_by,
            caches: HashMap::new(),
        })
    }
}

#[async_trait]
impl<S: StateStore> TopNExecutorBase for InnerGroupTopNExecutorNew<S> {
    async fn apply_chunk(
        &mut self,
        chunk: StreamChunk,
        epoch: u64,
    ) -> StreamExecutorResult<StreamChunk> {
        let mut res_ops = Vec::with_capacity(self.limit.unwrap_or(1024));
        let mut res_rows = Vec::with_capacity(self.limit.unwrap_or(1024));

        for (op, row_ref) in chunk.rows() {
            let pk_row = row_ref.row_by_indices(&self.internal_key_indices);
            let ordered_pk_row = OrderedRow::new(pk_row, &self.internal_key_order_types);
            let row = row_ref.to_owned_row();
            let mut group_key = Vec::with_capacity(self.group_by.len());
            for &col_id in &self.group_by {
                group_key.push(row[col_id].clone());
            }

            // If 'self.caches' does not already have a cache for the current group, create a new
            // cache for it and insert it into `self.caches`
            let pk_prefix = Row::new(group_key.clone());
            let entry = self.caches.entry(group_key);
            match entry {
                Occupied(_) => {}
                Vacant(entry) => {
                    let mut topn_cache = TopNCache::new(self.offset, self.limit.unwrap_or(1024));
                    self.managed_state
                        .init_topn_cache(Some(&pk_prefix), &mut topn_cache, epoch)
                        .await?;
                    entry.insert(topn_cache);
                }
            }

            // apply the chunk to state table
            match op {
                Op::Insert | Op::UpdateInsert => {
                    self.managed_state
                        .insert(ordered_pk_row.clone(), row.clone(), epoch)?;
                }

                Op::Delete | Op::UpdateDelete => {
                    self.managed_state
                        .delete(&ordered_pk_row, row.clone(), epoch)?;
                }
            }

            // update the corresponding rows in the group cache.
            self.caches
                .get_mut(&pk_prefix.0)
                .unwrap()
                .update(
                    Some(&pk_prefix),
                    &mut self.managed_state,
                    op,
                    ordered_pk_row,
                    row,
                    epoch,
                    &mut res_ops,
                    &mut res_rows,
                )
                .await?;
        }
        // compare the those two ranges and emit the differantial result
        generate_output(res_rows, res_ops, &self.schema)
    }

    async fn flush_data(&mut self, epoch: u64) -> StreamExecutorResult<()> {
        self.managed_state.flush(epoch).await
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn pk_indices(&self) -> PkIndicesRef {
        &self.pk_indices
    }

    fn identity(&self) -> &str {
        &self.info.identity
    }

    fn update_state_table_vnode_bitmap(&mut self, vnode_bitmap: Arc<Bitmap>) {
        self.managed_state
            .state_table
            .update_vnode_bitmap(vnode_bitmap);
    }

    async fn init(&mut self, _epoch: u64) -> StreamExecutorResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use assert_matches::assert_matches;
    use futures::StreamExt;
    use risingwave_common::array::stream_chunk::StreamChunkTestExt;
    use risingwave_common::catalog::Field;
    use risingwave_common::types::DataType;
    use risingwave_common::util::sort_util::OrderType;

    use super::*;
    use crate::executor::test_utils::top_n_executor::create_in_memory_state_table;
    use crate::executor::test_utils::MockSource;
    use crate::executor::{Barrier, Message};

    fn create_schema() -> Schema {
        Schema {
            fields: vec![
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
            ],
        }
    }

    fn create_order_pairs() -> Vec<OrderPair> {
        vec![
            OrderPair::new(1, OrderType::Ascending),
            OrderPair::new(2, OrderType::Ascending),
            OrderPair::new(0, OrderType::Ascending),
        ]
    }

    fn create_stream_chunks() -> Vec<StreamChunk> {
        let chunk0 = StreamChunk::from_pretty(
            "  I I I
            + 10 9 1
            +  8 8 2
            +  7 8 2
            +  9 1 1
            + 10 1 1
            +  8 1 3",
        );
        let chunk1 = StreamChunk::from_pretty(
            "  I I I
            - 10 9 1
            - 8 8 2
            - 10 1 1",
        );
        let chunk2 = StreamChunk::from_pretty(
            "  I I I
            - 7 8 2
            - 8 1 3
            - 9 1 1",
        );
        let chunk3 = StreamChunk::from_pretty(
            "  I I I
            +  5 1 1
            +  2 1 1
            +  3 1 2
            +  4 1 3",
        );
        vec![chunk0, chunk1, chunk2, chunk3]
    }

    fn create_source() -> Box<MockSource> {
        let mut chunks = create_stream_chunks();
        let schema = create_schema();
        Box::new(MockSource::with_messages(
            schema,
            PkIndices::new(),
            vec![
                Message::Barrier(Barrier::new_test_barrier(1)),
                Message::Chunk(std::mem::take(&mut chunks[0])),
                Message::Barrier(Barrier::new_test_barrier(2)),
                Message::Chunk(std::mem::take(&mut chunks[1])),
                Message::Barrier(Barrier::new_test_barrier(3)),
                Message::Chunk(std::mem::take(&mut chunks[2])),
                Message::Barrier(Barrier::new_test_barrier(4)),
                Message::Chunk(std::mem::take(&mut chunks[3])),
                Message::Barrier(Barrier::new_test_barrier(5)),
            ],
        ))
    }

    fn compare_stream_chunk(lhs_chunk: &StreamChunk, rhs_chunk: &StreamChunk) {
        let mut lhs_message = HashSet::new();
        let mut rhs_message = HashSet::new();
        for (op, row_ref) in lhs_chunk.rows() {
            let row = row_ref.to_owned_row();
            let op_code = match op {
                Op::Insert => 1,
                Op::Delete => 2,
                Op::UpdateDelete => 3,
                Op::UpdateInsert => 4,
            };
            lhs_message.insert((op_code, row.clone()));
        }

        for (op, row_ref) in rhs_chunk.rows() {
            let row = row_ref.to_owned_row();
            let op_code = match op {
                Op::Insert => 1,
                Op::Delete => 2,
                Op::UpdateDelete => 3,
                Op::UpdateInsert => 4,
            };
            rhs_message.insert((op_code, row.clone()));
        }

        assert_eq!(lhs_message, rhs_message);
    }

    #[tokio::test]
    async fn test_without_offset_and_with_limits() {
        let order_types = create_order_pairs();
        let source = create_source();
        let state_table = create_in_memory_state_table(
            &[DataType::Int64, DataType::Int64, DataType::Int64],
            &[
                OrderType::Ascending,
                OrderType::Ascending,
                OrderType::Ascending,
            ],
            &[1, 2, 0],
        );
        let top_n_executor = Box::new(
            GroupTopNExecutor::new(
                source as Box<dyn Executor>,
                order_types,
                (0, Some(2)),
                vec![1, 2, 0],
                0,
                1,
                vec![],
                vec![1],
                state_table,
            )
            .unwrap(),
        );
        let mut top_n_executor = top_n_executor.execute();

        // consume the init barrier
        top_n_executor.next().await.unwrap().unwrap();
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                +  9 1 1
                + 10 1 1
                +  7 8 2
                +  8 8 2
                + 10 9 1",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 10 1 1
                - 8 8 2
                - 10 9 1
                +  8 1 3",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 9 1 1
                - 8 1 3
                - 7 8 2",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                " I I I
                + 2 1 1
                + 5 1 1",
            ),
        );
    }

    #[tokio::test]
    async fn test_with_offset_and_with_limits() {
        let order_types = create_order_pairs();
        let source = create_source();
        let state_table = create_in_memory_state_table(
            &[DataType::Int64, DataType::Int64, DataType::Int64],
            &[
                OrderType::Ascending,
                OrderType::Ascending,
                OrderType::Ascending,
            ],
            &[1, 2, 0],
        );
        let top_n_executor = Box::new(
            GroupTopNExecutor::new(
                source as Box<dyn Executor>,
                order_types,
                (1, Some(2)),
                vec![1, 2, 0],
                0,
                1,
                vec![],
                vec![1],
                state_table,
            )
            .unwrap(),
        );
        let mut top_n_executor = top_n_executor.execute();

        // consume the init barrier
        top_n_executor.next().await.unwrap().unwrap();
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                + 10 1 1
                +  8 1 3
                +  8 8 2",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 10 1 1
                - 8 8 2",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 8 1 3",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                " I I I
                + 3 1 2
                + 5 1 1",
            ),
        );
    }

    #[tokio::test]
    async fn test_without_limits() {
        let order_types = create_order_pairs();
        let source = create_source();
        let state_table = create_in_memory_state_table(
            &[DataType::Int64, DataType::Int64, DataType::Int64],
            &[
                OrderType::Ascending,
                OrderType::Ascending,
                OrderType::Ascending,
            ],
            &[1, 2, 0],
        );
        let top_n_executor = Box::new(
            GroupTopNExecutor::new(
                source as Box<dyn Executor>,
                order_types,
                (0, None),
                vec![1, 2, 0],
                0,
                1,
                vec![],
                vec![1],
                state_table,
            )
            .unwrap(),
        );
        let mut top_n_executor = top_n_executor.execute();

        // consume the init barrier
        top_n_executor.next().await.unwrap().unwrap();
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                + 10 9 1
                +  8 8 2
                +  7 8 2
                +  9 1 1
                + 10 1 1
                +  8 1 3",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 10 9 1
                - 8 8 2
                - 10 1 1",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 9 1 1
                - 8 1 3
                - 7 8 2",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                +  5 1 1
                +  2 1 1
                +  3 1 2
                +  4 1 3",
            ),
        );
    }

    #[tokio::test]
    async fn test_multi_group_key() {
        let order_types = create_order_pairs();
        let source = create_source();
        let state_table = create_in_memory_state_table(
            &[DataType::Int64, DataType::Int64, DataType::Int64],
            &[
                OrderType::Ascending,
                OrderType::Ascending,
                OrderType::Ascending,
            ],
            &[1, 2, 0],
        );
        let top_n_executor = Box::new(
            GroupTopNExecutor::new(
                source as Box<dyn Executor>,
                order_types,
                (0, Some(2)),
                vec![1, 2, 0],
                0,
                1,
                vec![],
                vec![1, 2],
                state_table,
            )
            .unwrap(),
        );
        let mut top_n_executor = top_n_executor.execute();

        // consume the init barrier
        top_n_executor.next().await.unwrap().unwrap();
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                + 10 9 1
                +  8 8 2
                +  7 8 2
                +  9 1 1
                + 10 1 1
                +  8 1 3",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 10 9 1
                - 8 8 2
                - 10 1 1",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 7 8 2
                - 8 1 3
                - 9 1 1",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        compare_stream_chunk(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                +  5 1 1
                +  2 1 1
                +  3 1 2
                +  4 1 3",
            ),
        );
    }
}
