// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::ops::RangeBounds;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::{RwLock, RwLockWriteGuard};
use risingwave_common::catalog::TableId;
use risingwave_hummock_sdk::compaction_group::hummock_version_ext::HummockVersionUpdateExt;
use risingwave_hummock_sdk::key::TableKey;
use risingwave_hummock_sdk::CompactionGroupId;
use risingwave_pb::hummock::pin_version_response;
use risingwave_pb::hummock::pin_version_response::Payload;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::hummock::compactor::CompactorContext;
use crate::hummock::event_handler::hummock_event_handler::BufferTracker;
use crate::hummock::local_version::pinned_version::PinnedVersion;
use crate::hummock::local_version::{LocalVersion, ReadVersion, SyncUncommittedDataStage};
use crate::hummock::shared_buffer::shared_buffer_batch::SharedBufferBatch;
use crate::hummock::shared_buffer::shared_buffer_uploader::{
    SharedBufferUploader, UploadTaskPayload,
};
use crate::hummock::shared_buffer::OrderIndex;
use crate::hummock::utils::validate_table_key_range;
use crate::hummock::{HummockEpoch, HummockError, HummockResult, SstableIdManagerRef, TrackerId};
use crate::storage_value::StorageValue;
use crate::store::SyncResult;

pub type LocalVersionManagerRef = Arc<LocalVersionManager>;

/// The `LocalVersionManager` maintains a local copy of storage service's hummock version data.
/// By acquiring a `ScopedLocalVersion`, the `Sstables` of this version is guaranteed to be valid
/// during the lifetime of `ScopedLocalVersion`. Internally `LocalVersionManager` will pin/unpin the
/// versions in storage service.
pub struct LocalVersionManager {
    pub(crate) local_version: RwLock<LocalVersion>,
    buffer_tracker: BufferTracker,
    shared_buffer_uploader: Arc<SharedBufferUploader>,
    sstable_id_manager: SstableIdManagerRef,
}

impl LocalVersionManager {
    pub fn new(
        pinned_version: PinnedVersion,
        compactor_context: Arc<CompactorContext>,
        buffer_tracker: BufferTracker,
    ) -> Arc<Self> {
        assert!(pinned_version.is_valid());
        let sstable_id_manager = compactor_context.sstable_id_manager.clone();

        Arc::new(LocalVersionManager {
            local_version: RwLock::new(LocalVersion::new(pinned_version)),
            buffer_tracker,
            shared_buffer_uploader: Arc::new(SharedBufferUploader::new(compactor_context)),
            sstable_id_manager,
        })
    }

    pub fn get_buffer_tracker(&self) -> &BufferTracker {
        &self.buffer_tracker
    }

    /// Updates cached version if the new version is of greater id.
    /// You shouldn't unpin even the method returns false, as it is possible `hummock_version` is
    /// being referenced by some readers.
    pub fn try_update_pinned_version(
        &self,
        pin_resp_payload: pin_version_response::Payload,
    ) -> Option<PinnedVersion> {
        let old_version = self.local_version.read();
        let new_version_id = match &pin_resp_payload {
            Payload::VersionDeltas(version_deltas) => match version_deltas.version_deltas.last() {
                Some(version_delta) => version_delta.id,
                None => old_version.pinned_version().id(),
            },
            Payload::PinnedVersion(version) => version.get_id(),
        };

        if old_version.pinned_version().id() >= new_version_id {
            return None;
        }

        let newly_pinned_version = match pin_resp_payload {
            Payload::VersionDeltas(mut version_deltas) => {
                old_version.filter_local_sst(&mut version_deltas);
                let mut version_to_apply = old_version.pinned_version().version();
                for version_delta in &version_deltas.version_deltas {
                    assert_eq!(version_to_apply.id, version_delta.prev_id);
                    version_to_apply.apply_version_delta(version_delta);
                }
                version_to_apply
            }
            Payload::PinnedVersion(version) => version,
        };

        validate_table_key_range(&newly_pinned_version);

        drop(old_version);
        let mut new_version = self.local_version.write();
        // check again to prevent other thread changes new_version.
        if new_version.pinned_version().id() >= newly_pinned_version.get_id() {
            return None;
        }

        self.sstable_id_manager
            .remove_watermark_sst_id(TrackerId::Epoch(newly_pinned_version.max_committed_epoch));
        new_version.set_pinned_version(newly_pinned_version);
        let result = new_version.pinned_version().clone();
        RwLockWriteGuard::unlock_fair(new_version);

        Some(result)
    }

