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

use std::time::Duration;

use risingwave_common::error::{ErrorCode, Result};
use risingwave_common::util::addr::HostAddr;
use risingwave_pb::common::WorkerType;
use risingwave_pb::meta::SubscribeResponse;
use risingwave_rpc_client::MetaClient;
use tokio::task::JoinHandle;
use tonic::Streaming;

/// `ObserverManager` is used to update data based on notification from meta.
/// Call `start` to spawn a new asynchronous task
/// We can write the notification logic by implementing `ObserverNodeImpl`.
pub struct ObserverManager {
    rx: Streaming<SubscribeResponse>,
    meta_client: MetaClient,
    addr: HostAddr,
    worker_type: WorkerType,
    observer_states: Box<dyn ObserverNodeImpl + Send>,
}

pub trait ObserverNodeImpl {
    /// modify data after receiving notification from meta
    fn handle_notification(&mut self, resp: SubscribeResponse);

    /// Initialize data from the meta. It will be called at start or resubscribe
    fn handle_initialization_notification(&mut self, resp: SubscribeResponse) -> Result<()>;
}

impl ObserverManager {
    pub async fn new(
        meta_client: MetaClient,
        addr: HostAddr,
        observer_states: Box<dyn ObserverNodeImpl + Send>,
        worker_type: WorkerType,
    ) -> Self {
        let rx = meta_client.subscribe(&addr, worker_type).await.unwrap();
        Self {
            rx,
            meta_client,
            addr,
            worker_type,
            observer_states,
        }
    }

    /// `start` is used to spawn a new asynchronous task which receives meta's notification and
    /// call the `handle_initialization_notification` and `handle_notification` to update node data.
    pub async fn start(mut self) -> Result<JoinHandle<()>> {
        let first_resp = self.rx.message().await?.ok_or_else(|| {
            ErrorCode::InternalError(
                "ObserverManager start failed, Stream of notification terminated at the start."
                    .to_string(),
            )
        })?;
        self.observer_states
            .handle_initialization_notification(first_resp)?;
        let handle = tokio::spawn(async move {
            loop {
                match self.rx.message().await {
                    Ok(resp) => {
                        if resp.is_none() {
                            tracing::error!("Stream of notification terminated.");
                            self.re_subscribe().await;
                            continue;
                        }
                        self.observer_states.handle_notification(resp.unwrap());
                    }
                    Err(e) => {
                        tracing::error!("Receives meta's notification err {:?}", e);
                        self.re_subscribe().await;
                    }
                }
            }
        });
        Ok(handle)
    }

    /// `re_subscribe` is used to re-subscribe to the meta's notification.
    async fn re_subscribe(&mut self) {
        loop {
            match self
                .meta_client
                .subscribe(&self.addr, self.worker_type)
                .await
            {
                Ok(rx) => {
                    tracing::debug!("re-subscribe success");
                    self.rx = rx;
                    if let Ok(Some(snapshot_resp)) = self.rx.message().await {
                        self.observer_states
                            .handle_initialization_notification(snapshot_resp)
                            .expect("handle snapshot notification failed after re-subscribe");
                        break;
                    }
                }
                Err(_) => {
                    tokio::time::sleep(RE_SUBSCRIBE_RETRY_INTERVAL).await;
                }
            }
        }
    }
}
const RE_SUBSCRIBE_RETRY_INTERVAL: Duration = Duration::from_millis(100);
