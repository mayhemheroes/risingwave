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

mod compaction_executor;
mod compaction_filter;
mod compactor_runner;
mod context;
mod iterator;
mod shared_buffer_compact;
mod sstable_store;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
pub use compaction_executor::CompactionExecutor;
pub use compaction_filter::{
    CompactionFilter, DummyCompactionFilter, MultiCompactionFilter, StateCleanUpCompactionFilter,
    TTLCompactionFilter,
};
pub use context::{CompactorContext, Context};
use futures::future::try_join_all;
use futures::{stream, FutureExt, StreamExt};
pub use iterator::ConcatSstableIterator;
use itertools::Itertools;
use risingwave_common::config::constant::hummock::CompactionFilterFlag;
use risingwave_common::util::sync_point::on_sync_point;
use risingwave_hummock_sdk::compact::compact_task_to_string;
use risingwave_hummock_sdk::filter_key_extractor::FilterKeyExtractorImpl;
use risingwave_hummock_sdk::key::{get_epoch, FullKey};
use risingwave_hummock_sdk::key_range::KeyRange;
use risingwave_hummock_sdk::{HummockEpoch, VersionedComparator};
use risingwave_pb::hummock::subscribe_compact_tasks_response::Task;
use risingwave_pb::hummock::{CompactTask, LevelType, SstableInfo, SubscribeCompactTasksResponse};
use risingwave_rpc_client::HummockMetaClient;
pub use shared_buffer_compact::compact;
pub use sstable_store::{
    CompactorMemoryCollector, CompactorSstableStore, CompactorSstableStoreRef,
};
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;

use super::multi_builder::CapacitySplitTableBuilder;
use super::{HummockResult, SstableBuilderOptions};
use crate::hummock::compactor::compactor_runner::CompactorRunner;
use crate::hummock::iterator::{Forward, HummockIterator};
use crate::hummock::multi_builder::{SealedSstableBuilder, TableBuilderFactory};
use crate::hummock::utils::{MemoryLimiter, MemoryTracker};
use crate::hummock::vacuum::Vacuum;
use crate::hummock::{
    CachePolicy, HummockError, SstableBuilder, SstableIdManagerRef, SstableStoreWrite,
    DEFAULT_ENTRY_SIZE,
};

pub struct RemoteBuilderFactory {
    sstable_id_manager: SstableIdManagerRef,
    limiter: Arc<MemoryLimiter>,
    options: SstableBuilderOptions,
    remote_rpc_cost: Arc<AtomicU64>,
    filter_key_extractor: Arc<FilterKeyExtractorImpl>,
}

#[async_trait::async_trait]
impl TableBuilderFactory for RemoteBuilderFactory {
    async fn open_builder(&self) -> HummockResult<(MemoryTracker, SstableBuilder)> {
        let tracker = self
            .limiter
            .require_memory(
                (self.options.capacity
                    + self.options.block_capacity
                    + self.options.estimate_bloom_filter_capacity) as u64,
            )
            .await
            .unwrap();
        let timer = Instant::now();
        let table_id = self.sstable_id_manager.get_new_sst_id().await?;
        let cost = (timer.elapsed().as_secs_f64() * 1000000.0).round() as u64;
        self.remote_rpc_cost.fetch_add(cost, Ordering::Relaxed);
        let builder = SstableBuilder::new(
            table_id,
            self.options.clone(),
            self.filter_key_extractor.clone(),
        );
        Ok((tracker, builder))
    }
}

#[derive(Clone)]
/// Implementation of Hummock compaction.
pub struct Compactor {
    /// The context of the compactor.
    context: Arc<Context>,

    options: SstableBuilderOptions,

    sstable_store: Arc<dyn SstableStoreWrite>,
    key_range: KeyRange,
    cache_policy: CachePolicy,
    gc_delete_keys: bool,
    watermark: u64,
}

pub type CompactOutput = (usize, Vec<SstableInfo>);

