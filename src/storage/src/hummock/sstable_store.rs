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
use std::clone::Clone;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use bytes::{Buf, BufMut, Bytes};
use fail::fail_point;
use itertools::Itertools;
use risingwave_common::cache::LruCacheEventListener;
use risingwave_hummock_sdk::{is_remote_sst_id, HummockSstableId};
use risingwave_object_store::object::{
    get_local_path, BlockLocation, ObjectMetadata, ObjectStoreRef, ObjectStreamingUploader,
};

use super::{
    Block, BlockCache, Sstable, SstableMeta, TieredCache, TieredCacheKey, TieredCacheValue,
};
use crate::hummock::{BlockHolder, CacheableEntry, HummockError, HummockResult, LruCache};
use crate::monitor::{MemoryCollector, StoreLocalStatistic};

const MAX_META_CACHE_SHARD_BITS: usize = 2;
const MAX_CACHE_SHARD_BITS: usize = 6; // It means that there will be 64 shards lru-cache to avoid lock conflict.
const MIN_BUFFER_SIZE_PER_SHARD: usize = 256 * 1024 * 1024; // 256MB

pub type TableHolder = CacheableEntry<HummockSstableId, Box<Sstable>>;

// BEGIN section for tiered cache

impl TieredCacheKey for (HummockSstableId, u64) {
    fn encoded_len() -> usize {
        16
    }

    fn encode(&self, mut buf: &mut [u8]) {
        buf.put_u64(self.0);
        buf.put_u64(self.1);
    }

    fn decode(mut buf: &[u8]) -> Self {
        let sst_id = buf.get_u64();
        let block_idx = buf.get_u64();
        (sst_id, block_idx)
    }
}

impl TieredCacheValue for Box<Block> {
    fn len(&self) -> usize {
        self.raw_data().len()
    }

    fn encoded_len(&self) -> usize {
        self.raw_data().len()
    }

    fn encode(&self, mut buf: &mut [u8]) {
        buf.put_slice(self.raw_data());
    }

    fn decode(buf: Vec<u8>) -> Self {
        Box::new(Block::decode_from_raw(buf))
    }
}

pub struct BlockCacheEventListener {
    tiered_cache: TieredCache<(HummockSstableId, u64), Box<Block>>,
}

impl LruCacheEventListener for BlockCacheEventListener {
    type K = (HummockSstableId, u64);
    type T = Box<Block>;

    fn on_release(&self, key: Self::K, value: Self::T) {
        // TODO(MrCroxx): handle error?
        self.tiered_cache.insert(key, value).unwrap();
    }
}

// END section for tiered cache

pub struct SstableStreamingUploader {
    id: HummockSstableId,
    /// Data are uploaded block by block, except for the size footer.
    object_uploader: ObjectStreamingUploader,
    /// Compressed blocks to refill block or meta cache.
    blocks: Vec<Bytes>,
    // TODO: do not fill cache in uploader, rewrite this logic in other place.
    policy: CachePolicy,
}

impl SstableStreamingUploader {
    /// Upload compressed block data.
    pub fn upload_block(&mut self, block: Bytes) -> HummockResult<()> {
        if let CachePolicy::Fill = self.policy {
            self.blocks.push(block.clone());
        }
        self.object_uploader
            .write_bytes(block)
            .map_err(HummockError::object_io_error)
    }

    pub fn upload_size_footer(&mut self, size_footer: u32) -> HummockResult<()> {
        self.object_uploader
            .write_bytes(Bytes::from(size_footer.to_le_bytes().to_vec()))
            .map_err(HummockError::object_io_error)
    }
}

// TODO: Define policy based on use cases (read / compaction / ...).
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum CachePolicy {
    /// Disable read cache and not fill the cache afterwards.
    Disable,
    /// Try reading the cache and fill the cache afterwards.
    Fill,
    /// Read the cache but not fill the cache afterwards.
    NotFill,
}

