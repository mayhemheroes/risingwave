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

#[cfg(sync_point_test)]
mod utils;
#[cfg(sync_point_test)]
pub use utils::*;

#[cfg(not(sync_point_test))]
#[inline(always)]
#[expect(clippy::unused_async)]
pub async fn on_sync_point(_sync_point: &str) -> Result<(), Error> {
    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Wait for signal {0} timeout")]
    WaitForSignalTimeout(String),
}
