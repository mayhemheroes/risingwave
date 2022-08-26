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

use std::borrow::{Borrow, BorrowMut};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound::{Excluded, Included};
use std::ops::DerefMut;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use function_name::named;
use itertools::Itertools;
use prost::Message;
use risingwave_common::monitor::rwlock::MonitoredRwLock;
use risingwave_common::util::epoch::{Epoch, INVALID_EPOCH};
use risingwave_hummock_sdk::compact::compact_task_to_string;
use risingwave_hummock_sdk::compaction_group::hummock_version_ext::HummockVersionExt;
use risingwave_hummock_sdk::{
    CompactionGroupId, HummockCompactionTaskId, HummockContextId, HummockEpoch, HummockSstableId,
    HummockVersionId, LocalSstableInfo, SstIdRange, FIRST_VERSION_ID,
};
use risingwave_pb::hummock::hummock_version::Levels;
use risingwave_pb::hummock::subscribe_compact_tasks_response::Task;
use risingwave_pb::hummock::{
    pin_version_response, CompactTask, CompactTaskAssignment, HummockPinnedSnapshot,
    HummockPinnedVersion, HummockSnapshot, HummockVersion, HummockVersionDelta, Level, LevelDelta,
    LevelType, OverlappingLevel,
};
use risingwave_pb::meta::subscribe_response::{Info, Operation};
use risingwave_pb::meta::MetaLeaderInfo;
use tokio::sync::{RwLockReadGuard, RwLockWriteGuard};

use crate::hummock::compaction::{CompactStatus, ManualCompactionOption};
use crate::hummock::compaction_group::manager::CompactionGroupManagerRef;
use crate::hummock::compaction_scheduler::CompactionRequestChannelRef;
use crate::hummock::error::{Error, Result};
use crate::hummock::metrics_utils::{trigger_commit_stat, trigger_sst_stat};
use crate::hummock::CompactorManagerRef;
use crate::manager::{ClusterManagerRef, IdCategory, MetaSrvEnv, META_NODE_ID};
use crate::model::{BTreeMapTransaction, MetadataModel, ValTransaction, VarTransaction};
use crate::rpc::metrics::MetaMetrics;
use crate::rpc::{META_CF_NAME, META_LEADER_KEY};
use crate::storage::{MetaStore, Transaction};

mod context;
mod gc;
#[cfg(test)]
mod tests;
mod versioning;
use versioning::*;
mod compaction;
use compaction::*;

// Update to states are performed as follow:
// - Initialize ValTransaction for the meta state to update
// - Make changes on the ValTransaction.
// - Call `commit_multi_var` to commit the changes via meta store transaction. If transaction
//   succeeds, the in-mem state will be updated by the way.
pub struct HummockManager<S: MetaStore> {
    env: MetaSrvEnv<S>,
    cluster_manager: ClusterManagerRef<S>,
    compaction_group_manager: CompactionGroupManagerRef<S>,
    // When trying to locks compaction and versioning at the same time, compaction lock should
    // be requested before versioning lock.
    compaction: MonitoredRwLock<Compaction>,
    versioning: MonitoredRwLock<Versioning>,
    max_committed_epoch: AtomicU64,

    metrics: Arc<MetaMetrics>,

    // `compaction_scheduler` is used to schedule a compaction for specified CompactionGroupId
    compaction_scheduler: parking_lot::RwLock<Option<CompactionRequestChannelRef>>,

    compactor_manager: CompactorManagerRef,
}

pub type HummockManagerRef<S> = Arc<HummockManager<S>>;

/// Commit multiple `ValTransaction`s to state store and upon success update the local in-mem state
/// by the way
/// After called, the `ValTransaction` will be dropped.
macro_rules! commit_multi_var {
    ($hummock_mgr:expr, $context_id:expr, $($val_txn:expr),*) => {
        {
            async {
                let mut trx = Transaction::default();
                // Apply the change in `ValTransaction` to trx
                $(
                    $val_txn.apply_to_txn(&mut trx)?;
                )*
                // Commit to state store
                $hummock_mgr.commit_trx($hummock_mgr.env.meta_store(), trx, $context_id, $hummock_mgr.env.get_leader_info())
                .await?;
                // Upon successful commit, commit the change to local in-mem state
                $(
                    $val_txn.commit();
                )*
                Result::Ok(())
            }.await
        }
    };
}
pub(crate) use commit_multi_var;

/// Acquire read lock of the lock with `lock_name`.
/// The macro will use macro `function_name` to get the name of the function of method that calls
/// the lock, and therefore, anyone to call this macro should ensured that the caller method has the
/// macro #[named]
macro_rules! read_lock {
    ($hummock_mgr:expr, $lock_name:ident) => {
        async {
            $hummock_mgr
                .$lock_name
                .read(&[function_name!(), stringify!($lock_name), "read"])
                .await
        }
    };
}
pub(crate) use read_lock;
use risingwave_pb::hummock::pin_version_response::{HummockVersionDeltas, Payload};

/// Acquire write lock of the lock with `lock_name`.
/// The macro will use macro `function_name` to get the name of the function of method that calls
/// the lock, and therefore, anyone to call this macro should ensured that the caller method has the
/// macro #[named]
macro_rules! write_lock {
    ($hummock_mgr:expr, $lock_name:ident) => {
        async {
            $hummock_mgr
                .$lock_name
                .write(&[function_name!(), stringify!($lock_name), "write"])
                .await
        }
    };
}
pub(crate) use write_lock;

macro_rules! start_measure_real_process_timer {
    ($hummock_mgr:expr) => {
        $hummock_mgr
            .metrics
            .hummock_manager_real_process_time
            .with_label_values(&[function_name!()])
            .start_timer()
    };
}
pub(crate) use start_measure_real_process_timer;