impl Compactor {
    /// Handles a compaction task and reports its status to hummock manager.
    /// Always return `Ok` and let hummock manager handle errors.
    pub async fn compact(
        compactor_context: Arc<CompactorContext>,
        mut compact_task: CompactTask,
    ) -> bool {
        let context = compactor_context.context.clone();
        // Set a watermark SST id to prevent full GC from accidentally deleting SSTs for in-progress
        // write op. The watermark is invalidated when this method exits.
        let tracker_id = match context.sstable_id_manager.add_watermark_sst_id(None).await {
            Ok(tracker_id) => tracker_id,
            Err(err) => {
                tracing::warn!("Failed to track pending SST id. {:#?}", err);
                return false;
            }
        };
        let sstable_id_manager_clone = context.sstable_id_manager.clone();
        let _guard = scopeguard::guard(
            (tracker_id, sstable_id_manager_clone),
            |(tracker_id, sstable_id_manager)| {
                sstable_id_manager.remove_watermark_sst_id(tracker_id);
            },
        );

        let group_label = compact_task.compaction_group_id.to_string();
        let cur_level_label = compact_task.input_ssts[0].level_idx.to_string();
        let select_table_infos = compact_task
            .input_ssts
            .iter()
            .filter(|level| level.level_idx != compact_task.target_level)
            .flat_map(|level| level.table_infos.iter())
            .collect_vec();
        let target_table_infos = compact_task
            .input_ssts
            .iter()
            .filter(|level| level.level_idx == compact_task.target_level)
            .flat_map(|level| level.table_infos.iter())
            .collect_vec();
        context
            .stats
            .compact_read_current_level
            .with_label_values(&[group_label.as_str(), cur_level_label.as_str()])
            .inc_by(
                select_table_infos
                    .iter()
                    .map(|table| table.file_size)
                    .sum::<u64>(),
            );
        context
            .stats
            .compact_read_sstn_current_level
            .with_label_values(&[group_label.as_str(), cur_level_label.as_str()])
            .inc_by(select_table_infos.len() as u64);

        let sec_level_read_bytes = target_table_infos.iter().map(|t| t.file_size).sum::<u64>();
        let next_level_label = compact_task.target_level.to_string();
        context
            .stats
            .compact_read_next_level
            .with_label_values(&[group_label.as_str(), next_level_label.as_str()])
            .inc_by(sec_level_read_bytes);
        context
            .stats
            .compact_read_sstn_next_level
            .with_label_values(&[group_label.as_str(), next_level_label.as_str()])
            .inc_by(target_table_infos.len() as u64);

        let timer = context
            .stats
            .compact_task_duration
            .with_label_values(&[compact_task.input_ssts[0].level_idx.to_string().as_str()])
            .start_timer();

        let need_quota = estimate_memory_use_for_compaction(&compact_task);
        tracing::info!(
            "Ready to handle compaction task: {} need memory: {}",
            compact_task.task_id,
            need_quota
        );

        let multi_filter = build_multi_compaction_filter(&compact_task);

        let multi_filter_key_extractor = context
            .filter_key_extractor_manager
            .acquire(HashSet::from_iter(compact_task.existing_table_ids.clone()))
            .await;
        let multi_filter_key_extractor = Arc::new(multi_filter_key_extractor);

        // Number of splits (key ranges) is equal to number of compaction tasks
        let parallelism = compact_task.splits.len();
        assert_ne!(parallelism, 0, "splits cannot be empty");
        context.stats.compact_task_pending_num.inc();
        let mut compact_success = true;
        let mut output_ssts = Vec::with_capacity(parallelism);
        let mut compaction_futures = vec![];

        for (split_index, _) in compact_task.splits.iter().enumerate() {
            let filter = multi_filter.clone();
            let multi_filter_key_extractor = multi_filter_key_extractor.clone();
            let compactor_runner = CompactorRunner::new(
                split_index,
                compactor_context.as_ref(),
                compact_task.clone(),
            );
            let handle = tokio::spawn(async move {
                compactor_runner
                    .run(filter, multi_filter_key_extractor)
                    .await
            });
            compaction_futures.push(handle);
        }

        let mut buffered = stream::iter(compaction_futures).buffer_unordered(parallelism);
        while let Some(future_result) = buffered.next().await {
            match future_result {
                Ok(Ok((split_index, ssts))) => {
                    output_ssts.push((split_index, ssts));
                }
                Ok(Err(e)) => {
                    compact_success = false;
                    tracing::warn!(
                        "Compaction task {} failed with error: {:#?}",
                        compact_task.task_id,
                        e
                    );
                }
                Err(e) => {
                    compact_success = false;
                    tracing::warn!(
                        "Compaction task {} failed with join handle error: {:#?}",
                        compact_task.task_id,
                        e
                    );
                }
            }
        }

        // Sort by split/key range index.
        output_ssts.sort_by_key(|(split_index, _)| *split_index);

        on_sync_point("BEFORE_COMPACT_REPORT").await.unwrap();
        // After a compaction is done, mutate the compaction task.
        Self::compact_done(
            &mut compact_task,
            context.clone(),
            output_ssts,
            compact_success,
        )
        .await;
        on_sync_point("AFTER_COMPACT_REPORT").await.unwrap();
        let cost_time = timer.stop_and_record() * 1000.0;
        tracing::info!(
            "Finished compaction task in {:?}ms: \n{}",
            cost_time,
            compact_task_to_string(&compact_task)
        );
        context.stats.compact_task_pending_num.dec();
        for level in &compact_task.input_ssts {
            for table in &level.table_infos {
                context.sstable_store.delete_cache(table.id);
            }
        }
        compact_success
    }

