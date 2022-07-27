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

use risingwave_common::array::*;
use risingwave_common::error::{ErrorCode, Result};
use risingwave_common::types::*;

use crate::expr::ExpressionRef;
use crate::vector_op::agg::aggregator::Aggregator;

pub struct CountStar {
    return_type: DataType,
    result: usize,
    filter: ExpressionRef,
}

impl CountStar {
    pub fn new(return_type: DataType, result: usize, filter: ExpressionRef) -> Self {
        Self {
            return_type,
            result,
            filter,
        }
    }

    /// `apply_filter_on_row` apply a filter on the given row, and return if the row satisfies the
    /// filter or not # SAFETY
    /// the given row must be visible
    fn apply_filter_on_row(&self, input: &DataChunk, row_id: usize) -> Result<bool> {
        let (row, visible) = input.row_at(row_id)?;
        assert!(visible);
        let filter_res = if let Some(ScalarImpl::Bool(v)) = self.filter.eval_row(&Row::from(row))? {
            v
        } else {
            false
        };
        Ok(filter_res)
    }
}

impl Aggregator for CountStar {
    fn return_type(&self) -> DataType {
        self.return_type.clone()
    }

    fn update_multi(
        &mut self,
        input: &DataChunk,
        start_row_id: usize,
        end_row_id: usize,
    ) -> Result<()> {
        if let Some(visibility) = input.visibility() {
            for row_id in start_row_id..end_row_id {
                if visibility.is_set(row_id)? && self.apply_filter_on_row(input, row_id)? {
                    self.result += 1;
                }
            }
        } else {
            self.result += self
                .filter
                .eval(input)?
                .iter()
                .skip(start_row_id)
                .take(end_row_id - start_row_id)
                .filter(|res| {
                    res.map(|x| *x.into_scalar_impl().as_bool())
                        .unwrap_or(false)
                })
                .count();
        }
        Ok(())
    }

    fn output(&self, builder: &mut ArrayBuilderImpl) -> Result<()> {
        match builder {
            ArrayBuilderImpl::Int64(b) => b.append(Some(self.result as i64)).map_err(Into::into),
            _ => Err(ErrorCode::InternalError("Unexpected builder for count(*).".into()).into()),
        }
    }

    fn update_single(&mut self, input: &DataChunk, row_id: usize) -> Result<()> {
        if let (row, true) = input.row_at(row_id)? {
            let filter_res =
                if let Some(ScalarImpl::Bool(v)) = self.filter.eval_row(&Row::from(row))? {
                    v
                } else {
                    false
                };

            if filter_res {
                self.result += 1;
            }
        }
        Ok(())
    }

    fn output_and_reset(&mut self, builder: &mut ArrayBuilderImpl) -> Result<()> {
        let res = self.output(builder);
        self.result = 0;
        res
    }
}
