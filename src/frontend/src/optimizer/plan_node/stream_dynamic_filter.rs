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
use std::collections::HashMap;
use std::fmt;

use risingwave_common::catalog::Schema;
use risingwave_common::config::constant::hummock::PROPERTIES_RETAINTION_SECOND_KEY;
use risingwave_common::util::sort_util::OrderType;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::DynamicFilterNode;

use super::utils::TableCatalogBuilder;
use crate::catalog::TableCatalog;
use crate::expr::Expr;
use crate::optimizer::plan_node::{PlanBase, PlanTreeNodeBinary, ToStreamProst};
use crate::optimizer::PlanRef;
use crate::utils::{Condition, ConditionDisplay};

#[derive(Clone, Debug)]
pub struct StreamDynamicFilter {
    pub base: PlanBase,
    /// The predicate (formed with exactly one of < , <=, >, >=)
    predicate: Condition,
    // dist_key_l: Distribution,
    left_index: usize,
    left: PlanRef,
    right: PlanRef,
}

impl StreamDynamicFilter {
    pub fn new(left_index: usize, predicate: Condition, left: PlanRef, right: PlanRef) -> Self {
        // TODO: derive from input
        let base = PlanBase::new_stream(
            left.ctx(),
            left.schema().clone(),
            left.logical_pk().to_vec(),
            left.functional_dependency().clone(),
            left.distribution().clone(),
            false, /* we can have a new abstraction for append only and monotonically increasing
                    * in the future */
        );
        Self {
            base,
            predicate,
            left_index,
            left,
            right,
        }
    }
}

impl fmt::Display for StreamDynamicFilter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut concat_schema = self.left().schema().fields.clone();
        concat_schema.extend(self.right().schema().fields.clone());
        let concat_schema = Schema::new(concat_schema);
        write!(
            f,
            "StreamDynamicFilter {{ predicate: {} }}",
            ConditionDisplay {
                condition: &self.predicate,
                input_schema: &concat_schema
            }
        )
    }
}

impl PlanTreeNodeBinary for StreamDynamicFilter {
    fn left(&self) -> PlanRef {
        self.left.clone()
    }

    fn right(&self) -> PlanRef {
        self.right.clone()
    }

    fn clone_with_left_right(&self, left: PlanRef, right: PlanRef) -> Self {
        Self::new(self.left_index, self.predicate.clone(), left, right)
    }
}

impl_plan_tree_node_for_binary! { StreamDynamicFilter }

impl ToStreamProst for StreamDynamicFilter {
    fn to_stream_prost_body(&self) -> NodeBody {
        NodeBody::DynamicFilter(DynamicFilterNode {
            left_key: self.left_index as u32,
            condition: self
                .predicate
                .as_expr_unless_true()
                .map(|x| x.to_expr_proto()),
            left_table: Some(
                infer_left_internal_table_catalog(self.clone().into(), self.left_index)
                    .to_state_table_prost(),
            ),
            right_table: Some(
                infer_right_internal_table_catalog(self.right.clone()).to_state_table_prost(),
            ),
        })
    }
}

fn infer_left_internal_table_catalog(input: PlanRef, left_key_index: usize) -> TableCatalog {
    let base = input.plan_base();
    let schema = &base.schema;

    let append_only = input.append_only();
    let dist_keys = base.dist.dist_column_indices().to_vec();

    // The pk of dynamic filter internal table should be left_key + input_pk.
    let mut pk_indices = vec![left_key_index];
    // TODO(yuhao): dedup the dist key and pk.
    pk_indices.extend(&base.logical_pk);

    let mut internal_table_catalog_builder = TableCatalogBuilder::new();
    if !base.ctx.inner().with_properties.is_empty() {
        let properties: HashMap<_, _> = base
            .ctx
            .inner()
            .with_properties
            .iter()
            .filter(|(key, _)| key.as_str() == PROPERTIES_RETAINTION_SECOND_KEY)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        if !properties.is_empty() {
            internal_table_catalog_builder.add_properties(properties);
        }
    }

    schema.fields().iter().for_each(|field| {
        internal_table_catalog_builder.add_column(field);
    });

    pk_indices.iter().for_each(|idx| {
        internal_table_catalog_builder.add_order_column(*idx, OrderType::Ascending)
    });

    if !base.ctx.inner().with_properties.is_empty() {
        let properties: HashMap<_, _> = base
            .ctx
            .inner()
            .with_properties
            .iter()
            .filter(|(key, _)| key.as_str() == PROPERTIES_RETAINTION_SECOND_KEY)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        if !properties.is_empty() {
            internal_table_catalog_builder.add_properties(properties);
        }
    }

    internal_table_catalog_builder.build(dist_keys, append_only)
}

fn infer_right_internal_table_catalog(input: PlanRef) -> TableCatalog {
    let base = input.plan_base();
    let schema = &base.schema;

    // We require that the right table has distribution `Single`
    assert_eq!(
        base.dist.dist_column_indices().to_vec(),
        Vec::<usize>::new()
    );

    let mut internal_table_catalog_builder = TableCatalogBuilder::new();

    schema.fields().iter().for_each(|field| {
        internal_table_catalog_builder.add_column(field);
    });

    // No distribution keys
    internal_table_catalog_builder.build(vec![], false)
}