    /// Fill in the compact task and let hummock manager know the compaction output ssts.
    async fn compact_done(
        compact_task: &mut CompactTask,
        context: Arc<Context>,
        output_ssts: Vec<CompactOutput>,
        task_ok: bool,
    ) {
        compact_task.task_status = task_ok;
        compact_task
            .sorted_output_ssts
            .reserve(compact_task.splits.len());
        let mut compaction_write_bytes = 0;
        for (_, ssts) in output_ssts {
            for sst_info in ssts {
                compaction_write_bytes += sst_info.file_size;
                compact_task.sorted_output_ssts.push(sst_info);
            }
        }

        let group_label = compact_task.compaction_group_id.to_string();
        let level_label = compact_task.target_level.to_string();
        context
            .stats
            .compact_write_bytes
            .with_label_values(&[group_label.as_str(), level_label.as_str()])
            .inc_by(compaction_write_bytes);
        context
            .stats
            .compact_write_sstn
            .with_label_values(&[group_label.as_str(), level_label.as_str()])
            .inc_by(compact_task.sorted_output_ssts.len() as u64);
        let ret_label = if task_ok { "success" } else { "failed" };
        context
            .stats
            .compact_frequency
            .with_label_values(&[group_label.as_str(), ret_label])
            .inc();

        if let Err(e) = context
            .hummock_meta_client
            .report_compaction_task(compact_task.clone())
            .await
        {
            tracing::warn!(
                "Failed to report compaction task: {}, error: {}",
                compact_task.task_id,
                e
            );
        }
    }