pub struct SstableStore {
    path: String,
    store: ObjectStoreRef,
    block_cache: BlockCache,
    meta_cache: Arc<LruCache<HummockSstableId, Box<Sstable>>>,
    tiered_cache: TieredCache<(HummockSstableId, u64), Box<Block>>,
}

impl SstableStore {
    pub fn new(
        store: ObjectStoreRef,
        path: String,
        block_cache_capacity: usize,
        meta_cache_capacity: usize,
        tiered_cache: TieredCache<(HummockSstableId, u64), Box<Block>>,
    ) -> Self {
        let mut shard_bits = MAX_META_CACHE_SHARD_BITS;
        while (meta_cache_capacity >> shard_bits) < MIN_BUFFER_SIZE_PER_SHARD && shard_bits > 0 {
            shard_bits -= 1;
        }
        let meta_cache = Arc::new(LruCache::new(shard_bits, meta_cache_capacity));
        let listener = Arc::new(BlockCacheEventListener {
            tiered_cache: tiered_cache.clone(),
        });

        Self {
            path,
            store,
            block_cache: BlockCache::with_event_listener(
                block_cache_capacity,
                MAX_CACHE_SHARD_BITS,
                listener,
            ),
            meta_cache,
            tiered_cache,
        }
    }

    /// For compactor, we do not need a high concurrency load for cache. Instead, we need the cache
    ///  can be evict more effective.
    pub fn for_compactor(
        store: ObjectStoreRef,
        path: String,
        block_cache_capacity: usize,
        meta_cache_capacity: usize,
    ) -> Self {
        let meta_cache = Arc::new(LruCache::new(0, meta_cache_capacity));
        let tiered_cache = TieredCache::none();
        Self {
            path,
            store,
            block_cache: BlockCache::new(block_cache_capacity, 0),
            meta_cache,
            tiered_cache,
        }
    }

    pub async fn put_sst_stream(
        &self,
        sst_id: HummockSstableId,
        policy: CachePolicy,
    ) -> HummockResult<SstableStreamingUploader> {
        let data_path = self.get_sst_data_path(sst_id);
        Ok(SstableStreamingUploader {
            id: sst_id,
            object_uploader: self.store.streaming_upload(&data_path).await?,
            blocks: Vec::new(),
            policy,
        })
    }

    /// Finish uploading by providing size footer and metadata.
    pub async fn finish_put_sst_stream(
        &self,
        uploader: SstableStreamingUploader,
        meta: SstableMeta,
    ) -> HummockResult<()> {
        uploader.object_uploader.finish().await?;
        let sst_id = uploader.id;
        if let Err(e) = self.put_meta(sst_id, &meta).await {
            self.delete_sst_data(sst_id).await?;
            return Err(e);
        }
        let sst = Sstable::new(sst_id, meta);
        let charge = sst.estimate_size();
        self.meta_cache
            .insert(sst_id, sst_id, charge, Box::new(sst));
        Ok(())
    }

    pub async fn delete(&self, sst_id: HummockSstableId) -> HummockResult<()> {
        // Meta
        self.store
            .delete(self.get_sst_meta_path(sst_id).as_str())
            .await?;
        // Data
        self.store
            .delete(self.get_sst_data_path(sst_id).as_str())
            .await?;
        self.meta_cache.erase(sst_id, &sst_id);
        Ok(())
    }

    pub fn delete_cache(&self, sst_id: HummockSstableId) {
        self.meta_cache.erase(sst_id, &sst_id);
    }

    async fn put_meta(&self, sst_id: HummockSstableId, meta: &SstableMeta) -> HummockResult<()> {
        let meta_path = self.get_sst_meta_path(sst_id);
        let meta = Bytes::from(meta.encode_to_bytes());
        self.store
            .upload(&meta_path, meta)
            .await
            .map_err(HummockError::object_io_error)
    }

    async fn put_sst_data(&self, sst_id: HummockSstableId, data: Bytes) -> HummockResult<()> {
        let data_path = self.get_sst_data_path(sst_id);
        self.store
            .upload(&data_path, data)
            .await
            .map_err(HummockError::object_io_error)
    }