    pub fn write_shared_buffer_batch(&self, batch: SharedBufferBatch) {
        self.write_shared_buffer_inner(batch.epoch(), batch);
    }

    pub async fn write_shared_buffer(
        &self,
        epoch: HummockEpoch,
        kv_pairs: Vec<(Bytes, StorageValue)>,
        delete_ranges: Vec<(Bytes, Bytes)>,
        table_id: TableId,
    ) -> HummockResult<usize> {
        let sorted_items = SharedBufferBatch::build_shared_buffer_item_batches(kv_pairs);
        let size = SharedBufferBatch::measure_batch_size(&sorted_items);
        let batch = SharedBufferBatch::build_shared_buffer_batch(
            epoch,
            sorted_items,
            size,
            delete_ranges,
            table_id,
            Some(
                self.buffer_tracker
                    .get_memory_limiter()
                    .require_memory(size as u64)
                    .await,
            ),
        );
        let batch_size = batch.size();
        self.write_shared_buffer_inner(batch.epoch(), batch);
        Ok(batch_size)
    }

    pub(crate) fn write_shared_buffer_inner(&self, epoch: HummockEpoch, batch: SharedBufferBatch) {
        let mut local_version_guard = self.local_version.write();
        let sealed_epoch = local_version_guard.get_sealed_epoch();
        assert!(
            epoch > sealed_epoch,
            "write epoch must greater than max current epoch, write epoch {}, sealed epoch {}",
            epoch,
            sealed_epoch
        );
        // Write into shared buffer

        let shared_buffer = match local_version_guard.get_mut_shared_buffer(epoch) {
            Some(shared_buffer) => shared_buffer,
            None => local_version_guard
                .new_shared_buffer(epoch, self.buffer_tracker.global_upload_task_size().clone()),
        };
        // The batch will be synced to S3 asynchronously if it is a local batch
        shared_buffer.write_batch(batch);
    }

    /// Issue a concurrent upload task to flush some local shared buffer batch to object store.
    ///
    /// This method should only be called in the buffer tracker worker.
    ///
    /// Return:
    ///   - Some(task join handle) when there is new upload task
    ///   - None when there is no new task
    #[expect(dead_code)]
    pub(in crate::hummock) fn flush_shared_buffer(
        self: Arc<Self>,
    ) -> Option<(HummockEpoch, JoinHandle<()>)> {
        let (epoch, (order_index, payload, task_write_batch_size), compaction_group_index) = {
            let mut local_version_guard = self.local_version.write();

            // The current implementation is a trivial one, which issue only one flush task and wait
            // for the task to finish.
            let mut task = None;
            let compaction_group_index =
                local_version_guard.pinned_version.compaction_group_index();
            for (epoch, shared_buffer) in local_version_guard.iter_mut_unsynced_shared_buffer() {
                if let Some(upload_task) = shared_buffer.new_upload_task() {
                    task = Some((*epoch, upload_task, compaction_group_index));
                    break;
                }
            }
            match task {
                Some(task) => task,
                None => return None,
            }
        };

        let join_handle = tokio::spawn(async move {
            info!(
                "running flush task in epoch {} of size {}",
                epoch, task_write_batch_size
            );
            // TODO: may apply different `is_local` according to whether local spill is enabled.
            let _ = self
                .run_flush_upload_task(order_index, epoch, payload, compaction_group_index)
                .await
                .inspect_err(|err| {
                    error!(
                        "upload task fail. epoch: {}, order_index: {}. Err: {:?}",
                        epoch, order_index, err
                    );
                });
            info!(
                "flush task in epoch {} of size {} finished",
                epoch, task_write_batch_size
            );
        });
        Some((epoch, join_handle))
    }

    #[cfg(any(test, feature = "test"))]
    pub async fn sync_shared_buffer(&self, epoch: HummockEpoch) -> HummockResult<SyncResult> {
        self.seal_epoch(epoch, true);
        self.await_sync_shared_buffer(epoch).await
    }

    /// send event to `event_handler` thaen seal epoch in local version.
    pub fn seal_epoch(&self, epoch: HummockEpoch, is_checkpoint: bool) {
        self.local_version.write().seal_epoch(epoch, is_checkpoint);
    }

