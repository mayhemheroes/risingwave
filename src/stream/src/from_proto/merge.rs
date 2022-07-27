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

use risingwave_common::catalog::{Field, Schema};

use super::*;
use crate::executor::exchange::input::new_input;
use crate::executor::{MergeExecutor, ReceiverExecutor};

pub struct MergeExecutorBuilder;

impl ExecutorBuilder for MergeExecutorBuilder {
    fn new_boxed_executor(
        params: ExecutorParams,
        x_node: &StreamNode,
        _store: impl StateStore,
        stream: &mut LocalStreamManagerCore,
    ) -> Result<BoxedExecutor> {
        let node = try_match_expand!(x_node.get_node_body().unwrap(), NodeBody::Merge)?;
        let upstreams = node.get_upstream_actor_id();
        let upstream_fragment_id = node.get_upstream_fragment_id();
        let fields = node.fields.iter().map(Field::from).collect();
        let schema = Schema::new(fields);
        let actor_context = params.actor_context;

        let inputs: Vec<_> = upstreams
            .iter()
            .map(|&upstream_actor_id| {
                new_input(
                    &stream.context,
                    stream.streaming_metrics.clone(),
                    params.actor_id,
                    params.fragment_id,
                    upstream_actor_id,
                    upstream_fragment_id,
                )
            })
            .try_collect()?;

        if inputs.len() == 1 {
            Ok(ReceiverExecutor::new(
                schema,
                params.pk_indices,
                inputs.into_iter().next().unwrap(),
                actor_context,
                x_node.operator_id,
                params.actor_id,
                stream.streaming_metrics.clone(),
            )
            .boxed())
        } else {
            Ok(MergeExecutor::new(
                schema,
                params.pk_indices,
                params.actor_id,
                params.fragment_id,
                upstream_fragment_id,
                inputs,
                stream.context.clone(),
                actor_context,
                x_node.operator_id,
                stream.streaming_metrics.clone(),
            )
            .boxed())
        }
    }
}
