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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use risingwave_common::util::sync_point::on_sync_point;
use risingwave_hummock_sdk::compact::compact_task_to_string;
use risingwave_hummock_sdk::CompactionGroupId;
use risingwave_pb::hummock::subscribe_compact_tasks_response::Task;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot::Receiver;

use crate::hummock::error::Error;
use crate::hummock::{CompactorManagerRef, HummockManagerRef};
use crate::manager::MetaSrvEnv;
use crate::storage::MetaStore;

pub type CompactionSchedulerRef<S> = Arc<CompactionScheduler<S>>;

pub type CompactionRequestChannelRef = Arc<CompactionRequestChannel>;
/// [`CompactionRequestChannel`] wrappers a mpsc channel and deduplicate requests from same
/// compaction groups.
pub struct CompactionRequestChannel {
    request_tx: UnboundedSender<CompactionGroupId>,
    scheduled: Mutex<HashSet<CompactionGroupId>>,
}

impl CompactionRequestChannel {
    fn new(request_tx: UnboundedSender<CompactionGroupId>) -> Self {
        Self {
            request_tx,
            scheduled: Default::default(),
        }
    }

    /// Enqueues only if the target is not yet in queue.
    pub fn try_send(&self, compaction_group: CompactionGroupId) -> bool {
        let mut guard = self.scheduled.lock();
        if guard.contains(&compaction_group) {
            return false;
        }
        if self.request_tx.send(compaction_group).is_err() {
            return false;
        }
        guard.insert(compaction_group);
        true
    }

    fn unschedule(&self, compaction_group: CompactionGroupId) {
        self.scheduled.lock().remove(&compaction_group);
    }
}

/// Schedules compaction task picking and assignment.
pub struct CompactionScheduler<S>
where
    S: MetaStore,
{
    hummock_manager: HummockManagerRef<S>,
    compactor_manager: CompactorManagerRef,
    compactor_selection_retry_interval_sec: u64,
}

impl<S> CompactionScheduler<S>
where
    S: MetaStore,
{
    pub fn new(
        env: MetaSrvEnv<S>,
        hummock_manager: HummockManagerRef<S>,
        compactor_manager: CompactorManagerRef,
    ) -> Self {
        Self {
            hummock_manager,
            compactor_manager,
            compactor_selection_retry_interval_sec: env.opts.compactor_selection_retry_interval_sec,
        }
    }

    pub async fn start(&self, mut shutdown_rx: Receiver<()>) {
        let (request_tx, mut request_rx) =
            tokio::sync::mpsc::unbounded_channel::<CompactionGroupId>();
        let request_channel = Arc::new(CompactionRequestChannel::new(request_tx));
        self.hummock_manager
            .set_compaction_scheduler(request_channel.clone());
        tracing::info!("Start compaction scheduler.");
        'compaction_trigger: loop {
            let compaction_group: CompactionGroupId = tokio::select! {
                compaction_group = request_rx.recv() => {
                    match compaction_group {
                        Some(compaction_group) => compaction_group,
                        None => {
                            break 'compaction_trigger;
                        }
                    }
                },
                // Shutdown compactor
                _ = &mut shutdown_rx => {
                    break 'compaction_trigger;
                }
            };
            on_sync_point("BEFORE_SCHEDULE_COMPACTION_TASK")
                .await
                .unwrap();
            self.pick_and_assign(compaction_group, request_channel.clone())
                .await;
        }
        tracing::info!("Compaction scheduler is stopped");
    }

    async fn pick_and_assign(
        &self,
        compaction_group: CompactionGroupId,
        request_channel: Arc<CompactionRequestChannel>,
    ) -> bool {
        // 1. Pick a compaction task.
        let compact_task = self
            .hummock_manager
            .get_compact_task(compaction_group)
            .await;
        request_channel.unschedule(compaction_group);
        let mut compact_task = match compact_task {
            Ok(Some(compact_task)) => compact_task,
            Ok(None) => {
                // No compaction task available.
                return false;
            }
            Err(err) => {
                tracing::warn!("Failed to get compaction task: {:#?}.", err);
                return false;
            }
        };
        tracing::trace!(
            "Picked compaction task. {}",
            compact_task_to_string(&compact_task)
        );

        // 2. Assign the compaction task to a compactor.
        'send_task: loop {
            // 2.1 Select a compactor.
            let compactor = match self
                .compactor_manager
                .next_idle_compactor(&self.hummock_manager)
                .await
            {
                None => {
                    let current_compactor_tasks =
                        self.hummock_manager.list_assigned_tasks_number().await;
                    tracing::warn!("No idle compactor available. The assigned task number for every compactor is (context_id, count):\n {:?}", current_compactor_tasks);
                    compact_task.task_status = false;
                    tokio::time::sleep(Duration::from_secs(
                        self.compactor_selection_retry_interval_sec,
                    ))
                    .await;
                    match self
                        .hummock_manager
                        .cancel_compact_task(&compact_task)
                        .await
                    {
                        Ok(_) => return false,
                        // failed to cancel, try assign to compactor again.
                        Err(_) => continue 'send_task,
                    }
                }
                Some(compactor) => compactor,
            };

            // 2.2 Assign the compaction task.
            match self
                .hummock_manager
                .assign_compaction_task(&compact_task, compactor.context_id())
                .await
            {
                Ok(_) => {
                    tracing::trace!(
                        "Assigned compaction task. {}",
                        compact_task_to_string(&compact_task)
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        "Failed to assign compaction task to compactor {}: {:#?}",
                        compactor.context_id(),
                        err
                    );
                    match err {
                        Error::InvalidContext(_) | Error::CompactorUnreachable(_) => {
                            self.compactor_manager
                                .remove_compactor(compactor.context_id());
                        }
                        _ => {}
                    }
                    continue 'send_task;
                }
            }

            // 2.3 Send the compaction task.
            if let Err(e) = compactor
                .send_task(Task::CompactTask(compact_task.clone()))
                .await
            {
                tracing::warn!(
                    "Failed to send task {} to {}. {:#?}",
                    compact_task.task_id,
                    compactor.context_id(),
                    e
                );
                // Cancel the task at best effort
                compact_task.task_status = false;
                if let Err(e) = self
                    .hummock_manager
                    .report_compact_task(compactor.context_id(), &compact_task)
                    .await
                {
                    tracing::error!("Failed to cancel task {}. {:#?}", compact_task.task_id, e);
                    // TODO #3677: handle cancellation via compaction heartbeat after #4496
                    return false;
                }
                continue 'send_task;
            }

            // Reschedule it in case there are more tasks from this compaction group.
            request_channel.try_send(compaction_group);

            return true;
        }
    }
}