    pub async fn await_sync_shared_buffer(&self, epoch: HummockEpoch) -> HummockResult<SyncResult> {
        tracing::trace!("sync epoch {}", epoch);

        let future = {
            // Wait all epochs' task that less than epoch.
            let mut local_version_guard = self.local_version.write();
            let (payload, sync_size) = local_version_guard.start_syncing(epoch);
            let compaction_group_index = local_version_guard
                .pinned_version()
                .compaction_group_index();
            self.run_sync_upload_task(payload, compaction_group_index, sync_size, epoch)
        };
        future.await?;
        let local_version_guard = self.local_version.read();
        let sync_data = local_version_guard
            .sync_uncommitted_data
            .get(&epoch)
            .expect("should exist");
        match sync_data.stage() {
            SyncUncommittedDataStage::CheckpointEpochSealed(_)
            | SyncUncommittedDataStage::Syncing(_) => {
                unreachable!("when a join handle is finished, the stage should be synced or failed")
            }
            SyncUncommittedDataStage::Failed(_) => Err(HummockError::other("sync task failed")),
            SyncUncommittedDataStage::Synced(ssts, sync_size) => {
                let ssts = ssts.clone();
                let sync_size = *sync_size;
                drop(local_version_guard);
                Ok(SyncResult {
                    sync_size,
                    uncommitted_ssts: ssts,
                })
            }
        }
    }

    pub async fn run_sync_upload_task(
        &self,
        task_payload: UploadTaskPayload,
        compaction_group_index: Arc<HashMap<TableId, CompactionGroupId>>,
        sync_size: usize,
        epoch: HummockEpoch,
    ) -> HummockResult<()> {
        match self
            .shared_buffer_uploader
            .flush(task_payload, epoch, compaction_group_index)
            .await
        {
            Ok(ssts) => {
                self.local_version
                    .write()
                    .data_synced(epoch, ssts, sync_size);
                Ok(())
            }
            Err(e) => {
                self.local_version.write().fail_epoch_sync(epoch);
                Err(e)
            }
        }
    }

    #[expect(dead_code)]
    async fn run_flush_upload_task(
        &self,
        order_index: OrderIndex,
        epoch: HummockEpoch,
        task_payload: UploadTaskPayload,
        compaction_group_index: Arc<HashMap<TableId, CompactionGroupId>>,
    ) -> HummockResult<()> {
        let task_result = self
            .shared_buffer_uploader
            .flush(task_payload, epoch, compaction_group_index)
            .await;

        let mut local_version_guard = self.local_version.write();
        let shared_buffer_guard = local_version_guard
            .get_mut_shared_buffer(epoch)
            .expect("shared buffer should exist since some uncommitted data is not committed yet");

        match task_result {
            Ok(ssts) => {
                shared_buffer_guard.succeed_upload_task(order_index, ssts);
                Ok(())
            }
            Err(e) => {
                shared_buffer_guard.fail_upload_task(order_index);
                Err(e)
            }
        }
    }

    pub fn read_filter<R, B>(
        self: &LocalVersionManager,
        read_epoch: HummockEpoch,
        table_id: TableId,
        table_key_range: &R,
    ) -> ReadVersion
    where
        R: RangeBounds<TableKey<B>>,
        B: AsRef<[u8]>,
    {
        LocalVersion::read_filter(&self.local_version, read_epoch, table_id, table_key_range)
    }

    pub fn get_pinned_version(&self) -> PinnedVersion {
        self.local_version.read().pinned_version().clone()
    }

    pub(crate) fn buffer_tracker(&self) -> &BufferTracker {
        &self.buffer_tracker
    }

    pub fn sstable_id_manager(&self) -> SstableIdManagerRef {
        self.sstable_id_manager.clone()
    }
}

// concurrent worker thread of `LocalVersionManager`
impl LocalVersionManager {
    pub fn clear_shared_buffer(&self) {
        self.local_version.write().shared_buffer.clear();
        self.local_version.write().sync_uncommitted_data.clear();
    }
}

#[cfg(any(test, feature = "test"))]
// Some method specially for tests of `LocalVersionManager`
impl LocalVersionManager {
    pub fn local_version(&self) -> &RwLock<LocalVersion> {
        &self.local_version
    }

    pub fn get_local_version(&self) -> LocalVersion {
        self.local_version.read().clone()
    }

    pub fn get_shared_buffer_size(&self) -> usize {
        self.buffer_tracker.get_buffer_size()
    }
}