    /// The background compaction thread that receives compaction tasks from hummock compaction
    /// manager and runs compaction tasks.
    pub fn start_compactor(
        compactor_context: Arc<CompactorContext>,
        hummock_meta_client: Arc<dyn HummockMetaClient>,
        max_concurrent_task_number: u64,
    ) -> (JoinHandle<()>, Sender<()>) {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let stream_retry_interval = Duration::from_secs(60);
        let join_handle = tokio::spawn(async move {
            let mut min_interval = tokio::time::interval(stream_retry_interval);
            // This outer loop is to recreate stream.
            'start_stream: loop {
                tokio::select! {
                    // Wait for interval.
                    _ = min_interval.tick() => {},
                    // Shutdown compactor.
                    _ = &mut shutdown_rx => {
                        tracing::info!("Compactor is shutting down");
                        return;
                    }
                }

                let mut stream = match hummock_meta_client
                    .subscribe_compact_tasks(max_concurrent_task_number)
                    .await
                {
                    Ok(stream) => {
                        tracing::debug!("Succeeded subscribe_compact_tasks.");
                        stream
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Subscribing to compaction tasks failed with error: {}. Will retry.",
                            e
                        );
                        continue 'start_stream;
                    }
                };
                let executor = compactor_context.context.compaction_executor.clone();

                // This inner loop is to consume stream.
                'consume_stream: loop {
                    let message = tokio::select! {
                        message = stream.message() => {
                            message
                        },
                        // Shutdown compactor
                        _ = &mut shutdown_rx => {
                            tracing::info!("Compactor is shutting down");
                            return
                        }
                    };
                    match message {
                        // The inner Some is the side effect of generated code.
                        Ok(Some(SubscribeCompactTasksResponse { task })) => {
                            let task = match task {
                                Some(task) => task,
                                None => continue 'consume_stream,
                            };

                            let context = compactor_context.clone();
                            let meta_client = hummock_meta_client.clone();
                            executor.execute(async move {
                                match task {
                                    Task::CompactTask(compact_task) => {
                                        Compactor::compact(context, compact_task).await;
                                    }
                                    Task::VacuumTask(vacuum_task) => {
                                        Vacuum::vacuum(
                                            vacuum_task,
                                            context.context.sstable_store.clone(),
                                            meta_client,
                                        )
                                        .await;
                                    }
                                    Task::FullScanTask(full_scan_task) => {
                                        Vacuum::full_scan(
                                            full_scan_task,
                                            context.context.sstable_store.clone(),
                                            meta_client,
                                        )
                                        .await;
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!("Failed to consume stream. {}", e.message());
                            continue 'start_stream;
                        }
                        _ => {
                            // The stream is exhausted
                            continue 'start_stream;
                        }
                    }
                }
            }
        });

        (join_handle, shutdown_tx)
    }

    pub async fn compact_and_build_sst<T: TableBuilderFactory>(
        sst_builder: &mut CapacitySplitTableBuilder<T>,
        kr: &KeyRange,
        mut iter: impl HummockIterator<Direction = Forward>,
        gc_delete_keys: bool,
        watermark: HummockEpoch,
        mut compaction_filter: impl CompactionFilter,
    ) -> HummockResult<()> {
        if !kr.left.is_empty() {
            iter.seek(&kr.left).await?;
        } else {
            iter.rewind().await?;
        }

        let mut last_key = BytesMut::new();
        let mut watermark_can_see_last_key = false;

        while iter.is_valid() {
            let iter_key = iter.key();

            let is_new_user_key =
                last_key.is_empty() || !VersionedComparator::same_user_key(iter_key, &last_key);

            let mut drop = false;
            let epoch = get_epoch(iter_key);
            if is_new_user_key {
                if !kr.right.is_empty()
                    && VersionedComparator::compare_key(iter_key, &kr.right)
                        != std::cmp::Ordering::Less
                {
                    break;
                }

                last_key.clear();
                last_key.extend_from_slice(iter_key);
                watermark_can_see_last_key = false;
            }

            // Among keys with same user key, only retain keys which satisfy `epoch` >= `watermark`.
            // If there is no keys whose epoch is equal than `watermark`, keep the latest key which
            // satisfies `epoch` < `watermark`
            // in our design, frontend avoid to access keys which had be deleted, so we dont
            // need to consider the epoch when the compaction_filter match (it
            // means that mv had drop)
            if (epoch <= watermark && gc_delete_keys && iter.value().is_delete())
                || (epoch < watermark && watermark_can_see_last_key)
            {
                drop = true;
            }

            if !drop && compaction_filter.should_delete(iter_key) {
                drop = true;
            }

            if epoch <= watermark {
                watermark_can_see_last_key = true;
            }

            if drop {
                iter.next().await?;
                continue;
            }

            // Don't allow two SSTs to share same user key
            sst_builder
                .add_full_key(FullKey::from_slice(iter_key), iter.value(), is_new_user_key)
                .await?;

            iter.next().await?;
        }
        Ok(())
    }
}

impl Compactor {
    /// Create a new compactor.
    pub fn new(
        context: Arc<Context>,
        options: SstableBuilderOptions,
        sstable_store: Arc<dyn SstableStoreWrite>,
        key_range: KeyRange,
        cache_policy: CachePolicy,
        gc_delete_keys: bool,
        watermark: u64,
    ) -> Self {
        Self {
            context,
            options,
            sstable_store,
            key_range,
            cache_policy,
            gc_delete_keys,
            watermark,
        }
    }