    async fn delete_sst_data(&self, sst_id: HummockSstableId) -> HummockResult<()> {
        let data_path = self.get_sst_data_path(sst_id);
        self.store
            .delete(&data_path)
            .await
            .map_err(HummockError::object_io_error)
    }

    pub async fn get(
        &self,
        sst: &Sstable,
        block_index: u64,
        policy: CachePolicy,
        stats: &mut StoreLocalStatistic,
    ) -> HummockResult<BlockHolder> {
        stats.cache_data_block_total += 1;
        let tiered_cache = self.tiered_cache.clone();
        let fetch_block = || {
            stats.cache_data_block_miss += 1;
            let block_meta = sst
                .meta
                .block_metas
                .get(block_index as usize)
                .ok_or_else(HummockError::invalid_block)
                .unwrap(); // FIXME: don't unwrap here.
            let block_loc = BlockLocation {
                offset: block_meta.offset as usize,
                size: block_meta.len as usize,
            };
            let data_path = self.get_sst_data_path(sst.id);
            let store = self.store.clone();
            let sst_id = sst.id;
            let use_tiered_cache = !matches!(policy, CachePolicy::Disable);
            let uncompressed_capacity = block_meta.uncompressed_size as usize;

            async move {
                if use_tiered_cache && let Some(holder) = tiered_cache
                    .get(&(sst_id, block_index))
                    .await
                    .map_err(HummockError::tiered_cache)?
                {
                    // TODO(MrCroxx): `into_owned()` may perform buffer copy, eliminate it later.
                    return Ok(holder.into_owned());
                }

                let block_data = store.read(&data_path, Some(block_loc)).await?;
                let block = Block::decode(&block_data, uncompressed_capacity)?;
                Ok(Box::new(block))
            }
        };

        let disable_cache: fn() -> bool = || {
            fail_point!("disable_block_cache", |_| true);
            false
        };

        let policy = if disable_cache() {
            CachePolicy::Disable
        } else {
            policy
        };

        match policy {
            CachePolicy::Fill => {
                self.block_cache
                    .get_or_insert_with(sst.id, block_index, fetch_block)
                    .await
            }
            CachePolicy::NotFill => match self.block_cache.get(sst.id, block_index) {
                Some(block) => Ok(block),
                None => match self
                    .tiered_cache
                    .get(&(sst.id, block_index))
                    .await
                    .map_err(HummockError::tiered_cache)?
                {
                    Some(holder) => Ok(BlockHolder::from_tiered_cache(holder.into_inner())),
                    None => fetch_block().await.map(BlockHolder::from_owned_block),
                },
            },
            CachePolicy::Disable => fetch_block().await.map(BlockHolder::from_owned_block),
        }
    }

    pub fn get_sst_meta_path(&self, sst_id: HummockSstableId) -> String {
        let mut ret = format!("{}/{}.meta", self.path, sst_id);
        if !is_remote_sst_id(sst_id) {
            ret = get_local_path(&ret);
        }
        ret
    }

    pub fn get_sst_data_path(&self, sst_id: HummockSstableId) -> String {
        let mut ret = format!("{}/{}.data", self.path, sst_id);
        if !is_remote_sst_id(sst_id) {
            ret = get_local_path(&ret);
        }
        ret
    }

    pub fn get_sst_id_from_path(&self, path: &str) -> HummockSstableId {
        let split = path.split(&['/', '.']).collect_vec();
        debug_assert!(split.len() > 2);
        debug_assert!(split[split.len() - 1] == "meta" || split[split.len() - 1] == "data");
        split[split.len() - 2]
            .parse::<HummockSstableId>()
            .expect("valid sst id")
    }

    pub fn store(&self) -> ObjectStoreRef {
        self.store.clone()
    }

    pub fn get_meta_cache(&self) -> Arc<LruCache<HummockSstableId, Box<Sstable>>> {
        self.meta_cache.clone()
    }

