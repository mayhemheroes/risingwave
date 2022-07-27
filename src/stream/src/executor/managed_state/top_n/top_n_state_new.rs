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

use std::borrow::Cow;
use std::collections::BTreeMap;

use futures::{pin_mut, StreamExt};
use risingwave_common::array::Row;
use risingwave_common::catalog::{ColumnDesc, ColumnId, TableId};
use risingwave_common::types::DataType;
use risingwave_common::util::ordered::*;
use risingwave_storage::table::state_table::RowBasedStateTable;
use risingwave_storage::StateStore;

use crate::executor::error::StreamExecutorResult;
use crate::executor::PkIndices;

pub struct ManagedTopNStateNew<S: StateStore> {
    /// Relational table.
    state_table: RowBasedStateTable<S>,
    /// The total number of rows in state table.
    total_count: usize,
    /// For deserializing `OrderedRow`.
    ordered_row_deserializer: OrderedRowDeserializer,
}

#[derive(Clone, PartialEq, Debug)]
pub struct TopNStateRow {
    pub ordered_key: OrderedRow,
    pub row: Row,
}

impl TopNStateRow {
    pub fn new(ordered_key: OrderedRow, row: Row) -> Self {
        Self { ordered_key, row }
    }
}

impl<S: StateStore> ManagedTopNStateNew<S> {
    pub fn new(
        total_count: usize,
        store: S,
        table_id: TableId,
        data_types: Vec<DataType>,
        ordered_row_deserializer: OrderedRowDeserializer,
        pk_indices: PkIndices,
    ) -> Self {
        let order_types = ordered_row_deserializer.get_order_types().to_vec();

        let column_descs = data_types
            .iter()
            .enumerate()
            .map(|(id, data_type)| {
                ColumnDesc::unnamed(ColumnId::from(id as i32), data_type.clone())
            })
            .collect::<Vec<_>>();
        let state_table = RowBasedStateTable::new_without_distribution(
            store,
            table_id,
            column_descs,
            order_types,
            pk_indices,
        );
        Self {
            state_table,
            total_count,
            ordered_row_deserializer,
        }
    }

    pub fn insert(
        &mut self,
        _key: OrderedRow,
        value: Row,
        _epoch: u64,
    ) -> StreamExecutorResult<()> {
        self.state_table.insert(value)?;
        self.total_count += 1;
        Ok(())
    }

    pub fn delete(
        &mut self,
        _key: &OrderedRow,
        value: Row,
        _epoch: u64,
    ) -> StreamExecutorResult<()> {
        self.state_table.delete(value)?;
        self.total_count -= 1;
        Ok(())
    }

    #[cfg(test)]
    pub fn total_count(&self) -> usize {
        self.total_count
    }

    fn get_topn_row(&self, iter_res: Cow<Row>) -> TopNStateRow {
        let row = iter_res.into_owned();
        let mut datums = Vec::with_capacity(self.state_table.pk_indices().len());
        for pk_index in self.state_table.pk_indices() {
            datums.push(row[*pk_index].clone());
        }
        let pk = Row::new(datums);
        let pk_ordered = OrderedRow::new(pk, self.ordered_row_deserializer.get_order_types());
        TopNStateRow::new(pk_ordered, row)
    }

    #[cfg(test)]
    /// This function will return the rows in the range of [`offset`, `offset` + `limit`),
    pub async fn find_range(
        &self,
        offset: usize,
        num_limit: usize,
        epoch: u64,
    ) -> StreamExecutorResult<Vec<TopNStateRow>> {
        let state_table_iter = self.state_table.iter(epoch).await?;
        pin_mut!(state_table_iter);

        let mut rows = Vec::with_capacity(num_limit.min(1024));
        // here we don't expect users to have large OFFSET.
        let mut stream = state_table_iter.skip(offset).take(num_limit);
        while let Some(item) = stream.next().await {
            rows.push(self.get_topn_row(item?));
        }
        Ok(rows)
    }

    pub async fn fill_cache(
        &self,
        cache: &mut BTreeMap<OrderedRow, Row>,
        start_key: &OrderedRow,
        cache_size_limit: usize,
        epoch: u64,
    ) -> StreamExecutorResult<()> {
        let state_table_iter = self.state_table.iter(epoch).await?;
        pin_mut!(state_table_iter);
        while let Some(item) = state_table_iter.next().await {
            let topn_row = self.get_topn_row(item?);
            if topn_row.ordered_key <= *start_key {
                continue;
            }
            cache.insert(topn_row.ordered_key, topn_row.row);
            if cache.len() == cache_size_limit {
                break;
            }
        }
        Ok(())
    }