impl<S> HummockManager<S>
where
    S: MetaStore,
{
    pub async fn new(
        env: MetaSrvEnv<S>,
        cluster_manager: ClusterManagerRef<S>,
        metrics: Arc<MetaMetrics>,
        compaction_group_manager: CompactionGroupManagerRef<S>,
        compactor_manager: CompactorManagerRef,
    ) -> Result<HummockManager<S>> {
        let instance = HummockManager {
            env,
            versioning: MonitoredRwLock::new(
                metrics.hummock_manager_lock_time.clone(),
                Default::default(),
            ),
            compaction: MonitoredRwLock::new(
                metrics.hummock_manager_lock_time.clone(),
                Default::default(),
            ),
            metrics,
            cluster_manager,
            compaction_group_manager,
            compaction_scheduler: parking_lot::RwLock::new(None),
            compactor_manager,
            max_committed_epoch: AtomicU64::new(0),
        };

        instance.load_meta_store_state().await?;
        instance.release_invalid_contexts().await?;
        instance.cancel_unassigned_compaction_task().await?;
        // Release snapshots pinned by meta on restarting.
        instance.release_contexts([META_NODE_ID]).await?;
        Ok(instance)
    }

    /// Load state from meta store.
    #[named]
    async fn load_meta_store_state(&self) -> Result<()> {
        let mut compaction_guard = write_lock!(self, compaction).await;
        let mut versioning_guard = write_lock!(self, versioning).await;
        self.load_meta_store_state_impl(
            compaction_guard.borrow_mut(),
            versioning_guard.borrow_mut(),
        )
        .await
    }

    /// Load state from meta store.
    async fn load_meta_store_state_impl(
        &self,
        compaction_guard: &mut RwLockWriteGuard<'_, Compaction>,
        versioning_guard: &mut RwLockWriteGuard<'_, Versioning>,
    ) -> Result<()> {
        let compaction_statuses = CompactStatus::list(self.env.meta_store())
            .await?
            .into_iter()
            .map(|cg| (cg.compaction_group_id(), cg))
            .collect::<BTreeMap<CompactionGroupId, CompactStatus>>();
        if !compaction_statuses.is_empty() {
            compaction_guard.compaction_statuses = compaction_statuses;
        } else {
            // Initialize compact status for each compaction group
            let mut compaction_statuses =
                BTreeMapTransaction::new(&mut compaction_guard.compaction_statuses);
            for compaction_group in self.compaction_group_manager.compaction_groups().await {
                let compact_status = CompactStatus::new(
                    compaction_group.group_id(),
                    Arc::new(compaction_group.compaction_config().clone()),
                );
                compaction_statuses.insert(compact_status.compaction_group_id(), compact_status);
            }
            commit_multi_var!(self, None, compaction_statuses)?;
        }
        compaction_guard.compact_task_assignment =
            CompactTaskAssignment::list(self.env.meta_store())
                .await?
                .into_iter()
                .map(|assigned| (assigned.key().unwrap(), assigned))
                .collect();

        let versions = HummockVersion::list(self.env.meta_store()).await?;

        let hummock_version_deltas: BTreeMap<_, _> =
            HummockVersionDelta::list(self.env.meta_store())
                .await?
                .into_iter()
                .map(|version_delta| (version_delta.id, version_delta))
                .collect();

        // Insert the initial version.
        let mut redo_state = if versions.is_empty() {
            let mut init_version = HummockVersion {
                id: FIRST_VERSION_ID,
                levels: Default::default(),
                max_committed_epoch: INVALID_EPOCH,
                safe_epoch: INVALID_EPOCH,
            };
            // Initialize independent levels via corresponding compaction group' config.
            for compaction_group in self.compaction_group_manager.compaction_groups().await {
                let mut levels = vec![];
                for l in 0..compaction_group.compaction_config().max_level {
                    levels.push(Level {
                        level_idx: (l + 1) as u32,
                        level_type: LevelType::Nonoverlapping as i32,
                        table_infos: vec![],
                        total_file_size: 0,
                        sub_level_id: 0,
                    });
                }
                init_version.levels.insert(
                    compaction_group.group_id(),
                    Levels {
                        levels,
                        l0: Some(OverlappingLevel {
                            sub_levels: vec![],
                            total_file_size: 0,
                        }),
                    },
                );
            }
            init_version.insert(self.env.meta_store()).await?;
            init_version
        } else {
            versions.first().unwrap().clone()
        };
        versioning_guard.checkpoint_version = redo_state.clone();

        for version_delta in hummock_version_deltas.values() {
            if version_delta.prev_id == redo_state.id {
                redo_state.apply_version_delta(version_delta);
            }
        }
        self.max_committed_epoch
            .store(redo_state.max_committed_epoch, Ordering::Relaxed);
        versioning_guard.current_version = redo_state;
        versioning_guard.hummock_version_deltas = hummock_version_deltas;

        versioning_guard.pinned_versions = HummockPinnedVersion::list(self.env.meta_store())
            .await?
            .into_iter()
            .map(|p| (p.context_id, p))
            .collect();
        versioning_guard.pinned_snapshots = HummockPinnedSnapshot::list(self.env.meta_store())
            .await?
            .into_iter()
            .map(|p| (p.context_id, p))
            .collect();

        let checkpoint_id = versioning_guard.checkpoint_version.id;
        versioning_guard.ssts_to_delete.clear();
        versioning_guard.extend_ssts_to_delete_from_deltas(..=checkpoint_id);
        let preserved_deltas: HashSet<HummockVersionId> =
            HashSet::from_iter(versioning_guard.ssts_to_delete.values().cloned());
        versioning_guard.deltas_to_delete = versioning_guard
            .hummock_version_deltas
            .keys()
            .cloned()
            .filter(|id| {
                *id <= versioning_guard.checkpoint_version.id && !preserved_deltas.contains(id)
            })
            .collect_vec();

        Ok(())
    }

    /// We use worker node id as the `context_id`.
    /// If the `context_id` is provided, the transaction will abort if the `context_id` is not
    /// valid, which means the worker node is not a valid member of the cluster.
    /// This operation is protected by mutex of compaction, so that other thread can not
    /// call `release_contexts` even if it has removed `context_id` from cluster manager.
    async fn commit_trx(
        &self,
        meta_store: &S,
        mut trx: Transaction,
        context_id: Option<HummockContextId>,
        info: MetaLeaderInfo,
    ) -> Result<()> {
        if let Some(context_id) = context_id {
            if context_id == META_NODE_ID {
                // Using the preserved meta id is allowed.
            } else if !self.check_context(context_id).await {
                // The worker is not found in cluster.
                return Err(Error::InvalidContext(context_id));
            }
        }
        trx.check_equal(
            META_CF_NAME.to_owned(),
            META_LEADER_KEY.as_bytes().to_vec(),
            info.encode_to_vec(),
        );
        meta_store.txn(trx).await.map_err(Into::into)
    }

    /// Pin the current greatest hummock version. The pin belongs to `context_id`
    /// and will be unpinned when `context_id` is invalidated.
    #[named]
    pub async fn pin_version(
        &self,
        context_id: HummockContextId,
        last_pinned: HummockVersionId,
    ) -> Result<pin_version_response::Payload> {
        let mut versioning_guard = write_lock!(self, versioning).await;
        let _timer = start_measure_real_process_timer!(self);
        let versioning = versioning_guard.deref_mut();
        let mut pinned_versions = BTreeMapTransaction::new(&mut versioning.pinned_versions);
        let mut context_pinned_version = pinned_versions.new_entry_txn_or_default(
            context_id,
            HummockPinnedVersion {
                context_id,
                min_pinned_id: 0,
            },
        );

        let version_id = versioning.current_version.id;

        let ret = {
            if last_pinned <= version_id
                && versioning.hummock_version_deltas.contains_key(&last_pinned)
            {
                Payload::VersionDeltas(HummockVersionDeltas {
                    delta: versioning
                        .hummock_version_deltas
                        .range((Excluded(last_pinned), Included(version_id)))
                        .map(|(_, delta)| delta.clone())
                        .collect_vec(),
                })
            } else {
                Payload::PinnedVersion(versioning.current_version.clone())
            }
        };

        if context_pinned_version.min_pinned_id == 0 {
            context_pinned_version.min_pinned_id = version_id;
            commit_multi_var!(self, Some(context_id), context_pinned_version)?;
        }

        #[cfg(test)]
        {
            drop(versioning_guard);
            self.check_state_consistency().await;
        }

        Ok(ret)
    }

    /// Unpin all pins which belongs to `context_id` and has an id which is older than
    /// `unpin_before`. All versions >= `unpin_before` will be treated as if they are all pinned by
    /// this `context_id` so they will not be vacummed.
    #[named]
    pub async fn unpin_version_before(
        &self,
        context_id: HummockContextId,
        unpin_before: HummockVersionId,
    ) -> Result<()> {
        let mut versioning_guard = write_lock!(self, versioning).await;
        let _timer = start_measure_real_process_timer!(self);
        let versioning = versioning_guard.deref_mut();
        let mut pinned_versions = BTreeMapTransaction::new(&mut versioning.pinned_versions);
        let mut context_pinned_version = pinned_versions.new_entry_txn_or_default(
            context_id,
            HummockPinnedVersion {
                context_id,
                min_pinned_id: 0,
            },
        );

        context_pinned_version.min_pinned_id = unpin_before;
        commit_multi_var!(self, Some(context_id), context_pinned_version)?;

        #[cfg(test)]
        {
            drop(versioning_guard);
            self.check_state_consistency().await;
        }

        Ok(())
    }

    /// Remove this context from context pin info.
    #[named]
    pub async fn unpin_version(&self, context_id: HummockContextId) -> Result<()> {
        let mut versioning_guard = write_lock!(self, versioning).await;
        let _timer = start_measure_real_process_timer!(self);
        let mut pinned_versions = BTreeMapTransaction::new(&mut versioning_guard.pinned_versions);
        let release_version = pinned_versions.remove(context_id);
        if release_version.is_some() {
            commit_multi_var!(self, Some(context_id), pinned_versions)?;
        }

        #[cfg(test)]
        {
            drop(versioning_guard);
            self.check_state_consistency().await;
        }

        Ok(())
    }

    /// Make sure `max_committed_epoch` is pinned and return it.
    #[named]
    pub async fn pin_snapshot(&self, context_id: HummockContextId) -> Result<HummockSnapshot> {
        let max_committed_epoch = self.max_committed_epoch.load(Ordering::Relaxed);
        let mut guard = write_lock!(self, versioning).await;
        let _timer = start_measure_real_process_timer!(self);
        let mut pinned_snapshots = BTreeMapTransaction::new(&mut guard.pinned_snapshots);
        let mut context_pinned_snapshot = pinned_snapshots.new_entry_txn_or_default(
            context_id,
            HummockPinnedSnapshot {
                context_id,
                minimal_pinned_snapshot: INVALID_EPOCH,
            },
        );
        if context_pinned_snapshot.minimal_pinned_snapshot == INVALID_EPOCH {
            context_pinned_snapshot.minimal_pinned_snapshot = max_committed_epoch;
            commit_multi_var!(self, Some(context_id), context_pinned_snapshot)?;
        }

        Ok(HummockSnapshot {
            epoch: max_committed_epoch,
        })
    }

    pub fn get_last_epoch(&self) -> Result<HummockSnapshot> {
        let max_committed_epoch = self.max_committed_epoch.load(Ordering::Relaxed);
        Ok(HummockSnapshot {
            epoch: max_committed_epoch,
        })
    }

    #[named]
    pub async fn unpin_snapshot(&self, context_id: HummockContextId) -> Result<()> {
        let mut versioning_guard = write_lock!(self, versioning).await;
        let _timer = start_measure_real_process_timer!(self);
        let mut pinned_snapshots = BTreeMapTransaction::new(&mut versioning_guard.pinned_snapshots);
        let release_snapshot = pinned_snapshots.remove(context_id);

        if release_snapshot.is_some() {
            commit_multi_var!(self, Some(context_id), pinned_snapshots)?;
        }

        #[cfg(test)]
        {
            drop(versioning_guard);
            self.check_state_consistency().await;
        }

        Ok(())
    }

    /// Unpin all snapshots smaller than specified epoch for current context.
    #[named]
    pub async fn unpin_snapshot_before(
        &self,
        context_id: HummockContextId,
        hummock_snapshot: HummockSnapshot,
    ) -> Result<()> {
        let mut versioning_guard = write_lock!(self, versioning).await;
        let _timer = start_measure_real_process_timer!(self);
        // Use the max_committed_epoch in storage as the snapshot ts so only committed changes are
        // visible in the snapshot.
        let max_committed_epoch = versioning_guard.current_version.max_committed_epoch;
        // Ensure the unpin will not clean the latest one.
        #[cfg(not(test))]
        {
            assert!(hummock_snapshot.epoch <= max_committed_epoch);
        }
        let last_read_epoch = std::cmp::min(hummock_snapshot.epoch, max_committed_epoch);

        let mut pinned_snapshots = BTreeMapTransaction::new(&mut versioning_guard.pinned_snapshots);
        let mut context_pinned_snapshot = pinned_snapshots.new_entry_txn_or_default(
            context_id,
            HummockPinnedSnapshot {
                context_id,
                minimal_pinned_snapshot: INVALID_EPOCH,
            },
        );

        // Unpin the snapshots pinned by meta but frontend doesn't know. Also equal to unpin all
        // epochs below specific watermark.
        if context_pinned_snapshot.minimal_pinned_snapshot < last_read_epoch
            || context_pinned_snapshot.minimal_pinned_snapshot == INVALID_EPOCH
        {
            context_pinned_snapshot.minimal_pinned_snapshot = last_read_epoch;
            commit_multi_var!(self, Some(context_id), context_pinned_snapshot)?;
        }

        #[cfg(test)]
        {
            drop(versioning_guard);
            self.check_state_consistency().await;
        }

        Ok(())
    }

    #[named]
    pub async fn get_compact_task_impl(
        &self,
        compaction_group_id: CompactionGroupId,
        manual_compaction_option: Option<ManualCompactionOption>,
    ) -> Result<Option<CompactTask>> {
        let mut compaction_guard = write_lock!(self, compaction).await;
        let compaction = compaction_guard.deref_mut();
        let start_time = Instant::now();
        // StoredIdGenerator already implements ids pre-allocation by ID_PREALLOCATE_INTERVAL.
        let task_id = self
            .env
            .id_gen_manager()
            .generate::<{ IdCategory::HummockCompactionTask }>()
            .await?;
        let mut compact_status = VarTransaction::new(
            compaction
                .compaction_statuses
                .get_mut(&compaction_group_id)
                .ok_or(Error::InvalidCompactionGroup(compaction_group_id))?,
        );
        let (current_version, watermark) = {
            let versioning_guard = read_lock!(self, versioning).await;
            let max_committed_epoch = versioning_guard.current_version.max_committed_epoch;
            let watermark = versioning_guard
                .pinned_snapshots
                .values()
                .map(|v| v.minimal_pinned_snapshot)
                .fold(max_committed_epoch, std::cmp::min);
            (versioning_guard.current_version.clone(), watermark)
        };
        let can_trivial_move = manual_compaction_option.is_none();
        let compact_task = compact_status.get_compact_task(
            current_version.get_compaction_group_levels(compaction_group_id),
            task_id as HummockCompactionTaskId,
            compaction_group_id,
            manual_compaction_option,
        );
        let mut compact_task = match compact_task {
            None => {
                return Ok(None);
            }
            Some(task) => task,
        };
        compact_task.watermark = watermark;

        if CompactStatus::is_trivial_move_task(&compact_task) && can_trivial_move {
            compact_status.report_compact_task(&compact_task);
            compact_task.sorted_output_ssts = compact_task.input_ssts[0].table_infos.clone();
            let mut versioning_guard = write_lock!(self, versioning).await;

            // need to regain the newest version ynder the protection of a write lock, otherwise the
            // old version may be used to overwrite the new one
            let current_version = versioning_guard.current_version.clone();
            let new_version_id = current_version.id + 1;
            let versioning = versioning_guard.deref_mut();
            let mut hummock_version_deltas =
                BTreeMapTransaction::new(&mut versioning.hummock_version_deltas);
            apply_version_delta(
                &mut hummock_version_deltas,
                &current_version,
                &compact_task,
                true,
            );

            // FIXME: refactor the implementation of `apply_compact_result` by using
            // `hummock_version_deltas`
            let mut new_version =
                CompactStatus::apply_compact_result(&compact_task, current_version);
            new_version.id = new_version_id;
            commit_multi_var!(self, None, hummock_version_deltas)?;
            // this task has been finished and does not need to be schedule.
            compact_task.task_status = true;
            versioning.current_version = new_version;
            trigger_sst_stat(
                &self.metrics,
                Some(
                    compaction
                        .compaction_statuses
                        .get(&compaction_group_id)
                        .ok_or(Error::InvalidCompactionGroup(compaction_group_id))?,
                ),
                &versioning.current_version,
                compaction_group_id,
            );
        } else {
            let existing_table_ids_from_meta = self
                .compaction_group_manager
                .internal_table_ids_by_compaction_group_id(compaction_group_id)
                .await?;

            // to get all relational table_id from sst_info
            let table_ids = compact_task
                .input_ssts
                .iter()
                .flat_map(|level| {
                    level
                        .table_infos
                        .iter()
                        .flat_map(|sst_info| sst_info.table_ids.iter().cloned())
                        .collect_vec()
                })
                .collect::<HashSet<u32>>();
            for table_id in table_ids {
                // to found exist table_id from
                if existing_table_ids_from_meta.contains(&table_id) {
                    compact_task.existing_table_ids.push(table_id);
                }
            }

            // build table_options
            let compaction_group = self
                .compaction_group_manager
                .compaction_group(compaction_group_id)
                .await
                .unwrap();
            compact_task.table_options = compaction_group
                .table_id_to_options()
                .iter()
                .filter(|id_to_option| compact_task.existing_table_ids.contains(id_to_option.0))
                .map(|id_to_option| (*id_to_option.0, id_to_option.1.into()))
                .collect();
            compact_task.current_epoch_time = Epoch::now().0;

            compact_task.compaction_filter_mask =
                compact_status.compaction_config.compaction_filter_mask;
            commit_multi_var!(self, None, compact_status)?;
            tracing::trace!(
                    "For compaction group {}: pick up {} tables in level {} to compact.  cost time: {:?}",
                    compaction_group_id,
                    compact_task.input_ssts[0].table_infos.len(),
                    compact_task.input_ssts[0].level_idx,
                    start_time.elapsed()
                );
            // this task has been finished.
            compact_task.task_status = false;
            trigger_sst_stat(
                &self.metrics,
                Some(
                    compaction
                        .compaction_statuses
                        .get(&compaction_group_id)
                        .ok_or(Error::InvalidCompactionGroup(compaction_group_id))?,
                ),
                &current_version,
                compaction_group_id,
            );
        }
        #[cfg(test)]
        {
            drop(compaction_guard);
            self.check_state_consistency().await;
        }

        Ok(Some(compact_task))
    }

    pub async fn cancel_compact_task(&self, compact_task: &CompactTask) -> Result<bool> {
        self.report_compact_task_impl(None, compact_task).await
    }

    pub async fn get_compact_task(
        &self,
        compaction_group_id: CompactionGroupId,
    ) -> Result<Option<CompactTask>> {
        while let Some(task) = self
            .get_compact_task_impl(compaction_group_id, None)
            .await?
        {
            if !task.task_status {
                return Ok(Some(task));
            }
            assert!(CompactStatus::is_trivial_move_task(&task));
        }
        Ok(None)
    }

    pub async fn manual_get_compact_task(
        &self,
        compaction_group_id: CompactionGroupId,
        manual_compaction_option: ManualCompactionOption,
    ) -> Result<Option<CompactTask>> {
        self.get_compact_task_impl(compaction_group_id, Some(manual_compaction_option))
            .await
    }

    /// Assigns a compaction task to a compactor
    #[named]
    pub async fn assign_compaction_task(
        &self,
        compact_task: &CompactTask,
        assignee_context_id: HummockContextId,
    ) -> Result<()> {
        let mut compaction_guard = write_lock!(self, compaction).await;
        let _timer = start_measure_real_process_timer!(self);
        let compaction = compaction_guard.deref_mut();
        let mut compact_task_assignment =
            BTreeMapTransaction::new(&mut compaction.compact_task_assignment);
        if let Some(assignment) = compact_task_assignment.get(&compact_task.task_id) {
            return Err(Error::CompactionTaskAlreadyAssigned(
                compact_task.task_id,
                assignment.context_id,
            ));
        }
        compact_task_assignment.insert(
            compact_task.task_id,
            CompactTaskAssignment {
                compact_task: Some(compact_task.clone()),
                context_id: assignee_context_id,
            },
        );
        commit_multi_var!(self, Some(assignee_context_id), compact_task_assignment)?;

        #[cfg(test)]
        {
            drop(compaction_guard);
            self.check_state_consistency().await;
        }

        Ok(())
    }

    pub async fn report_compact_task(
        &self,
        context_id: HummockContextId,
        compact_task: &CompactTask,
    ) -> Result<bool> {
        let ret = self
            .report_compact_task_impl(Some(context_id), compact_task)
            .await?;

        Ok(ret)
    }

    /// `report_compact_task` is retryable. `task_id` in `compact_task` parameter is used as the
    /// idempotency key. Return Ok(false) to indicate the `task_id` is not found, which may have
    /// been processed previously.
    #[named]
    pub async fn report_compact_task_impl(
        &self,
        context_id: Option<HummockContextId>,
        compact_task: &CompactTask,
    ) -> Result<bool> {
        let mut compaction_guard = write_lock!(self, compaction).await;
        let compaction = compaction_guard.deref_mut();
        let start_time = Instant::now();
        let mut compact_status = VarTransaction::new(
            compaction
                .compaction_statuses
                .get_mut(&compact_task.compaction_group_id)
                .ok_or(Error::InvalidCompactionGroup(
                    compact_task.compaction_group_id,
                ))?,
        );
        let mut compact_task_assignment =
            BTreeMapTransaction::new(&mut compaction.compact_task_assignment);
        let assignee_context_id = compact_task_assignment
            .remove(compact_task.task_id)
            .map(|assignment| assignment.context_id);

        // For cancel task, there is no need to check the task assignment because
        // we have not assign any compactor to it.
        if let Some(context_id) = context_id {
            match assignee_context_id {
                Some(id) => {
                    // Assignee id mismatch.
                    if id != context_id {
                        tracing::warn!(
                            "Wrong reporter {}. Compaction task {} is assigned to {}",
                            context_id,
                            compact_task.task_id,
                            *assignee_context_id.as_ref().unwrap(),
                        );
                        return Ok(false);
                    }
                }
                None => {
                    // The task is not found.
                    tracing::warn!("Compaction task {} not found", compact_task.task_id);
                    return Ok(false);
                }
            }
        }
        compact_status.report_compact_task(compact_task);
        if compact_task.task_status {
            // The compaction task is finished.
            let mut versioning_guard = write_lock!(self, versioning).await;
            let old_version = versioning_guard.current_version.clone();
            let versioning = versioning_guard.deref_mut();
            let mut hummock_version_deltas =
                BTreeMapTransaction::new(&mut versioning.hummock_version_deltas);
            apply_version_delta(
                &mut hummock_version_deltas,
                &old_version,
                compact_task,
                false,
            );
            let new_version_id = old_version.id + 1;
            let mut new_version = CompactStatus::apply_compact_result(compact_task, old_version);
            new_version.id = new_version_id;
            commit_multi_var!(
                self,
                assignee_context_id,
                compact_status,
                compact_task_assignment,
                hummock_version_deltas
            )?;
            versioning.current_version = new_version;

            self.env
                .notification_manager()
                .notify_compute_asynchronously(
                    Operation::Add,
                    Info::HummockVersionDeltas(risingwave_pb::hummock::HummockVersionDeltas {
                        version_deltas: vec![versioning
                            .hummock_version_deltas
                            .last_key_value()
                            .unwrap()
                            .1
                            .clone()],
                    }),
                );
        } else {
            // The compaction task is cancelled.
            commit_multi_var!(
                self,
                assignee_context_id,
                compact_status,
                compact_task_assignment
            )?;
        }

        tracing::trace!(
            "Reported compaction task. {}. cost time: {:?}",
            compact_task_to_string(compact_task),
            start_time.elapsed(),
        );

        trigger_sst_stat(
            &self.metrics,
            Some(
                compaction
                    .compaction_statuses
                    .get(&compact_task.compaction_group_id)
                    .ok_or(Error::InvalidCompactionGroup(
                        compact_task.compaction_group_id,
                    ))?,
            ),
            read_lock!(self, versioning).await.current_version.borrow(),
            compact_task.compaction_group_id,
        );

        self.try_send_compaction_request(compact_task.compaction_group_id);

        #[cfg(test)]
        {
            drop(compaction_guard);
            self.check_state_consistency().await;
        }

        Ok(true)
    }

    /// Caller should ensure `epoch` > `max_committed_epoch`
    #[named]
    pub async fn commit_epoch(
        &self,
        epoch: HummockEpoch,
        sstables: Vec<LocalSstableInfo>,
        sst_to_context: HashMap<HummockSstableId, HummockContextId>,
    ) -> Result<()> {
        // Warn of table_ids that is not found in expected compaction group.
        // It indicates:
        // 1. Either these table_ids are never registered to any compaction group. This is FATAL
        // since compaction filter will remove these valid states incorrectly.
        // 2. Or the owners of these table_ids have been dropped, but their stale states are still
        // committed. This is OK since compaction filter will remove these stale states
        // later.
        for (compaction_group_id, sst) in &sstables {
            let compaction_group = self
                .compaction_group_manager
                .compaction_group(*compaction_group_id)
                .await
                .unwrap_or_else(|| panic!("compaction group {} exists", compaction_group_id));
            for table_id in sst
                .table_ids
                .iter()
                .filter(|t| !compaction_group.member_table_ids().contains(t))
            {
                tracing::warn!(
                    "table {} doesn't belong to expected compaction group {}",
                    table_id,
                    compaction_group_id
                );
            }
        }

        let mut versioning_guard = write_lock!(self, versioning).await;
        let _timer = start_measure_real_process_timer!(self);

        for (sst_id, context_id) in &sst_to_context {
            #[cfg(test)]
            {
                if *context_id == META_NODE_ID {
                    continue;
                }
            }
            if !self.check_context(*context_id).await {
                return Err(Error::InvalidSst(*sst_id));
            }
        }

        let old_version = versioning_guard.current_version.clone();
        let new_version_id = old_version.id + 1;
        let versioning = versioning_guard.deref_mut();
        let mut hummock_version_deltas =
            BTreeMapTransaction::new(&mut versioning.hummock_version_deltas);
        let mut new_version_delta = hummock_version_deltas.new_entry_insert_txn(
            new_version_id,
            HummockVersionDelta {
                prev_id: old_version.id,
                safe_epoch: old_version.safe_epoch,
                trivial_move: false,
                ..Default::default()
            },
        );
        let mut new_hummock_version = old_version;
        new_hummock_version.id = new_version_id;
        new_version_delta.id = new_version_id;
        if epoch <= new_hummock_version.max_committed_epoch {
            return Err(anyhow::anyhow!(
                "Epoch {} <= max_committed_epoch {}",
                epoch,
                new_hummock_version.max_committed_epoch
            )
            .into());
        }

        let mut modified_compaction_groups = vec![];
        // Append SSTs to a new version.
        for (compaction_group_id, sstables) in &sstables.into_iter().group_by(|(cg_id, _)| *cg_id) {
            modified_compaction_groups.push(compaction_group_id);
            let group_sstables = sstables.into_iter().map(|(_, sst)| sst).collect_vec();
            let level_deltas = &mut new_version_delta
                .level_deltas
                .entry(compaction_group_id)
                .or_default()
                .level_deltas;
            let version_l0 = new_hummock_version
                .get_compaction_group_levels_mut(compaction_group_id)
                .l0
                .as_mut()
                .expect("Expect level 0 is not empty");
            let level_delta = LevelDelta {
                level_idx: 0,
                inserted_table_infos: group_sstables.clone(),
                l0_sub_level_id: epoch,
                ..Default::default()
            };

            // All files will be committed in one new Overlapping sub-level and become
            // Nonoverlapping  after at least one compaction.
            let level = Level {
                level_type: LevelType::Overlapping as i32,
                level_idx: 0,
                total_file_size: group_sstables
                    .iter()
                    .map(|table| table.file_size)
                    .sum::<u64>(),
                table_infos: group_sstables,
                sub_level_id: level_delta.l0_sub_level_id,
            };
            version_l0.total_file_size += level.total_file_size;
            version_l0.sub_levels.push(level);
            level_deltas.push(level_delta);
        }

        // Create a new_version, possibly merely to bump up the version id and max_committed_epoch.
        new_version_delta.max_committed_epoch = epoch;
        new_hummock_version.max_committed_epoch = epoch;
        commit_multi_var!(self, None, new_version_delta)?;
        versioning.current_version = new_hummock_version;
        self.max_committed_epoch.store(epoch, Ordering::Release);

        // Update metrics
        trigger_commit_stat(&self.metrics, &versioning.current_version);
        for compaction_group_id in &modified_compaction_groups {
            trigger_sst_stat(
                &self.metrics,
                None,
                &versioning.current_version,
                *compaction_group_id,
            );
        }

        tracing::trace!("new committed epoch {}", epoch);

        self.env
            .notification_manager()
            .notify_frontend_asynchronously(
                Operation::Update,
                Info::HummockSnapshot(HummockSnapshot { epoch }),
            );
        self.env
            .notification_manager()
            .notify_compute_asynchronously(
                Operation::Add,
                Info::HummockVersionDeltas(risingwave_pb::hummock::HummockVersionDeltas {
                    version_deltas: vec![versioning
                        .hummock_version_deltas
                        .last_key_value()
                        .unwrap()
                        .1
                        .clone()],
                }),
            );

        drop(versioning_guard);

        // commit_epoch may contains SSTs from any compaction group
        for id in modified_compaction_groups {
            self.try_send_compaction_request(id);
        }

        #[cfg(test)]
        {
            self.check_state_consistency().await;
        }

        Ok(())
    }

    pub async fn get_new_sst_ids(&self, number: u32) -> Result<SstIdRange> {
        // TODO: refactor id generator to u64
        assert!(number <= (i32::MAX as u32), "number overflow");
        let start_id = self
            .env
            .id_gen_manager()
            .generate_interval::<{ IdCategory::HummockSstableId }>(number as i32)
            .await
            .map(|id| id as u64)?;
        assert!(start_id <= u64::MAX - number as u64, "SST id overflow");
        Ok(SstIdRange::new(start_id, start_id + number as u64))
    }

    /// Tries to checkpoint at min_pinned_version_id
    ///
    /// Returns the diff between new and old checkpoint id.
    #[named]
    pub async fn proceed_version_checkpoint(&self) -> Result<u64> {
        let mut versioning_guard = write_lock!(self, versioning).await;
        let min_pinned_version_id = versioning_guard.min_pinned_version_id();
        if min_pinned_version_id <= versioning_guard.checkpoint_version.id {
            return Ok(0);
        }
        let versioning = versioning_guard.deref_mut();
        let mut checkpoint = VarTransaction::new(&mut versioning.checkpoint_version);
        let old_checkpoint_id = checkpoint.id;
        let mut new_checkpoint_id = min_pinned_version_id;
        for (_, version_delta) in versioning
            .hummock_version_deltas
            .range((Excluded(old_checkpoint_id), Included(new_checkpoint_id)))
        {
            checkpoint.apply_version_delta(version_delta);
        }
        new_checkpoint_id = checkpoint.id;
        if new_checkpoint_id == old_checkpoint_id {
            return Ok(0);
        }
        commit_multi_var!(self, None, checkpoint)?;
        versioning.extend_ssts_to_delete_from_deltas((
            Excluded(old_checkpoint_id),
            Included(new_checkpoint_id),
        ));
        #[cfg(test)]
        {
            drop(versioning_guard);
            self.check_state_consistency().await;
        }
        Ok(new_checkpoint_id - old_checkpoint_id)
    }

    #[named]
    pub async fn get_min_pinned_version_id(&self) -> HummockVersionId {
        read_lock!(self, versioning).await.min_pinned_version_id()
    }

    // TODO: use proc macro to call check_state_consistency
    #[named]
    #[cfg(test)]
    pub async fn check_state_consistency(&self) {
        use std::borrow::Borrow;
        let mut compaction_guard = write_lock!(self, compaction).await;
        let mut versioning_guard = write_lock!(self, versioning).await;
        let get_state =
            |compaction_guard: &RwLockWriteGuard<'_, Compaction>,
             versioning_guard: &RwLockWriteGuard<'_, Versioning>| {
                let compact_statuses_copy = compaction_guard.compaction_statuses.clone();
                let compact_task_assignment_copy = compaction_guard.compact_task_assignment.clone();
                let pinned_versions_copy = versioning_guard.pinned_versions.clone();
                let pinned_snapshots_copy = versioning_guard.pinned_snapshots.clone();
                let checkpoint_version_copy = versioning_guard.checkpoint_version.clone();
                let hummock_version_deltas_copy = versioning_guard.hummock_version_deltas.clone();
                (
                    compact_statuses_copy,
                    compact_task_assignment_copy,
                    pinned_versions_copy,
                    pinned_snapshots_copy,
                    checkpoint_version_copy,
                    hummock_version_deltas_copy,
                )
            };
        let mem_state = get_state(compaction_guard.borrow(), versioning_guard.borrow());
        self.load_meta_store_state_impl(
            compaction_guard.borrow_mut(),
            versioning_guard.borrow_mut(),
        )
        .await
        .expect("Failed to load state from meta store");
        let loaded_state = get_state(compaction_guard.borrow(), versioning_guard.borrow());
        assert_eq!(
            mem_state, loaded_state,
            "hummock in-mem state is inconsistent with meta store state",
        );
    }

    /// Gets current version without pinning it.
    #[named]
    pub async fn get_current_version(&self) -> HummockVersion {
        read_lock!(self, versioning).await.current_version.clone()
    }

    #[named]
    pub(crate) async fn get_read_guard(&self) -> RwLockReadGuard<Versioning> {
        read_lock!(self, versioning).await
    }

    pub fn set_compaction_scheduler(&self, sender: CompactionRequestChannelRef) {
        *self.compaction_scheduler.write() = Some(sender);
    }

    /// Cancels pending compaction tasks which are not yet assigned to any compactor.
    #[named]
    async fn cancel_unassigned_compaction_task(&self) -> Result<()> {
        let mut compaction_guard = write_lock!(self, compaction).await;
        let compaction = compaction_guard.deref_mut();
        let mut compact_statuses = BTreeMapTransaction::new(&mut compaction.compaction_statuses);
        let mut cancelled_count = 0;
        let mut modified_group_status = vec![];
        for (group_id, compact_status) in compact_statuses.tree_ref().iter() {
            let mut compact_status = compact_status.clone();
            let count = compact_status.cancel_compaction_tasks_if(|pending_task_id| {
                !compaction
                    .compact_task_assignment
                    .contains_key(&pending_task_id)
            });
            if count > 0 {
                cancelled_count += count;
                modified_group_status.push((*group_id, compact_status));
            }
        }
        for (group_id, compact_status) in modified_group_status {
            compact_statuses.insert(group_id, compact_status);
        }
        if cancelled_count > 0 {
            commit_multi_var!(self, None, compact_statuses)?;
        }
        #[cfg(test)]
        {
            drop(compaction_guard);
            self.check_state_consistency().await;
        }
        Ok(())
    }

    /// Sends a compaction request to compaction scheduler.
    pub fn try_send_compaction_request(&self, compaction_group: CompactionGroupId) -> bool {
        if let Some(sender) = self.compaction_scheduler.read().as_ref() {
            return sender.try_send(compaction_group);
        }
        false
    }

    pub async fn trigger_manual_compaction(
        &self,
        compaction_group: CompactionGroupId,
        manual_compaction_option: ManualCompactionOption,
    ) -> Result<()> {
        let start_time = Instant::now();

        // 1. select_compactor
        let compactor = self.compactor_manager.random_compactor();
        let compactor = match compactor {
            None => {
                tracing::warn!("trigger_manual_compaction No compactor is available.");
                return Err(anyhow::anyhow!(
                    "trigger_manual_compaction No compactor is available. compaction_group {}",
                    compaction_group
                )
                .into());
            }

            Some(compactor) => compactor,
        };

        // 2. manual_get_compact_task
        let compact_task = self
            .manual_get_compact_task(compaction_group, manual_compaction_option)
            .await;
        let mut compact_task = match compact_task {
            Ok(Some(compact_task)) => compact_task,
            Ok(None) => {
                // No compaction task available.
                return Err(anyhow::anyhow!(
                    "trigger_manual_compaction No compaction_task is available. compaction_group {}",
                    compaction_group
                ).into());
            }
            Err(err) => {
                tracing::warn!("Failed to get compaction task: {:#?}.", err);
                return Err(anyhow::anyhow!(
                    "Failed to get compaction task: {:#?} compaction_group {}",
                    err,
                    compaction_group
                )
                .into());
            }
        };

        let mut is_failed = false;
        let mut need_cancel_task = false;
        if let Err(e) = self
            .assign_compaction_task(&compact_task, compactor.context_id())
            .await
        {
            is_failed = true;
            tracing::warn!(
                "Failed to assign compaction task to compactor {}: {:#?}",
                compactor.context_id(),
                e
            );
        }

        if !is_failed {
            if let Err(e) = compactor
                .send_task(Task::CompactTask(compact_task.clone()))
                .await
            {
                is_failed = true;
                need_cancel_task = true;
                tracing::warn!(
                    "Failed to send task {} to {}. {:#?}",
                    compact_task.task_id,
                    compactor.context_id(),
                    e
                );
            }
        }

        if need_cancel_task {
            compact_task.task_status = false;
            if let Err(e) = self
                .report_compact_task(compactor.context_id(), &compact_task)
                .await
            {
                tracing::error!("Failed to cancel task {}. {:#?}", compact_task.task_id, e);
                // TODO #3677: handle cancellation via compaction heartbeat after #4496
            }
        }

        if is_failed {
            return Err(Error::InternalError(anyhow::anyhow!(
                "Failed to trigger_manual_compaction"
            )));
        }

        tracing::info!(
            "Trigger manual compaction task. {}. cost time: {:?}",
            compact_task_to_string(&compact_task),
            start_time.elapsed(),
        );

        Ok(())
    }

    pub fn compactor_manager_ref_for_test(&self) -> CompactorManagerRef {
        self.compactor_manager.clone()
    }

    #[named]
    pub async fn compaction_task_from_assignment_for_test(
        &self,
        task_id: u64,
    ) -> Option<CompactTaskAssignment> {
        let compaction_guard = read_lock!(self, compaction).await;
        let assignment_ref = &compaction_guard.compact_task_assignment;
        assignment_ref.get(&task_id).cloned()
    }

    pub fn compaction_group_manager_ref_for_test(&self) -> CompactionGroupManagerRef<S> {
        self.compaction_group_manager.clone()
    }

    pub fn cluster_manager(&self) -> &ClusterManagerRef<S> {
        &self.cluster_manager
    }
}