    pub fn get_block_cache(&self) -> BlockCache {
        self.block_cache.clone()
    }

    #[cfg(any(test, feature = "test"))]
    pub fn clear_block_cache(&self) {
        self.block_cache.clear();
    }

    #[cfg(any(test, feature = "test"))]
    pub fn clear_meta_cache(&self) {
        self.meta_cache.clear();
    }

    pub async fn sstable(
        &self,
        sst_id: HummockSstableId,
        stats: &mut StoreLocalStatistic,
    ) -> HummockResult<TableHolder> {
        stats.cache_meta_block_total += 1;
        self.meta_cache
            .lookup_with_request_dedup::<_, HummockError, _>(sst_id, sst_id, || {
                let store = self.store.clone();
                let meta_path = self.get_sst_meta_path(sst_id);
                stats.cache_meta_block_miss += 1;
                let stats_ptr = stats.remote_io_time.clone();
                async move {
                    let now = Instant::now();
                    let buf = store
                        .read(&meta_path, None)
                        .await
                        .map_err(HummockError::object_io_error)?;
                    let meta = SstableMeta::decode(&mut &buf[..])?;
                    let sst = Sstable::new(sst_id, meta);
                    let charge = sst.meta.encoded_size();
                    let add = (now.elapsed().as_secs_f64() * 1000.0).ceil();
                    stats_ptr.fetch_add(add as u64, Ordering::Relaxed);
                    Ok((Box::new(sst), charge))
                }
            })
            .await
            .map_err(|e| {
                HummockError::other(format!(
                    "meta cache lookup request dedup get cancel: {:?}",
                    e,
                ))
            })?
    }

    pub async fn list_ssts_from_object_store(&self) -> HummockResult<Vec<ObjectMetadata>> {
        self.store
            .list(&self.path)
            .await
            .map_err(HummockError::object_io_error)
    }
}

pub type SstableStoreRef = Arc<SstableStore>;

impl MemoryCollector for SstableStore {
    fn get_meta_memory_usage(&self) -> u64 {
        self.meta_cache.get_memory_usage() as u64
    }

    fn get_data_memory_usage(&self) -> u64 {
        self.block_cache.size() as u64
    }

    // TODO: limit shared-buffer uploading memory
    fn get_total_memory_usage(&self) -> u64 {
        0
    }
}

#[async_trait::async_trait]
pub trait SstableStoreWrite: Send + Sync {
    async fn put_sst(
        &self,
        sst_id: HummockSstableId,
        meta: SstableMeta,
        data: Bytes,
        policy: CachePolicy,
    ) -> HummockResult<()>;
}

