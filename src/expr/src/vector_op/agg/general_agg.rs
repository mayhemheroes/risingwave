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

use std::marker::PhantomData;

use risingwave_common::array::*;
use risingwave_common::bail;
use risingwave_common::types::*;

use crate::expr::ExpressionRef;
use crate::vector_op::agg::aggregator::Aggregator;
use crate::vector_op::agg::functions::RTFn;
use crate::Result;

#[derive(Clone)]
pub struct GeneralAgg<T, F, R>
where
    T: Array,
    F: for<'a> RTFn<'a, T, R>,
    R: Array,
{
    return_type: DataType,
    input_col_idx: usize,
    init_result: Option<R::OwnedItem>,
    result: Option<R::OwnedItem>,
    f: F,
    filter: ExpressionRef,
    _phantom: PhantomData<T>,
}

impl<T, F, R> GeneralAgg<T, F, R>
where
    T: Array,
    F: for<'a> RTFn<'a, T, R>,
    R: Array,
{
    pub fn new(
        return_type: DataType,
        input_col_idx: usize,
        f: F,
        init_result: Option<R::OwnedItem>,
        filter: ExpressionRef,
    ) -> Self {
        Self {
            return_type,
            input_col_idx,
            init_result: init_result.clone(),
            result: init_result,
            f,
            filter,
            _phantom: PhantomData,
        }
    }

    pub(super) fn update_single_concrete(
        &mut self,
        array: &T,
        input: &DataChunk,
        row_id: usize,
    ) -> Result<()> {
        let filter_res = self.apply_filter_on_row(input, row_id)?;
        if filter_res {
            let tmp = self
                .f
                .eval(
                    self.result.as_ref().map(|x| x.as_scalar_ref()),
                    array.value_at(row_id),
                )?
                .map(|x| x.to_owned_scalar());
            self.result = tmp;
        }
        Ok(())
    }

    pub(super) fn update_multi_concrete(
        &mut self,
        array: &T,
        input: &DataChunk,
        start_row_id: usize,
        end_row_id: usize,
    ) -> Result<()> {
        let mut cur = self.result.as_ref().map(|x| x.as_scalar_ref());
        for row_id in start_row_id..end_row_id {
            let filter_res = self.apply_filter_on_row(input, row_id)?;
            if filter_res {
                let datum_ref = array.value_at(row_id);
                cur = self.f.eval(cur, datum_ref)?;
            }
        }
        self.result = cur.map(|x| x.to_owned_scalar());
        Ok(())
    }

    pub(super) fn output_concrete(&mut self, builder: &mut R::Builder) -> Result<()> {
        let res = std::mem::replace(&mut self.result, self.init_result.clone());
        builder.append(res.as_ref().map(|x| x.as_scalar_ref()))?;
        Ok(())
    }

    /// `apply_filter_on_row` apply a filter on the given row, and return if the row satisfies the
    /// filter or not
    /// # SAFETY
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

macro_rules! impl_aggregator {
    ($input:ty, $input_variant:ident, $result:ty, $result_variant:ident) => {
        impl<F> Aggregator for GeneralAgg<$input, F, $result>
        where
            F: for<'a> RTFn<'a, $input, $result>,
        {
            fn return_type(&self) -> DataType {
                self.return_type.clone()
            }

            fn update_single(&mut self, input: &DataChunk, row_id: usize) -> Result<()> {
                if let ArrayImpl::$input_variant(i) =
                    input.column_at(self.input_col_idx).array_ref()
                {
                    self.update_single_concrete(i, input, row_id)
                } else {
                    bail!("Input fail to match {}.", stringify!($input_variant))
                }
            }

            fn update_multi(
                &mut self,
                input: &DataChunk,
                start_row_id: usize,
                end_row_id: usize,
            ) -> Result<()> {
                if let ArrayImpl::$input_variant(i) =
                    input.column_at(self.input_col_idx).array_ref()
                {
                    self.update_multi_concrete(i, input, start_row_id, end_row_id)
                } else {
                    bail!(
                        "Input fail to match {} or builder fail to match {}.",
                        stringify!($input_variant),
                        stringify!($result_variant)
                    )
                }
            }

            fn output(&mut self, builder: &mut ArrayBuilderImpl) -> Result<()> {
                if let ArrayBuilderImpl::$result_variant(b) = builder {
                    self.output_concrete(b)
                } else {
                    bail!("Builder fail to match {}.", stringify!($result_variant))
                }
            }
        }
    };
}

// max/min
impl_aggregator! { I16Array, Int16, I16Array, Int16 }
impl_aggregator! { I32Array, Int32, I32Array, Int32 }
impl_aggregator! { I64Array, Int64, I64Array, Int64 }
impl_aggregator! { F32Array, Float32, F32Array, Float32 } // sum
impl_aggregator! { F64Array, Float64, F64Array, Float64 } // sum
impl_aggregator! { DecimalArray, Decimal, DecimalArray, Decimal } // sum
impl_aggregator! { Utf8Array, Utf8, Utf8Array, Utf8 }
impl_aggregator! { BoolArray, Bool, BoolArray, Bool } // TODO(#359): remove once unnecessary
impl_aggregator! { StructArray, Struct, StructArray, Struct }
impl_aggregator! { ListArray, List, ListArray, List }
impl_aggregator! { IntervalArray, Interval, IntervalArray, Interval }
impl_aggregator! { NaiveTimeArray, NaiveTime, NaiveTimeArray, NaiveTime }
impl_aggregator! { NaiveDateArray, NaiveDate, NaiveDateArray, NaiveDate }
impl_aggregator! { NaiveDateTimeArray, NaiveDateTime, NaiveDateTimeArray, NaiveDateTime }

// count
impl_aggregator! { I16Array, Int16, I64Array, Int64 } // sum
impl_aggregator! { I32Array, Int32, I64Array, Int64 } // sum
impl_aggregator! { F32Array, Float32, I64Array, Int64 }
impl_aggregator! { F64Array, Float64, I64Array, Int64 }
impl_aggregator! { DecimalArray, Decimal, I64Array, Int64 }
impl_aggregator! { Utf8Array, Utf8, I64Array, Int64 }
impl_aggregator! { BoolArray, Bool, I64Array, Int64 }
impl_aggregator! { StructArray, Struct, I64Array, Int64 }
impl_aggregator! { ListArray, List, I64Array, Int64 }
impl_aggregator! { IntervalArray, Interval, I64Array, Int64 }
impl_aggregator! { NaiveTimeArray, NaiveTime, I64Array, Int64 }
impl_aggregator! { NaiveDateArray, NaiveDate, I64Array, Int64 }
impl_aggregator! { NaiveDateTimeArray, NaiveDateTime, I64Array, Int64 }

// sum
impl_aggregator! { I64Array, Int64, DecimalArray, Decimal }

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use risingwave_common::array::column::Column;
    use risingwave_common::types::Decimal;

    use super::*;
    use crate::expr::{AggKind, Expression, LiteralExpression};
    use crate::vector_op::agg::aggregator::create_agg_state_unary;

    fn eval_agg(
        input_type: DataType,
        input: ArrayRef,
        agg_kind: AggKind,
        return_type: DataType,
        mut builder: ArrayBuilderImpl,
    ) -> Result<ArrayImpl> {
        let len = input.len();
        let input_chunk = DataChunk::new(vec![Column::new(input)], len);
        let filter: ExpressionRef = Arc::from(
            LiteralExpression::new(DataType::Boolean, Some(ScalarImpl::Bool(true))).boxed(),
        );
        let mut agg_state =
            create_agg_state_unary(input_type, 0, agg_kind, return_type, false, filter)?;
        agg_state.update_multi(&input_chunk, 0, input_chunk.cardinality())?;
        agg_state.output(&mut builder)?;
        builder.finish().map_err(Into::into)
    }

    #[test]
    fn single_value_int32() -> Result<()> {
        let test_case = |numbers: &[Option<i32>], result: &[Option<i32>]| -> Result<()> {
            let input = I32Array::from_slice(numbers).unwrap();
            let agg_kind = AggKind::SingleValue;
            let input_type = DataType::Int32;
            let return_type = DataType::Int32;
            let actual = eval_agg(
                input_type,
                Arc::new(input.into()),
                agg_kind,
                return_type,
                ArrayBuilderImpl::Int32(I32ArrayBuilder::new(0)),
            );
            if !result.is_empty() {
                let actual = actual?;
                let actual = actual.as_int32().iter().collect::<Vec<_>>();
                assert_eq!(actual, result);
            } else {
                assert!(actual.is_err());
            }
            Ok(())
        };

        // zero row
        test_case(&[], &[None])?;

        // one row
        test_case(&[Some(1)], &[Some(1)])?;
        test_case(&[None], &[None])?;

        // more than one row
        test_case(&[Some(1), Some(2)], &[])?;
        test_case(&[Some(1), None], &[])?;
        test_case(&[None, Some(1)], &[])?;
        test_case(&[None, None], &[])?;
        test_case(&[None, Some(1), None], &[])?;
        test_case(&[None, None, Some(1)], &[])
    }

    #[test]
    fn vec_sum_int32() -> Result<()> {
        let input = I32Array::from_slice(&[Some(1), Some(2), Some(3)]).unwrap();
        let agg_kind = AggKind::Sum;
        let input_type = DataType::Int32;
        let return_type = DataType::Int64;
        let actual = eval_agg(
            input_type,
            Arc::new(input.into()),
            agg_kind,
            return_type,
            ArrayBuilderImpl::Int64(I64ArrayBuilder::new(0)),
        )?;
        let actual = actual.as_int64();
        let actual = actual.iter().collect::<Vec<_>>();
        assert_eq!(actual, &[Some(6)]);
        Ok(())
    }

    #[test]
    fn vec_sum_int64() -> Result<()> {
        let input = I64Array::from_slice(&[Some(1), Some(2), Some(3)])?;
        let agg_kind = AggKind::Sum;
        let input_type = DataType::Int64;
        let return_type = DataType::Decimal;
        let actual = eval_agg(
            input_type,
            Arc::new(input.into()),
            agg_kind,
            return_type,
            DecimalArrayBuilder::new(0).into(),
        )?;
        let actual: DecimalArray = actual.into();
        let actual = actual.iter().collect::<Vec<Option<Decimal>>>();
        assert_eq!(actual, vec![Some(Decimal::from(6))]);
        Ok(())
    }

    #[test]
    fn vec_min_float32() -> Result<()> {
        let input =
            F32Array::from_slice(&[Some(1.0.into()), Some(2.0.into()), Some(3.0.into())]).unwrap();
        let agg_kind = AggKind::Min;
        let input_type = DataType::Float32;
        let return_type = DataType::Float32;
        let actual = eval_agg(
            input_type,
            Arc::new(input.into()),
            agg_kind,
            return_type,
            ArrayBuilderImpl::Float32(F32ArrayBuilder::new(0)),
        )?;
        let actual = actual.as_float32();
        let actual = actual.iter().collect::<Vec<_>>();
        assert_eq!(actual, &[Some(1.0.into())]);
        Ok(())
    }

    #[test]
    fn vec_min_char() -> Result<()> {
        let input = Utf8Array::from_slice(&[Some("b"), Some("aa")])?;
        let agg_kind = AggKind::Min;
        let input_type = DataType::Varchar;
        let return_type = DataType::Varchar;
        let actual = eval_agg(
            input_type,
            Arc::new(input.into()),
            agg_kind,
            return_type,
            ArrayBuilderImpl::Utf8(Utf8ArrayBuilder::new(0)),
        )?;
        let actual = actual.as_utf8();
        let actual = actual.iter().collect::<Vec<_>>();
        assert_eq!(actual, vec![Some("aa")]);
        Ok(())
    }

    #[test]
    fn vec_min_list() -> Result<()> {
        use risingwave_common::array;
        let input = ListArray::from_slices(
            &[true, true, true],
            vec![
                Some(array! { I32Array, [Some(0)] }.into()),
                Some(array! { I32Array, [Some(1)] }.into()),
                Some(array! { I32Array, [Some(2)] }.into()),
            ],
            DataType::Int32,
        )?;
        let agg_type = AggKind::Min;
        let input_type = DataType::List {
            datatype: Box::new(DataType::Int32),
        };
        let return_type = DataType::List {
            datatype: Box::new(DataType::Int32),
        };
        let actual = eval_agg(
            input_type,
            Arc::new(input.into()),
            agg_type,
            return_type,
            ArrayBuilderImpl::List(ListArrayBuilder::with_meta(
                0,
                ArrayMeta::List {
                    datatype: Box::new(DataType::Int32),
                },
            )),
        )?;
        let actual = actual.as_list();
        let actual = actual.iter().collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![Some(ListRef::ValueRef {
                val: &ListValue::new(vec![Some(ScalarImpl::Int32(0))])
            })]
        );
        Ok(())
    }

    #[test]
    fn vec_max_char() -> Result<()> {
        let input = Utf8Array::from_slice(&[Some("b"), Some("aa")])?;
        let agg_kind = AggKind::Max;
        let input_type = DataType::Varchar;
        let return_type = DataType::Varchar;
        let actual = eval_agg(
            input_type,
            Arc::new(input.into()),
            agg_kind,
            return_type,
            ArrayBuilderImpl::Utf8(Utf8ArrayBuilder::new(0)),
        )?;
        let actual = actual.as_utf8();
        let actual = actual.iter().collect::<Vec<_>>();
        assert_eq!(actual, vec![Some("b")]);
        Ok(())
    }

    #[test]
    fn vec_count_int32() -> Result<()> {
        let test_case = |input: ArrayImpl, expected: &[Option<i64>]| -> Result<()> {
            let agg_kind = AggKind::Count;
            let input_type = DataType::Int32;
            let return_type = DataType::Int64;
            let actual = eval_agg(
                input_type,
                Arc::new(input),
                agg_kind,
                return_type,
                ArrayBuilderImpl::Int64(I64ArrayBuilder::new(0)),
            )?;
            let actual = actual.as_int64();
            let actual = actual.iter().collect::<Vec<_>>();
            assert_eq!(actual, expected);
            Ok(())
        };
        let input = I32Array::from_slice(&[Some(1), Some(2), Some(3)]).unwrap();
        let expected = &[Some(3)];
        test_case(input.into(), expected)?;
        let input = I32Array::from_slice(&[]).unwrap();
        let expected = &[Some(0)];
        test_case(input.into(), expected)?;
        let input = I32Array::from_slice(&[None]).unwrap();
        let expected = &[Some(0)];
        test_case(input.into(), expected)
    }
}