    /// Compact the given key range and merge iterator.
    /// Upon a successful return, the built SSTs are already uploaded to object store.
    async fn compact_key_range_impl(
        &self,
        iter: impl HummockIterator<Direction = Forward>,
        compaction_filter: impl CompactionFilter,
        filter_key_extractor: Arc<FilterKeyExtractorImpl>,
    ) -> HummockResult<Vec<SstableInfo>> {
        let get_id_time = Arc::new(AtomicU64::new(0));
        let mut options = self.options.clone();
        options.estimate_bloom_filter_capacity = self
            .context
            .filter_key_extractor_manager
            .estimate_bloom_filter_size(options.capacity);
        if options.estimate_bloom_filter_capacity == 0 {
            options.estimate_bloom_filter_capacity = options.capacity / DEFAULT_ENTRY_SIZE;
        }
        let builder_factory = RemoteBuilderFactory {
            sstable_id_manager: self.context.sstable_id_manager.clone(),
            limiter: self.context.read_memory_limiter.clone(),
            options,
            remote_rpc_cost: get_id_time.clone(),
            filter_key_extractor,
        };

        // NOTICE: should be user_key overlap, NOT full_key overlap!
        let mut builder = CapacitySplitTableBuilder::new(
            builder_factory,
            self.cache_policy,
            self.sstable_store.clone(),
            self.context.stats.clone(),
        );

        // Monitor time cost building shared buffer to SSTs.
        let compact_timer = if self.context.is_share_buffer_compact {
            self.context.stats.write_build_l0_sst_duration.start_timer()
        } else {
            self.context.stats.compact_sst_duration.start_timer()
        };

        Compactor::compact_and_build_sst(
            &mut builder,
            &self.key_range,
            iter,
            self.gc_delete_keys,
            self.watermark,
            compaction_filter,
        )
        .await?;
        let builder_len = builder.len();
        let sealed_builders = builder.finish();
        compact_timer.observe_duration();

        let mut ssts = Vec::with_capacity(builder_len);
        let mut upload_join_handles = vec![];
        for SealedSstableBuilder {
            sst_info,
            upload_join_handle,
            bloom_filter_size,
        } in sealed_builders
        {
            // bloomfilter occuppy per thousand keys
            self.context
                .filter_key_extractor_manager
                .update_bloom_filter_avg_size(sst_info.file_size as usize, bloom_filter_size);
            let sst_size = sst_info.file_size;
            ssts.push(sst_info);
            upload_join_handles.push(upload_join_handle);

            if self.context.is_share_buffer_compact {
                self.context
                    .stats
                    .shared_buffer_to_sstable_size
                    .observe(sst_size as _);
            } else {
                self.context.stats.compaction_upload_sst_counts.inc();
            }
        }

        // Wait for all upload to finish
        try_join_all(upload_join_handles.into_iter().map(|join_handle| {
            join_handle.map(|result| match result {
                Ok(upload_result) => upload_result,
                Err(e) => Err(HummockError::other(format!(
                    "fail to receive from upload join handle: {:?}",
                    e
                ))),
            })
        }))
        .await?;

        self.context
            .stats
            .get_table_id_total_time_duration
            .observe(get_id_time.load(Ordering::Relaxed) as f64 / 1000.0 / 1000.0);
        Ok(ssts)
    }
}

pub fn estimate_memory_use_for_compaction(task: &CompactTask) -> u64 {
    let mut total_memory_size = 0;
    for level in &task.input_ssts {
        if level.level_type == LevelType::Nonoverlapping as i32 {
            if let Some(table) = level.table_infos.first() {
                total_memory_size += table.file_size * task.splits.len() as u64;
            }
        } else {
            for table in &level.table_infos {
                total_memory_size += table.file_size;
            }
        }
    }
    total_memory_size
}

fn build_multi_compaction_filter(compact_task: &CompactTask) -> MultiCompactionFilter {
    use risingwave_common::catalog::TableOption;
    let mut multi_filter = MultiCompactionFilter::default();
    let compaction_filter_flag =
        CompactionFilterFlag::from_bits(compact_task.compaction_filter_mask).unwrap_or_default();
    if compaction_filter_flag.contains(CompactionFilterFlag::STATE_CLEAN) {
        let state_clean_up_filter = Box::new(StateCleanUpCompactionFilter::new(
            HashSet::from_iter(compact_task.existing_table_ids.clone()),
        ));

        multi_filter.register(state_clean_up_filter);
    }

    if compaction_filter_flag.contains(CompactionFilterFlag::TTL) {
        let id_to_ttl = compact_task
            .table_options
            .iter()
            .filter(|id_to_option| {
                let table_option: TableOption = id_to_option.1.into();
                table_option.retention_seconds.is_some()
            })
            .map(|id_to_option| (*id_to_option.0, id_to_option.1.retention_seconds))
            .collect();

        let ttl_filter = Box::new(TTLCompactionFilter::new(
            id_to_ttl,
            compact_task.current_epoch_time,
        ));
        multi_filter.register(ttl_filter);
    }

    multi_filter
}