#[async_trait::async_trait]
impl SstableStoreWrite for SstableStore {
    async fn put_sst(
        &self,
        sst_id: HummockSstableId,
        meta: SstableMeta,
        data: Bytes,
        policy: CachePolicy,
    ) -> HummockResult<()> {
        self.put_sst_data(sst_id, data.clone()).await?;
        fail_point!("metadata_upload_err");
        if let Err(e) = self.put_meta(sst_id, &meta).await {
            self.delete_sst_data(sst_id).await?;
            return Err(e);
        }
        if let CachePolicy::Fill = policy {
            for (block_idx, block_meta) in meta.block_metas.iter().enumerate() {
                let end_offset = (block_meta.offset + block_meta.len) as usize;
                let block = Block::decode(
                    &data[block_meta.offset as usize..end_offset],
                    block_meta.uncompressed_size as usize,
                )?;
                self.block_cache
                    .insert(sst_id, block_idx as u64, Box::new(block));
            }
        }
        let sst = Sstable::new(sst_id, meta);
        let charge = sst.estimate_size();
        self.meta_cache
            .insert(sst_id, sst_id, charge, Box::new(sst));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::Arc;

    use byteorder::{LittleEndian, ReadBytesExt};
    use bytes::Bytes;
    use risingwave_hummock_sdk::HummockSstableId;

    use super::SstableStoreRef;
    use crate::hummock::iterator::test_utils::{iterator_test_key_of, mock_sstable_store};
    use crate::hummock::iterator::HummockIterator;
    use crate::hummock::sstable::SstableIteratorReadOptions;
    use crate::hummock::test_utils::{default_builder_opt_for_test, gen_test_sstable_data};
    use crate::hummock::value::HummockValue;
    use crate::hummock::{CachePolicy, SstableIterator, SstableMeta, SstableStoreWrite};
    use crate::monitor::StoreLocalStatistic;

    fn get_hummock_value(x: usize) -> HummockValue<Vec<u8>> {
        HummockValue::put(format!("overlapped_new_{}", x).as_bytes().to_vec())
    }

    async fn validate_sst(
        sstable_store: SstableStoreRef,
        id: HummockSstableId,
        meta: SstableMeta,
        x_range: Range<usize>,
    ) {
        let mut stats = StoreLocalStatistic::default();
        let holder = sstable_store.sstable(id, &mut stats).await.unwrap();
        assert_eq!(holder.value().meta, meta);
        let holder = sstable_store.sstable(id, &mut stats).await.unwrap();
        assert_eq!(holder.value().meta, meta);
        let mut iter = SstableIterator::new(
            holder,
            sstable_store,
            Arc::new(SstableIteratorReadOptions::default()),
        );
        iter.rewind().await.unwrap();
        for i in x_range {
            let key = iter.key();
            let value = iter.value();
            assert_eq!(key, iterator_test_key_of(i).as_slice());
            assert_eq!(value, get_hummock_value(i).as_slice());
            iter.next().await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_read_whole_data_object() {
        let sstable_store = mock_sstable_store();
        let x_range = 0..100;
        let (data, meta, _) = gen_test_sstable_data(
            default_builder_opt_for_test(),
            x_range
                .clone()
                .map(|x| (iterator_test_key_of(x), get_hummock_value(x))),
        );
        sstable_store
            .put_sst(1, meta.clone(), data, CachePolicy::NotFill)
            .await
            .unwrap();
        validate_sst(sstable_store, 1, meta, x_range).await;
    }

    #[tokio::test]
    async fn test_streaming_upload() {
        let sstable_store = mock_sstable_store();
        let x_range = 0..100;
        let (data, meta, _) = gen_test_sstable_data(
            default_builder_opt_for_test(),
            x_range
                .clone()
                .map(|x| (iterator_test_key_of(x), get_hummock_value(x))),
        );
        let mut blocks = vec![];
        for block_meta in &meta.block_metas {
            let end_offset = (block_meta.offset + block_meta.len) as usize;
            let block = Bytes::from(data[block_meta.offset as usize..end_offset].to_vec());
            blocks.push(block);
        }
        let size_footer = (&data[(data.len() - 4)..])
            .read_u32::<LittleEndian>()
            .unwrap();

        let mut uploader = sstable_store
            .put_sst_stream(1, CachePolicy::NotFill)
            .await
            .unwrap();
        for block in blocks {
            uploader.upload_block(block).unwrap();
        }
        uploader.upload_size_footer(size_footer).unwrap();
        sstable_store
            .finish_put_sst_stream(uploader, meta.clone())
            .await
            .unwrap();

        validate_sst(sstable_store, 1, meta, x_range).await;
    }

    #[test]
    fn test_basic() {
        let sstable_store = mock_sstable_store();
        let sst_id = 123;
        let meta_path = sstable_store.get_sst_meta_path(sst_id);
        let data_path = sstable_store.get_sst_data_path(sst_id);
        assert_eq!(meta_path, "test/123.meta");
        assert_eq!(data_path, "test/123.data");
        assert_eq!(sstable_store.get_sst_id_from_path(&meta_path), sst_id);
        assert_eq!(sstable_store.get_sst_id_from_path(&data_path), sst_id);
    }
}
