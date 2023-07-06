use std::sync::Arc;

use anyhow::Context;
use tracing::instrument;

use utils::id::TenantId;

use super::{
    mgr::{GetTenantError, TenantsMap},
    Tenant,
};

#[derive(Debug, thiserror::Error)]
pub enum DeleteTenantError {
    #[error("GetTenant {0}")]
    Get(#[from] GetTenantError),

    #[error("Tenant deletion is already in progress")]
    AlreadyInProgress,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Orchestrates timeline shut down of all timeline tasks, removes its in-memory structures,
/// and deletes its data from both disk and s3.
/// The sequence of steps:
/// 1. Set deleted_at in remote index part.
/// 2. Create local mark file.
/// 3. Delete local files except metadata (it is simpler this way, to be able to reuse timeline initialization code that expects metadata)
/// 4. Delete remote layers
/// 5. Delete index part
/// 6. Delete meta, timeline directory
/// 7. Delete mark file
/// It is resumable from any step in case a crash/restart occurs.
/// There are three entrypoints to the process:
/// 1. [`DeleteTimelineFlow::run`] this is the main one called by a management api handler.
/// 2. [`DeleteTimelineFlow::resume_deletion`] is called during restarts when local metadata is still present
/// and we possibly neeed to continue deletion of remote files.
/// 3. [`DeleteTimelineFlow::cleanup_remaining_fs_traces_after_timeline_deletion`] is used when we deleted remote
/// index but still have local metadata, timeline directory and delete mark.
/// Note the only other place that messes around timeline delete mark is the logic that scans directory with timelines during tenant load.
#[derive(Default)]
pub enum DeleteTenantFlow {
    #[default]
    NotStarted,
    InProgress,
    Finished,
}

impl DeleteTenantFlow {
    // These steps are run in the context of management api request handler.
    // Long running steps are continued to run in the background.
    // NB: If this fails half-way through, and is retried, the retry will go through
    // all the same steps again. Make sure the code here is idempotent, and don't
    // error out if some of the shutdown tasks have already been completed!
    #[instrument(skip(tenants), fields(tenant_id=%tenant_id))]
    pub(crate) async fn run(
        tenants: &tokio::sync::RwLock<TenantsMap>,
        tenant_id: TenantId,
    ) -> Result<(), DeleteTenantError> {
        let (timeline, mut guard) = Self::prepare(tenants, tenant_id).await?;

        // Create mark on s3
        // Create local mark
        // Tree sort timelines, schedule delete for them.
        // Wait till delete finishes (can use channel trick i e wait till its closed and give out receivers to timeline delete flows)
        // Call the same as detach calls
        // Cleanup local traces

        todo!()
    }

    async fn prepare(
        tenants: &tokio::sync::RwLock<TenantsMap>,
        tenant_id: TenantId,
    ) -> Result<(Arc<Tenant>, tokio::sync::OwnedMutexGuard<Self>), DeleteTenantError> {
        let m = tenants.read().await;

        let tenant = m
            .get(&tenant_id)
            .ok_or(GetTenantError::NotFound(tenant_id))?;

        // FIXME: unsure about active only. Our init jobs may not be cancellable properly,
        // so at least for now allow deletions only for active tenants. TODO recheck
        if !tenant.is_active() {
            return Err(GetTenantError::NotActive(tenant_id).into());
        }

        let guard = Arc::clone(&tenant.delete_progress)
            .try_lock_owned()
            .map_err(|_| DeleteTenantError::AlreadyInProgress)?;

        tenant
            .shutdown(false)
            .await
            .map_err(|e| anyhow::anyhow!("tenant shutdown failed: {e:?}"))?;

        // TODO transition tenant to stopping state.

        todo!()
    }
}