fn apply_version_delta<'a>(
    txn: &mut BTreeMapTransaction<'a, HummockVersionId, HummockVersionDelta>,
    old_version: &HummockVersion,
    compact_task: &CompactTask,
    trivial_move: bool,
) {
    let mut version_delta = HummockVersionDelta {
        prev_id: old_version.id,
        max_committed_epoch: old_version.max_committed_epoch,
        trivial_move,
        ..Default::default()
    };
    let level_deltas = &mut version_delta
        .level_deltas
        .entry(compact_task.compaction_group_id)
        .or_default()
        .level_deltas;
    for level in &compact_task.input_ssts {
        let level_delta = LevelDelta {
            level_idx: level.level_idx,
            removed_table_ids: level.table_infos.iter().map(|sst| sst.id).collect_vec(),
            ..Default::default()
        };
        level_deltas.push(level_delta);
    }
    let level_delta = LevelDelta {
        level_idx: compact_task.target_level,
        inserted_table_infos: compact_task.sorted_output_ssts.clone(),
        l0_sub_level_id: compact_task.target_sub_level_id,
        ..Default::default()
    };
    level_deltas.push(level_delta);
    version_delta.safe_epoch = std::cmp::max(old_version.safe_epoch, compact_task.watermark);
    version_delta.id = old_version.id + 1;
    txn.insert(version_delta.id, version_delta);
}
