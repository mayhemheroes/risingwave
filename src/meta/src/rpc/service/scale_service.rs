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
use std::sync::Arc;

use risingwave_pb::catalog::source::Info::StreamSource;
use risingwave_pb::common::WorkerType;
use risingwave_pb::meta::scale_service_server::ScaleService;
use risingwave_pb::meta::{
    GetClusterInfoRequest, GetClusterInfoResponse, PauseRequest, PauseResponse, ResumeRequest,
    ResumeResponse,
};
use risingwave_pb::source::{ConnectorSplit, ConnectorSplits};
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};

use crate::barrier::{BarrierManagerRef, Command};
use crate::manager::{CatalogManagerRef, ClusterManagerRef, FragmentManagerRef};
use crate::model::MetadataModel;
use crate::storage::MetaStore;
use crate::stream::SourceManagerRef;

pub struct ScaleServiceImpl<S: MetaStore> {
    barrier_manager: BarrierManagerRef<S>,
    fragment_manager: FragmentManagerRef<S>,
    cluster_manager: ClusterManagerRef<S>,
    source_manager: SourceManagerRef<S>,
    catalog_manager: CatalogManagerRef<S>,
    ddl_lock: Arc<RwLock<()>>,
}

impl<S> ScaleServiceImpl<S>
where
    S: MetaStore,
{
    pub fn new(
        barrier_manager: BarrierManagerRef<S>,
        fragment_manager: FragmentManagerRef<S>,
        cluster_manager: ClusterManagerRef<S>,
        source_manager: SourceManagerRef<S>,
        catalog_manager: CatalogManagerRef<S>,
        ddl_lock: Arc<RwLock<()>>,
    ) -> Self {
        Self {
            barrier_manager,
            fragment_manager,
            cluster_manager,
            source_manager,
            catalog_manager,
            ddl_lock,
        }
    }
}

#[async_trait::async_trait]
impl<S> ScaleService for ScaleServiceImpl<S>
where
    S: MetaStore,
{
    #[cfg_attr(coverage, no_coverage)]
    async fn pause(&self, _: Request<PauseRequest>) -> Result<Response<PauseResponse>, Status> {
        let _ddl_lock = self.ddl_lock.write().await;
        self.barrier_manager.run_command(Command::pause()).await?;
        Ok(Response::new(PauseResponse {}))
    }

    #[cfg_attr(coverage, no_coverage)]
    async fn resume(&self, _: Request<ResumeRequest>) -> Result<Response<ResumeResponse>, Status> {
        let _ddl_lock = self.ddl_lock.write().await;
        self.barrier_manager.run_command(Command::resume()).await?;
        Ok(Response::new(ResumeResponse {}))
    }

    #[cfg_attr(coverage, no_coverage)]
    async fn get_cluster_info(
        &self,
        _: Request<GetClusterInfoRequest>,
    ) -> Result<Response<GetClusterInfoResponse>, Status> {
        let table_fragments = self
            .fragment_manager
            .list_table_fragments()
            .await?
            .iter()
            .map(|tf| tf.to_protobuf())
            .collect();

        let worker_nodes = self
            .cluster_manager
            .list_worker_node(WorkerType::ComputeNode, None)
            .await;

        let actor_splits = self
            .source_manager
            .get_actor_splits()
            .await
            .into_iter()
            .map(|(actor_id, splits)| {
                (
                    actor_id,
                    ConnectorSplits {
                        splits: splits.iter().map(ConnectorSplit::from).collect(),
                    },
                )
            })
            .collect();

        let sources = self.catalog_manager.list_sources().await?;

        let mut stream_source_infos = HashMap::new();
        for source in sources {
            if let Some(StreamSource(info)) = source.info {
                stream_source_infos.insert(source.id, info);
            }
        }

        Ok(Response::new(GetClusterInfoResponse {
            worker_nodes,
            table_fragments,
            actor_splits,
            stream_source_infos,
        }))
    }
}
