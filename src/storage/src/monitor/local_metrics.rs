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

#[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::HummockStateStoreMetrics;
use crate::monitor::CompactorMetrics;

#[derive(Default, Debug)]
pub struct StoreLocalStatistic {
    pub cache_data_block_miss: u64,
    pub cache_data_block_total: u64,
    pub cache_meta_block_miss: u64,
    pub cache_meta_block_total: u64,

    // include multiple versions of one key.
    pub total_key_count: u64,
    pub skip_multi_version_key_count: u64,
    pub skip_delete_key_count: u64,
    pub processed_key_count: u64,
    pub bloom_filter_true_negative_count: u64,
    pub remote_io_time: Arc<AtomicU64>,
    pub bloom_filter_check_counts: u64,
    pub get_shared_buffer_hit_counts: u64,

    #[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
    reported: AtomicBool,
    #[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
    added: AtomicBool,
}

impl StoreLocalStatistic {
    pub fn add(&mut self, other: &StoreLocalStatistic) {
        self.cache_meta_block_miss += other.cache_meta_block_miss;
        self.cache_meta_block_total += other.cache_meta_block_total;

        self.cache_data_block_miss += other.cache_data_block_miss;
        self.cache_data_block_total += other.cache_data_block_total;

        self.skip_multi_version_key_count += other.skip_multi_version_key_count;
        self.skip_delete_key_count += other.skip_delete_key_count;
        self.processed_key_count += other.processed_key_count;
        self.bloom_filter_true_negative_count += other.bloom_filter_true_negative_count;
        self.remote_io_time.fetch_add(
            other.remote_io_time.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.bloom_filter_check_counts += other.bloom_filter_check_counts;
        self.total_key_count += other.total_key_count;
        self.get_shared_buffer_hit_counts += other.get_shared_buffer_hit_counts;

        #[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
        if other.added.fetch_or(true, Ordering::Relaxed) || other.reported.load(Ordering::Relaxed) {
            tracing::error!("double added\n{:#?}", other);
        }
    }

    pub fn apply_meta_fetch(&mut self, local_cache_meta_block_miss: u64) {
        self.cache_meta_block_total += 1;
        self.cache_meta_block_miss += local_cache_meta_block_miss;
    }

    pub fn report(&self, metrics: &HummockStateStoreMetrics, table_id_label: &str) {
        if self.cache_data_block_total > 0 {
            metrics
                .sst_store_block_request_counts
                .with_label_values(&[table_id_label, "data_total"])
                .inc_by(self.cache_data_block_total);
        }

        if self.cache_data_block_miss > 0 {
            metrics
                .sst_store_block_request_counts
                .with_label_values(&[table_id_label, "data_miss"])
                .inc_by(self.cache_data_block_miss);
        }

        if self.cache_meta_block_total > 0 {
            metrics
                .sst_store_block_request_counts
                .with_label_values(&[table_id_label, "meta_total"])
                .inc_by(self.cache_meta_block_total);
        }

        if self.cache_meta_block_miss > 0 {
            metrics
                .sst_store_block_request_counts
                .with_label_values(&[table_id_label, "meta_miss"])
                .inc_by(self.cache_meta_block_miss);
        }

        let t = self.remote_io_time.load(Ordering::Relaxed) as f64;
        if t > 0.0 {
            metrics
                .remote_read_time
                .with_label_values(&[table_id_label])
                .observe(t / 1000.0);
        }

        if self.processed_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&[table_id_label, "processed"])
                .inc_by(self.processed_key_count);
        }

        if self.skip_multi_version_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&[table_id_label, "skip_multi_version"])
                .inc_by(self.skip_multi_version_key_count);
        }

        if self.skip_delete_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&[table_id_label, "skip_delete"])
                .inc_by(self.skip_delete_key_count);
        }

        if self.total_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&[table_id_label, "total"])
                .inc_by(self.total_key_count);
        }

        if self.get_shared_buffer_hit_counts > 0 {
            metrics
                .get_shared_buffer_hit_counts
                .with_label_values(&[table_id_label])
                .inc_by(self.get_shared_buffer_hit_counts);
        }

        #[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
        if self.reported.fetch_or(true, Ordering::Relaxed) || self.added.load(Ordering::Relaxed) {
            tracing::error!("double reported\n{:#?}", self);
        }
    }

    pub fn report_compactor(&self, metrics: &CompactorMetrics) {
        let t = self.remote_io_time.load(Ordering::Relaxed) as f64;
        if t > 0.0 {
            metrics.remote_read_time.observe(t / 1000.0);
        }
        if self.processed_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&["processed"])
                .inc_by(self.processed_key_count);
        }

        if self.skip_multi_version_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&["skip_multi_version"])
                .inc_by(self.skip_multi_version_key_count);
        }

        if self.skip_delete_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&["skip_delete"])
                .inc_by(self.skip_delete_key_count);
        }

        if self.total_key_count > 0 {
            metrics
                .iter_scan_key_counts
                .with_label_values(&["total"])
                .inc_by(self.total_key_count);
        }

        #[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
        if self.reported.fetch_or(true, Ordering::Relaxed) || self.added.load(Ordering::Relaxed) {
            tracing::error!("double reported\n{:#?}", self);
        }
    }

    pub fn report_bloom_filter_metrics(
        &self,
        metrics: &HummockStateStoreMetrics,
        oper_type: &str,
        table_id_label: &str,
        is_non_existent_key: bool,
    ) {
        if self.bloom_filter_check_counts == 0 {
            return;
        }

        // checks SST bloom filters
        metrics
            .bloom_filter_check_counts
            .with_label_values(&[table_id_label, oper_type])
            .inc_by(self.bloom_filter_check_counts);

        metrics
            .read_req_check_bloom_filter_counts
            .with_label_values(&[table_id_label, oper_type])
            .inc();

        if self.bloom_filter_true_negative_count > 0 {
            // true negative
            metrics
                .bloom_filter_true_negative_counts
                .with_label_values(&[table_id_label, oper_type])
                .inc_by(self.bloom_filter_true_negative_count);
        }

        if self.bloom_filter_check_counts > self.bloom_filter_true_negative_count {
            if is_non_existent_key {
                // false positive
                // checks SST bloom filters (at least one bloom filter return true) but returns
                // nothing
                metrics
                    .read_req_positive_but_non_exist_counts
                    .with_label_values(&[table_id_label, oper_type])
                    .inc();
            }
            // positive
            // checks SST bloom filters and at least one bloom filter returns positive
            metrics
                .read_req_bloom_filter_positive_counts
                .with_label_values(&[table_id_label, oper_type])
                .inc();
        }
    }

    pub fn ignore(&self) {
        #[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
        self.reported.store(true, Ordering::Relaxed);
    }

    #[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
    fn need_report(&self) -> bool {
        self.cache_data_block_miss != 0
            || self.cache_data_block_total != 0
            || self.cache_meta_block_miss != 0
            || self.cache_meta_block_total != 0
            || self.skip_multi_version_key_count != 0
            || self.skip_delete_key_count != 0
            || self.processed_key_count != 0
            || self.bloom_filter_true_negative_count != 0
            || self.remote_io_time.load(Ordering::Relaxed) != 0
            || self.bloom_filter_check_counts != 0
    }
}

#[cfg(all(debug_assertions, not(any(madsim, test, feature = "test"))))]
impl Drop for StoreLocalStatistic {
    fn drop(&mut self) {
        if !self.reported.load(Ordering::Relaxed)
            && !self.added.load(Ordering::Relaxed)
            && self.need_report()
        {
            tracing::error!("local stats lost!\n{:#?}", self);
        }
    }
}
