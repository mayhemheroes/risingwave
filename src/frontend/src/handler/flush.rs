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

use pgwire::pg_response::{PgResponse, StatementType};
use risingwave_common::error::Result;

use crate::session::OptimizerContext;

pub(super) async fn handle_flush(context: OptimizerContext) -> Result<PgResponse> {
    let client = context.session_ctx.env().meta_client();
    // The returned epoch >= epoch for flush, but it is okay.
    let max_committed_epoch = client.flush().await?;
    // Update max committed epoch to ensure read-after-write correctness.
    context
        .session_ctx
        .env()
        .hummock_snapshot_manager()
        .update_epoch(max_committed_epoch);
    Ok(PgResponse::empty_result(StatementType::FLUSH))
}