    pub async fn flush(&mut self, epoch: u64) -> StreamExecutorResult<()> {
        self.state_table.commit(epoch).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::catalog::TableId;
    use risingwave_common::types::DataType;
    use risingwave_common::util::sort_util::OrderType;
    use risingwave_storage::memory::MemoryStateStore;

    // use std::collections::BTreeMap;
    use super::*;
    use crate::row_nonnull;

    #[tokio::test]
    async fn test_managed_top_n_state() {
        let store = MemoryStateStore::new();
        let data_types = vec![DataType::Varchar, DataType::Int64];
        let order_types = vec![OrderType::Ascending, OrderType::Ascending];
        let mut managed_state = ManagedTopNStateNew::new(
            0,
            store,
            TableId::from(0x11),
            data_types.clone(),
            OrderedRowDeserializer::new(data_types, order_types.clone()),
            vec![0, 1],
        );

        let row1 = row_nonnull!["abc".to_string(), 2i64];
        let row2 = row_nonnull!["abc".to_string(), 3i64];
        let row3 = row_nonnull!["abd".to_string(), 3i64];
        let row4 = row_nonnull!["ab".to_string(), 4i64];
        let rows = vec![row1, row2, row3, row4];
        let ordered_rows = rows
            .clone()
            .into_iter()
            .map(|row| OrderedRow::new(row, &order_types))
            .collect::<Vec<_>>();

        let epoch = 1;
        managed_state
            .insert(ordered_rows[3].clone(), rows[3].clone(), epoch)
            .unwrap();

        // now ("ab", 4)
        let valid_rows = managed_state.find_range(0, 1, epoch).await.unwrap();

        assert_eq!(valid_rows.len(), 1);
        assert_eq!(valid_rows[0].ordered_key, ordered_rows[3].clone());

        managed_state
            .insert(ordered_rows[2].clone(), rows[2].clone(), epoch)
            .unwrap();
        let valid_rows = managed_state.find_range(1, 1, epoch).await.unwrap();
        assert_eq!(valid_rows.len(), 1);
        assert_eq!(valid_rows[0].ordered_key, ordered_rows[2].clone());

        managed_state
            .insert(ordered_rows[1].clone(), rows[1].clone(), epoch)
            .unwrap();
        assert_eq!(3, managed_state.total_count());

        let valid_rows = managed_state.find_range(1, 2, epoch).await.unwrap();
        assert_eq!(valid_rows.len(), 2);
        assert_eq!(
            valid_rows.first().unwrap().ordered_key,
            ordered_rows[1].clone()
        );
        assert_eq!(
            valid_rows.last().unwrap().ordered_key,
            ordered_rows[2].clone()
        );

        // delete ("abc", 3)
        managed_state
            .delete(&ordered_rows[1].clone(), rows[1].clone(), epoch)
            .unwrap();

        // insert ("abc", 2)
        managed_state
            .insert(ordered_rows[0].clone(), rows[0].clone(), epoch)
            .unwrap();

        let valid_rows = managed_state.find_range(0, 3, epoch).await.unwrap();

        assert_eq!(valid_rows.len(), 3);
        assert_eq!(valid_rows[0].ordered_key, ordered_rows[3].clone());
        assert_eq!(valid_rows[1].ordered_key, ordered_rows[0].clone());
        assert_eq!(valid_rows[2].ordered_key, ordered_rows[2].clone());
    }

    #[tokio::test]
    async fn test_managed_top_n_state_fill_cache() {
        let store = MemoryStateStore::new();
        let data_types = vec![DataType::Varchar, DataType::Int64];
        let order_types = vec![OrderType::Ascending, OrderType::Ascending];
        let mut managed_state = ManagedTopNStateNew::new(
            0,
            store,
            TableId::from(0x11),
            data_types.clone(),
            OrderedRowDeserializer::new(data_types, order_types.clone()),
            vec![0, 1],
        );

        let row1 = row_nonnull!["abc".to_string(), 2i64];
        let row2 = row_nonnull!["abc".to_string(), 3i64];
        let row3 = row_nonnull!["abd".to_string(), 3i64];
        let row4 = row_nonnull!["ab".to_string(), 4i64];
        let row5 = row_nonnull!["abcd".to_string(), 5i64];
        let rows = vec![row1, row2, row3, row4, row5];

        let mut cache = BTreeMap::<OrderedRow, Row>::new();
        let ordered_rows = rows
            .clone()
            .into_iter()
            .map(|row| OrderedRow::new(row, &order_types))
            .collect::<Vec<_>>();

        let epoch = 1;
        managed_state
            .insert(ordered_rows[3].clone(), rows[3].clone(), epoch)
            .unwrap();
        managed_state
            .insert(ordered_rows[1].clone(), rows[1].clone(), epoch)
            .unwrap();
        managed_state
            .insert(ordered_rows[2].clone(), rows[2].clone(), epoch)
            .unwrap();
        managed_state
            .insert(ordered_rows[4].clone(), rows[4].clone(), epoch)
            .unwrap();

        managed_state
            .fill_cache(&mut cache, &ordered_rows[3], 2, epoch)
            .await
            .unwrap();
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.first_key_value().unwrap().0, &ordered_rows[1]);
        assert_eq!(cache.last_key_value().unwrap().0, &ordered_rows[4]);
    }
}
