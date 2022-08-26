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

#![feature(lint_reasons)]
#![feature(hash_drain_filter)]
#![feature(async_closure)]

mod version_cmp;

#[macro_use]
extern crate num_derive;

use risingwave_pb::hummock::SstableInfo;
pub use version_cmp::*;
pub mod compact;
pub mod compaction_group;
pub mod filter_key_extractor;
pub mod key;
pub mod key_range;
pub mod prost_key_range;

pub type HummockSstableId = u64;
pub type HummockRefCount = u64;
pub type HummockVersionId = u64;
pub type HummockContextId = u32;
pub type HummockEpoch = u64;
pub type HummockCompactionTaskId = u64;
pub type CompactionGroupId = u64;
pub const INVALID_VERSION_ID: HummockVersionId = 0;
pub const FIRST_VERSION_ID: HummockVersionId = 1;

pub const LOCAL_SST_ID_MASK: HummockSstableId = 1 << (HummockSstableId::BITS - 1);
pub const REMOTE_SST_ID_MASK: HummockSstableId = !LOCAL_SST_ID_MASK;

pub type LocalSstableInfo = (CompactionGroupId, SstableInfo);

pub fn get_remote_sst_id(id: HummockSstableId) -> HummockSstableId {
    id & REMOTE_SST_ID_MASK
}

pub fn get_local_sst_id(id: HummockSstableId) -> HummockSstableId {
    id | LOCAL_SST_ID_MASK
}

pub fn is_remote_sst_id(id: HummockSstableId) -> bool {
    id & LOCAL_SST_ID_MASK == 0
}

pub struct SstIdRange {
    // inclusive
    pub start_id: HummockSstableId,
    // exclusive
    pub end_id: HummockSstableId,
}

impl SstIdRange {
    pub fn new(start_id: HummockSstableId, end_id: HummockSstableId) -> Self {
        Self { start_id, end_id }
    }

    pub fn peek_next_sst_id(&self) -> Option<HummockSstableId> {
        if self.start_id < self.end_id {
            return Some(self.start_id);
        }
        None
    }

    /// Pops and returns next SST id.
    pub fn get_next_sst_id(&mut self) -> Option<HummockSstableId> {
        let next_id = self.peek_next_sst_id();
        self.start_id += 1;
        next_id
    }
}
