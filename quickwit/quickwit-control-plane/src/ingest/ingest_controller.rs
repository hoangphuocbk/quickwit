// Copyright (C) 2024 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::iter::zip;
use std::sync::Arc;
use std::time::Duration;
use std::{cmp, fmt};

use fnv::FnvHashSet;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use itertools::Itertools;
use quickwit_actors::Mailbox;
use quickwit_common::pretty::PrettySample;
use quickwit_common::Progress;
use quickwit_ingest::{IngesterPool, LeaderId, LocalShardsUpdate};
use quickwit_proto::control_plane::{
    AdviseResetShardsRequest, AdviseResetShardsResponse, ControlPlaneResult,
    GetOrCreateOpenShardsFailure, GetOrCreateOpenShardsFailureReason, GetOrCreateOpenShardsRequest,
    GetOrCreateOpenShardsResponse, GetOrCreateOpenShardsSuccess,
};
use quickwit_proto::ingest::ingester::{
    CloseShardsRequest, CloseShardsResponse, IngesterService, InitShardFailure,
    InitShardSubrequest, InitShardsRequest, InitShardsResponse, RetainShardsForSource,
    RetainShardsRequest,
};
use quickwit_proto::ingest::{Shard, ShardIdPosition, ShardIdPositions, ShardIds, ShardPKey};
use quickwit_proto::metastore;
use quickwit_proto::metastore::{MetastoreService, MetastoreServiceClient};
use quickwit_proto::types::{IndexUid, NodeId, Position, ShardId, SourceUid};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::task::JoinHandle;
use tracing::{debug, enabled, error, info, warn, Level};
use ulid::Ulid;

use crate::control_plane::ControlPlane;
use crate::ingest::wait_handle::WaitHandle;
use crate::model::{ControlPlaneModel, ScalingMode, ShardEntry, ShardStats};

const MAX_SHARD_INGESTION_THROUGHPUT_MIB_PER_SEC: f32 = 5.;

/// Threshold in MiB/s above which we increase the number of shards.
const SCALE_UP_SHARDS_THRESHOLD_MIB_PER_SEC: f32 =
    MAX_SHARD_INGESTION_THROUGHPUT_MIB_PER_SEC * 8. / 10.;

/// Threshold in MiB/s below which we decrease the number of shards.
const SCALE_DOWN_SHARDS_THRESHOLD_MIB_PER_SEC: f32 =
    MAX_SHARD_INGESTION_THROUGHPUT_MIB_PER_SEC * 2. / 10.;

const CLOSE_SHARDS_REQUEST_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(3)
};

const INIT_SHARDS_REQUEST_TIMEOUT: Duration = CLOSE_SHARDS_REQUEST_TIMEOUT;

const CLOSE_SHARDS_UPON_REBALANCE_DELAY: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(10)
};

const FIRE_AND_FORGET_TIMEOUT: Duration = Duration::from_secs(3);

/// Spawns a new task to execute the given future,
/// and stops polling it/drops it after a timeout.
///
/// All errors are ignored, and not even logged.
fn fire_and_forget(
    fut: impl Future<Output = ()> + Send + 'static,
    operation: impl std::fmt::Display + Send + Sync + 'static,
) {
    tokio::spawn(async move {
        if let Err(_timeout_elapsed) = tokio::time::timeout(FIRE_AND_FORGET_TIMEOUT, fut).await {
            error!(operation=%operation, "timeout elapsed");
        }
    });
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct IngestControllerStats {
    pub num_rebalance_shards_ops: usize,
}

pub struct IngestController {
    ingester_pool: IngesterPool,
    metastore: MetastoreServiceClient,
    replication_factor: usize,
    // This lock ensures that only one rebalance operation is performed at a time.
    rebalance_lock: Arc<Mutex<()>>,
    pub stats: IngestControllerStats,
}

impl fmt::Debug for IngestController {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("IngestController")
            .field("ingester_pool", &self.ingester_pool)
            .field("metastore", &self.metastore)
            .field("replication_factor", &self.replication_factor)
            .finish()
    }
}

impl IngestController {
    pub fn new(
        metastore: MetastoreServiceClient,
        ingester_pool: IngesterPool,
        replication_factor: usize,
    ) -> Self {
        IngestController {
            metastore,
            ingester_pool,
            replication_factor,
            rebalance_lock: Arc::new(Mutex::new(())),
            stats: IngestControllerStats::default(),
        }
    }

    /// Sends a retain shard request to the given list of ingesters.
    ///
    /// If the request fails, we just log an error.
    pub(crate) fn sync_with_ingesters(
        &self,
        ingesters: &BTreeSet<NodeId>,
        model: &ControlPlaneModel,
    ) {
        for ingester in ingesters {
            self.sync_with_ingester(ingester, model);
        }
    }

    pub(crate) fn sync_with_all_ingesters(&self, model: &ControlPlaneModel) {
        let ingesters: Vec<NodeId> = self.ingester_pool.keys();
        for ingester in ingesters {
            self.sync_with_ingester(&ingester, model);
        }
    }

    /// Syncs the ingester in a fire and forget manner.
    ///
    /// The returned oneshot is just here for unit test to wait for the operation to terminate.
    fn sync_with_ingester(&self, ingester: &NodeId, model: &ControlPlaneModel) -> WaitHandle {
        info!(ingester = %ingester, "sync_with_ingester");
        let (wait_drop_guard, wait_handle) = WaitHandle::new();
        let Some(mut ingester_client) = self.ingester_pool.get(ingester) else {
            // TODO: (Maybe) We should mark the ingester as unavailable, and stop advertise its
            // shard to routers.
            warn!("failed to sync with ingester `{ingester}`: not available");
            return wait_handle;
        };
        let mut retain_shards_req = RetainShardsRequest::default();
        for (source_uid, shard_ids) in &*model.list_shards_for_node(ingester) {
            let shards_for_source = RetainShardsForSource {
                index_uid: Some(source_uid.index_uid.clone()),
                source_id: source_uid.source_id.clone(),
                shard_ids: shard_ids.iter().cloned().collect(),
            };
            retain_shards_req
                .retain_shards_for_sources
                .push(shards_for_source);
        }
        info!(ingester = %ingester, "retain shards ingester");
        let operation: String = format!("retain shards `{ingester}`");
        fire_and_forget(
            async move {
                if let Err(retain_shards_err) =
                    ingester_client.retain_shards(retain_shards_req).await
                {
                    error!(%retain_shards_err, "retain shards error");
                }
                // just a way to force moving the drop guard.
                drop(wait_drop_guard);
            },
            operation,
        );
        wait_handle
    }

    fn handle_closed_shards(&self, closed_shards: Vec<ShardIds>, model: &mut ControlPlaneModel) {
        for closed_shard in closed_shards {
            let index_uid: IndexUid = closed_shard.index_uid().clone();
            let source_id = closed_shard.source_id;

            let source_uid = SourceUid {
                index_uid,
                source_id,
            };
            let closed_shard_ids = model.close_shards(&source_uid, &closed_shard.shard_ids);

            if !closed_shard_ids.is_empty() {
                info!(
                    index_id=%source_uid.index_uid.index_id,
                    source_id=%source_uid.source_id,
                    shard_ids=?PrettySample::new(&closed_shard_ids, 5),
                    "closed {} shards reported by router",
                    closed_shard_ids.len()
                );
            }
        }
    }

    pub(crate) async fn handle_local_shards_update(
        &mut self,
        local_shards_update: LocalShardsUpdate,
        model: &mut ControlPlaneModel,
        progress: &Progress,
    ) {
        let shard_stats = model.update_shards(
            &local_shards_update.source_uid,
            &local_shards_update.shard_infos,
        );
        if shard_stats.avg_ingestion_rate >= SCALE_UP_SHARDS_THRESHOLD_MIB_PER_SEC {
            self.try_scale_up_shards(local_shards_update.source_uid, shard_stats, model, progress)
                .await;
        } else if shard_stats.avg_ingestion_rate <= SCALE_DOWN_SHARDS_THRESHOLD_MIB_PER_SEC
            && shard_stats.num_open_shards > 1
        {
            self.try_scale_down_shards(
                local_shards_update.source_uid,
                shard_stats,
                model,
                progress,
            )
            .await;
        }
    }

    fn handle_unavailable_leaders(
        &self,
        unavailable_leaders: &FnvHashSet<NodeId>,
        model: &mut ControlPlaneModel,
    ) {
        let mut confirmed_unavailable_leaders = FnvHashSet::default();

        for leader_id in unavailable_leaders {
            if !self.ingester_pool.contains_key(leader_id) {
                confirmed_unavailable_leaders.insert(leader_id.clone());
            } else {
                // TODO: If a majority of ingesters consistenly reports a leader as unavailable, we
                // should probably mark it as unavailable too.
            }
        }
        if !confirmed_unavailable_leaders.is_empty() {
            model.set_shards_as_unavailable(&confirmed_unavailable_leaders);
        }
    }

    /// Finds the open shards that satisfies the [`GetOrCreateOpenShardsRequest`] request sent by an
    /// ingest router. First, the control plane checks its internal shard table to find
    /// candidates. If it does not contain any, the control plane will ask
    /// the metastore to open new shards.
    pub(crate) async fn get_or_create_open_shards(
        &mut self,
        get_open_shards_request: GetOrCreateOpenShardsRequest,
        model: &mut ControlPlaneModel,
        progress: &Progress,
    ) -> ControlPlaneResult<GetOrCreateOpenShardsResponse> {
        self.handle_closed_shards(get_open_shards_request.closed_shards, model);

        let unavailable_leaders: FnvHashSet<NodeId> = get_open_shards_request
            .unavailable_leaders
            .into_iter()
            .map(|ingester_id| ingester_id.into())
            .collect();

        self.handle_unavailable_leaders(&unavailable_leaders, model);

        let num_subrequests = get_open_shards_request.subrequests.len();
        let mut get_or_create_open_shards_successes = Vec::with_capacity(num_subrequests);
        let mut get_or_create_open_shards_failures = Vec::new();
        let mut open_shards_subrequests = Vec::new();

        for get_open_shards_subrequest in get_open_shards_request.subrequests {
            let Some(index_uid) = model.index_uid(&get_open_shards_subrequest.index_id) else {
                let get_or_create_open_shards_failure = GetOrCreateOpenShardsFailure {
                    subrequest_id: get_open_shards_subrequest.subrequest_id,
                    index_id: get_open_shards_subrequest.index_id,
                    source_id: get_open_shards_subrequest.source_id,
                    reason: GetOrCreateOpenShardsFailureReason::IndexNotFound as i32,
                };
                get_or_create_open_shards_failures.push(get_or_create_open_shards_failure);
                continue;
            };
            let Some(open_shard_entries) = model.find_open_shards(
                &index_uid,
                &get_open_shards_subrequest.source_id,
                &unavailable_leaders,
            ) else {
                let get_or_create_open_shards_failure = GetOrCreateOpenShardsFailure {
                    subrequest_id: get_open_shards_subrequest.subrequest_id,
                    index_id: get_open_shards_subrequest.index_id,
                    source_id: get_open_shards_subrequest.source_id,
                    reason: GetOrCreateOpenShardsFailureReason::SourceNotFound as i32,
                };
                get_or_create_open_shards_failures.push(get_or_create_open_shards_failure);
                continue;
            };
            if !open_shard_entries.is_empty() {
                let open_shards: Vec<Shard> = open_shard_entries
                    .into_iter()
                    .map(|shard_entry| shard_entry.shard)
                    .collect();
                let get_or_create_open_shards_success = GetOrCreateOpenShardsSuccess {
                    subrequest_id: get_open_shards_subrequest.subrequest_id,
                    index_uid: index_uid.into(),
                    source_id: get_open_shards_subrequest.source_id,
                    open_shards,
                };
                get_or_create_open_shards_successes.push(get_or_create_open_shards_success);
            } else {
                let shard_id = ShardId::from(Ulid::new());
                let open_shard_subrequest = metastore::OpenShardSubrequest {
                    subrequest_id: get_open_shards_subrequest.subrequest_id,
                    index_uid: index_uid.into(),
                    source_id: get_open_shards_subrequest.source_id,
                    shard_id: Some(shard_id),
                    // These attributes will be overwritten in the next stage.
                    leader_id: "".to_string(),
                    follower_id: None,
                };
                open_shards_subrequests.push(open_shard_subrequest);
            }
        }
        if !open_shards_subrequests.is_empty() {
            if let Some(leader_follower_pairs) =
                self.allocate_shards(open_shards_subrequests.len(), &unavailable_leaders, model)
            {
                for (open_shards_subrequest, (leader_id, follower_opt)) in open_shards_subrequests
                    .iter_mut()
                    .zip(leader_follower_pairs)
                {
                    open_shards_subrequest.leader_id = leader_id.into();
                    open_shards_subrequest.follower_id = follower_opt.map(Into::into);
                }
                let open_shards_request = metastore::OpenShardsRequest {
                    subrequests: open_shards_subrequests,
                };
                let open_shards_response = progress
                    .protect_future(self.metastore.open_shards(open_shards_request))
                    .await?;

                let init_shards_response = self
                    .init_shards(&open_shards_response.subresponses, progress)
                    .await;

                for init_shard_success in init_shards_response.successes {
                    let shard = init_shard_success.shard().clone();
                    let index_uid = shard.index_uid().clone();
                    let source_id = shard.source_id.clone();
                    model.insert_shards(&index_uid, &source_id, vec![shard]);

                    if let Some(open_shard_entries) =
                        model.find_open_shards(&index_uid, &source_id, &unavailable_leaders)
                    {
                        let open_shards = open_shard_entries
                            .into_iter()
                            .map(|shard_entry| shard_entry.shard)
                            .collect();
                        let get_or_create_open_shards_success = GetOrCreateOpenShardsSuccess {
                            subrequest_id: init_shard_success.subrequest_id,
                            index_uid: Some(index_uid),
                            source_id,
                            open_shards,
                        };
                        get_or_create_open_shards_successes.push(get_or_create_open_shards_success);
                    }
                }
            } else {
                for open_shards_subrequest in open_shards_subrequests {
                    let get_or_create_open_shards_failure = GetOrCreateOpenShardsFailure {
                        subrequest_id: open_shards_subrequest.subrequest_id,
                        index_id: open_shards_subrequest.index_uid().index_id.clone(),
                        source_id: open_shards_subrequest.source_id,
                        reason: GetOrCreateOpenShardsFailureReason::NoIngestersAvailable as i32,
                    };
                    get_or_create_open_shards_failures.push(get_or_create_open_shards_failure);
                }
            }
        }
        let response = GetOrCreateOpenShardsResponse {
            successes: get_or_create_open_shards_successes,
            failures: get_or_create_open_shards_failures,
        };
        Ok(response)
    }

    /// Allocates and assigns new shards to ingesters.
    fn allocate_shards(
        &self,
        num_shards_to_allocate: usize,
        unavailable_leaders: &FnvHashSet<NodeId>,
        model: &ControlPlaneModel,
    ) -> Option<Vec<(NodeId, Option<NodeId>)>> {
        let ingesters: Vec<NodeId> = self
            .ingester_pool
            .keys()
            .into_iter()
            .filter(|ingester| !unavailable_leaders.contains(ingester))
            .sorted_by(|left, right| left.cmp(right))
            .collect();

        let num_ingesters = ingesters.len();

        if num_ingesters == 0 {
            warn!("failed to allocate {num_shards_to_allocate} shards: no ingesters available");
            return None;
        } else if self.replication_factor > num_ingesters {
            warn!(
                "failed to allocate {num_shards_to_allocate} shards: replication factor is \
                 greater than the number of available ingesters"
            );
            return None;
        }
        let mut leader_follower_pairs = Vec::with_capacity(num_shards_to_allocate);

        let mut num_open_shards: usize = 0;
        let mut per_leader_num_open_shards: HashMap<&str, usize> =
            HashMap::with_capacity(num_ingesters);

        for shard in model.all_shards() {
            if shard.is_open() && !unavailable_leaders.contains(&shard.leader_id) {
                num_open_shards += 1;

                *per_leader_num_open_shards
                    .entry(&shard.leader_id)
                    .or_default() += 1;
            }
        }
        let mut num_remaining_shards_to_allocate = num_shards_to_allocate;
        let num_open_shards_target = num_shards_to_allocate + num_open_shards;
        let max_num_shards_to_allocate_per_node = num_open_shards_target / num_ingesters;

        // Allocate at most `max_num_shards_to_allocate_per_node` shards to each ingester.
        for (leader_id, follower_id) in ingesters.iter().zip(ingesters.iter().cycle().skip(1)) {
            if num_remaining_shards_to_allocate == 0 {
                break;
            }
            let num_open_shards_inner = per_leader_num_open_shards
                .get(leader_id.as_str())
                .copied()
                .unwrap_or_default();

            let num_shards_to_allocate_inner = max_num_shards_to_allocate_per_node
                .saturating_sub(num_open_shards_inner)
                .min(num_remaining_shards_to_allocate);

            for _ in 0..num_shards_to_allocate_inner {
                num_remaining_shards_to_allocate -= 1;

                let leader = leader_id.clone();
                let mut follower_opt = None;

                if self.replication_factor > 1 {
                    follower_opt = Some(follower_id.clone());
                }
                leader_follower_pairs.push((leader, follower_opt));
            }
        }
        // Allocate remaining shards one by one.
        for (leader_id, follower_id) in ingesters.iter().zip(ingesters.iter().cycle().skip(1)) {
            if num_remaining_shards_to_allocate == 0 {
                break;
            }
            num_remaining_shards_to_allocate -= 1;

            let leader = leader_id.clone();
            let mut follower_opt = None;

            if self.replication_factor > 1 {
                follower_opt = Some(follower_id.clone());
            }
            leader_follower_pairs.push((leader, follower_opt));
        }
        Some(leader_follower_pairs)
    }

    /// Calls init shards on the leaders hosting newly opened shards.
    async fn init_shards(
        &self,
        open_shards_subresponses: &[metastore::OpenShardSubresponse],
        progress: &Progress,
    ) -> InitShardsResponse {
        let mut successes = Vec::with_capacity(open_shards_subresponses.len());
        let mut failures = Vec::new();

        let mut per_leader_shards_to_init: HashMap<&String, Vec<InitShardSubrequest>> =
            HashMap::default();

        for subresponse in open_shards_subresponses {
            let shard = subresponse.open_shard();
            let init_shards_subrequest = InitShardSubrequest {
                subrequest_id: subresponse.subrequest_id,
                shard: Some(shard.clone()),
            };
            per_leader_shards_to_init
                .entry(&shard.leader_id)
                .or_default()
                .push(init_shards_subrequest);
        }
        let mut init_shards_futures = FuturesUnordered::new();

        for (leader_id, subrequests) in per_leader_shards_to_init {
            let init_shard_failures: Vec<InitShardFailure> = subrequests
                .iter()
                .map(|subrequest| {
                    let shard = subrequest.shard();

                    InitShardFailure {
                        subrequest_id: subrequest.subrequest_id,
                        index_uid: Some(shard.index_uid().clone()),
                        source_id: shard.source_id.clone(),
                        shard_id: Some(shard.shard_id().clone()),
                    }
                })
                .collect();
            let Some(mut leader) = self.ingester_pool.get(leader_id) else {
                warn!("failed to init shards: ingester `{leader_id}` is unavailable");
                failures.extend(init_shard_failures);
                continue;
            };
            let init_shards_request = InitShardsRequest { subrequests };
            let init_shards_future = async move {
                let init_shards_result = tokio::time::timeout(
                    INIT_SHARDS_REQUEST_TIMEOUT,
                    leader.init_shards(init_shards_request),
                )
                .await;
                (leader_id.clone(), init_shards_result, init_shard_failures)
            };
            init_shards_futures.push(init_shards_future);
        }
        while let Some((leader_id, init_shards_result, init_shard_failures)) =
            progress.protect_future(init_shards_futures.next()).await
        {
            match init_shards_result {
                Ok(Ok(init_shards_response)) => {
                    successes.extend(init_shards_response.successes);
                    failures.extend(init_shards_response.failures);
                }
                Ok(Err(error)) => {
                    error!(%error, "failed to init shards on `{leader_id}`");
                    failures.extend(init_shard_failures);
                }
                Err(_elapsed) => {
                    error!("failed to init shards on `{leader_id}`: request timed out");
                    failures.extend(init_shard_failures);
                }
            }
        }
        InitShardsResponse {
            successes,
            failures,
        }
    }

    /// Attempts to increase the number of shards. This operation is rate limited to avoid creating
    /// to many shards in a short period of time. As a result, this method may not create any
    /// shard.
    async fn try_scale_up_shards(
        &mut self,
        source_uid: SourceUid,
        shard_stats: ShardStats,
        model: &mut ControlPlaneModel,
        progress: &Progress,
    ) {
        const NUM_PERMITS: u64 = 1;

        if !model
            .acquire_scaling_permits(&source_uid, ScalingMode::Up, NUM_PERMITS)
            .unwrap_or(false)
        {
            return;
        }
        let new_num_open_shards = shard_stats.num_open_shards + 1;

        info!(
            index_id=%source_uid.index_uid.index_id,
            source_id=%source_uid.source_id,
            "scaling up number of shards to {new_num_open_shards}"
        );
        let unavailable_leaders: FnvHashSet<NodeId> = FnvHashSet::default();

        let Some((leader_id, follower_id)) = self
            .allocate_shards(1, &unavailable_leaders, model)
            .and_then(|pairs| pairs.into_iter().next())
        else {
            warn!("failed to scale up number of shards: no ingesters available");
            model.release_scaling_permits(&source_uid, ScalingMode::Up, NUM_PERMITS);
            return;
        };
        let shard_id = ShardId::from(Ulid::new());
        let open_shard_subrequest = metastore::OpenShardSubrequest {
            subrequest_id: 0,
            index_uid: source_uid.index_uid.clone().into(),
            source_id: source_uid.source_id.clone(),
            shard_id: Some(shard_id),
            leader_id: leader_id.into(),
            follower_id: follower_id.map(Into::into),
        };
        let open_shards_request = metastore::OpenShardsRequest {
            subrequests: vec![open_shard_subrequest],
        };
        let open_shards_response = match progress
            .protect_future(self.metastore.open_shards(open_shards_request))
            .await
        {
            Ok(open_shards_response) => open_shards_response,
            Err(error) => {
                warn!("failed to scale up number of shards: {error}");
                model.release_scaling_permits(&source_uid, ScalingMode::Up, NUM_PERMITS);
                return;
            }
        };
        let init_shards_response = self
            .init_shards(&open_shards_response.subresponses, progress)
            .await;

        if init_shards_response.successes.is_empty() {
            warn!("failed to scale up number of shards");
            model.release_scaling_permits(&source_uid, ScalingMode::Up, NUM_PERMITS);
            return;
        }
        for init_shard_success in init_shards_response.successes {
            let open_shard = init_shard_success.shard().clone();
            let index_uid = open_shard.index_uid().clone();
            let source_id = open_shard.source_id.clone();
            let open_shards = vec![open_shard];
            model.insert_shards(&index_uid, &source_id, open_shards);
        }
    }

    /// Attempts to decrease the number of shards. This operation is rate limited to avoid closing
    /// shards too aggressively. As a result, this method may not close any shard.
    async fn try_scale_down_shards(
        &self,
        source_uid: SourceUid,
        shard_stats: ShardStats,
        model: &mut ControlPlaneModel,
        progress: &Progress,
    ) {
        const NUM_PERMITS: u64 = 1;

        if !model
            .acquire_scaling_permits(&source_uid, ScalingMode::Down, NUM_PERMITS)
            .unwrap_or(false)
        {
            return;
        }
        let new_num_open_shards = shard_stats.num_open_shards - 1;

        info!(
            index_id=%source_uid.index_uid.index_id,
            source_id=%source_uid.source_id,
            "scaling down number of shards to {new_num_open_shards}"
        );
        let Some((leader_id, shard_id)) = find_scale_down_candidate(&source_uid, model) else {
            model.release_scaling_permits(&source_uid, ScalingMode::Down, NUM_PERMITS);
            return;
        };
        let Some(mut ingester) = self.ingester_pool.get(&leader_id) else {
            model.release_scaling_permits(&source_uid, ScalingMode::Down, NUM_PERMITS);
            return;
        };
        let shard_pkeys = vec![ShardPKey {
            index_uid: source_uid.index_uid.clone().into(),
            source_id: source_uid.source_id.clone(),
            shard_id: Some(shard_id.clone()),
        }];
        let close_shards_request = CloseShardsRequest { shard_pkeys };
        if let Err(error) = progress
            .protect_future(ingester.close_shards(close_shards_request))
            .await
        {
            warn!("failed to scale down number of shards: {error}");
            model.release_scaling_permits(&source_uid, ScalingMode::Down, NUM_PERMITS);
            return;
        }
        model.close_shards(&source_uid, &[shard_id]);
    }

    pub(crate) fn advise_reset_shards(
        &self,
        request: AdviseResetShardsRequest,
        model: &ControlPlaneModel,
    ) -> AdviseResetShardsResponse {
        info!("advise reset shards");
        debug!(shard_ids=?summarize_shard_ids(&request.shard_ids), "advise reset shards");

        let mut shards_to_delete: Vec<ShardIds> = Vec::new();
        let mut shards_to_truncate: Vec<ShardIdPositions> = Vec::new();

        for shard_ids in request.shard_ids {
            let index_uid = shard_ids.index_uid().clone();
            let source_id = shard_ids.source_id.clone();

            let source_uid = SourceUid {
                index_uid,
                source_id,
            };
            let Some(shard_entries) = model.get_shards_for_source(&source_uid) else {
                // The source no longer exists: we can safely delete all the shards.
                shards_to_delete.push(shard_ids);
                continue;
            };
            let mut shard_ids_to_delete = Vec::new();
            let mut shard_positions_to_truncate = Vec::new();

            for shard_id in shard_ids.shard_ids {
                if let Some(shard_entry) = shard_entries.get(&shard_id) {
                    let publish_position_inclusive =
                        shard_entry.publish_position_inclusive().clone();

                    shard_positions_to_truncate.push(ShardIdPosition {
                        shard_id: Some(shard_id),
                        publish_position_inclusive: Some(publish_position_inclusive),
                    });
                } else {
                    shard_ids_to_delete.push(shard_id);
                }
            }
            if !shard_ids_to_delete.is_empty() {
                shards_to_delete.push(ShardIds {
                    index_uid: Some(source_uid.index_uid.clone()),
                    source_id: source_uid.source_id.clone(),
                    shard_ids: shard_ids_to_delete,
                });
            }
            if !shard_positions_to_truncate.is_empty() {
                shards_to_truncate.push(ShardIdPositions {
                    index_uid: Some(source_uid.index_uid),
                    source_id: source_uid.source_id,
                    shard_positions: shard_positions_to_truncate,
                });
            }
        }

        if enabled!(Level::DEBUG) {
            let shards_to_truncate: Vec<(&str, &Position)> = shards_to_truncate
                .iter()
                .flat_map(|shard_positions| {
                    shard_positions
                        .shard_positions
                        .iter()
                        .map(|shard_id_position| {
                            (
                                shard_id_position.shard_id().as_str(),
                                shard_id_position.publish_position_inclusive(),
                            )
                        })
                })
                .collect();
            debug!(shard_ids_to_delete=?summarize_shard_ids(&shards_to_delete), shards_to_truncate=?shards_to_truncate, "advise reset shards response");
        }

        AdviseResetShardsResponse {
            shards_to_delete,
            shards_to_truncate,
        }
    }

    /// Moves shards from ingesters with too many shards to ingesters with too few shards. Moving a
    /// shard consists of closing the shard on the source ingester and opening a new one on the
    /// target ingester.
    ///
    /// This method is guarded by a lock to ensure that only one rebalance operation is performed at
    /// a time.
    pub(crate) async fn rebalance_shards(
        &mut self,
        model: &mut ControlPlaneModel,
        mailbox: &Mailbox<ControlPlane>,
        progress: &Progress,
    ) -> Option<JoinHandle<()>> {
        let Ok(rebalance_guard) = self.rebalance_lock.clone().try_lock_owned() else {
            return None;
        };
        self.stats.num_rebalance_shards_ops += 1;

        let num_ingesters = self.ingester_pool.len();
        let mut num_open_shards: usize = 0;

        if num_ingesters == 0 {
            return None;
        }
        let mut per_leader_open_shards: HashMap<&str, Vec<&ShardEntry>> =
            HashMap::with_capacity(num_ingesters);

        for shard in model.all_shards() {
            if shard.is_open() {
                num_open_shards += 1;

                per_leader_open_shards
                    .entry(&shard.leader_id)
                    .or_default()
                    .push(shard);
            }
        }
        let num_open_shards_per_leader_target = num_open_shards / num_ingesters;
        let num_open_shards_per_leader_threshold = cmp::max(
            num_open_shards_per_leader_target * 12 / 10,
            num_open_shards_per_leader_target + 1,
        );
        let mut shards_to_move: Vec<&ShardEntry> = Vec::new();

        for open_shards in per_leader_open_shards.values() {
            if open_shards.len() > num_open_shards_per_leader_threshold {
                shards_to_move.extend(&open_shards[num_open_shards_per_leader_threshold..]);
            }
        }
        if shards_to_move.is_empty() {
            return None;
        }
        info!("rebalancing {} shards", shards_to_move.len());
        let num_shards_to_move = shards_to_move.len();
        let unavailable_leaders: FnvHashSet<NodeId> = FnvHashSet::default();

        let leader_follower_pairs =
            self.allocate_shards(num_shards_to_move, &unavailable_leaders, model)?;
        let mut open_shards_subrequests = Vec::with_capacity(num_shards_to_move);
        let mut shards_to_close: HashMap<ShardId, (LeaderId, ShardPKey)> =
            HashMap::with_capacity(num_shards_to_move);

        for (subrequest_id, (shard_to_move, (leader_id, follower_id_opt))) in
            zip(&shards_to_move, leader_follower_pairs).enumerate()
        {
            let shard_id = ShardId::from(Ulid::new());
            let open_shard_subrequest = metastore::OpenShardSubrequest {
                subrequest_id: subrequest_id as u32,
                index_uid: shard_to_move.index_uid.clone(),
                source_id: shard_to_move.source_id.clone(),
                shard_id: Some(shard_id.clone()),
                leader_id: leader_id.into(),
                follower_id: follower_id_opt.map(Into::into),
            };
            open_shards_subrequests.push(open_shard_subrequest);

            let leader_id = NodeId::from(shard_to_move.leader_id.clone());
            let shard_pkey = ShardPKey {
                index_uid: shard_to_move.index_uid.clone(),
                source_id: shard_to_move.source_id.clone(),
                shard_id: shard_to_move.shard_id.clone(),
            };
            shards_to_close.insert(shard_id, (leader_id, shard_pkey));
        }
        let open_shards_request = metastore::OpenShardsRequest {
            subrequests: open_shards_subrequests,
        };
        let open_shards_response = match progress
            .protect_future(self.metastore.open_shards(open_shards_request))
            .await
        {
            Ok(open_shards_response) => open_shards_response,
            Err(error) => {
                error!(%error, "failed to rebalance shards");
                return None;
            }
        };
        let init_shards_response = self
            .init_shards(&open_shards_response.subresponses, progress)
            .await;

        for init_shard_success in init_shards_response.successes {
            let shard = init_shard_success.shard().clone();
            let index_uid = shard.index_uid().clone();
            let source_id = shard.source_id.clone();
            model.insert_shards(&index_uid, &source_id, vec![shard]);

            let source_uid = SourceUid {
                index_uid,
                source_id,
            };
            // We temporarily disable the ability the scale down the number of shards for the source
            // to avoid closing the shards we just opened.
            model.drain_scaling_permits(&source_uid, ScalingMode::Down);
        }
        for init_shard_failure in init_shards_response.failures {
            let shard_id = init_shard_failure.shard_id();
            shards_to_close.remove(shard_id);
        }
        let close_shards_fut = self.close_shards(shards_to_close.into_values());
        let mailbox_clone = mailbox.clone();

        let close_shards_and_send_callback_fut = async move {
            // We wait for a few seconds before closing the shards to give the ingesters some time
            // to learn about the ones we just opened via gossip.
            tokio::time::sleep(CLOSE_SHARDS_UPON_REBALANCE_DELAY).await;

            let closed_shards = close_shards_fut.await;

            if closed_shards.is_empty() {
                return;
            }
            let callback = RebalanceShardsCallback {
                closed_shards,
                rebalance_guard,
            };
            let _ = mailbox_clone.send_message(callback).await;
        };
        Some(tokio::spawn(close_shards_and_send_callback_fut))
    }

    fn close_shards(
        &self,
        shards_to_close: impl Iterator<Item = (LeaderId, ShardPKey)>,
    ) -> impl Future<Output = Vec<ShardPKey>> + Send + 'static {
        let mut per_leader_shards_to_close: HashMap<LeaderId, Vec<ShardPKey>> = HashMap::new();

        for (leader_id, shard_pkey) in shards_to_close {
            per_leader_shards_to_close
                .entry(leader_id)
                .or_default()
                .push(shard_pkey);
        }
        let mut close_shards_futures = FuturesUnordered::new();

        for (leader_id, shard_pkeys) in per_leader_shards_to_close {
            let Some(mut ingester) = self.ingester_pool.get(&leader_id) else {
                warn!("failed to close shards: ingester `{leader_id}` is unavailable");
                continue;
            };
            let shards_to_close_request = CloseShardsRequest { shard_pkeys };
            let close_shards_future = async move {
                tokio::time::timeout(
                    CLOSE_SHARDS_REQUEST_TIMEOUT,
                    ingester.close_shards(shards_to_close_request),
                )
                .await
            };
            close_shards_futures.push(close_shards_future);
        }
        async move {
            let mut closed_shards = Vec::new();

            while let Some(close_shards_result) = close_shards_futures.next().await {
                match close_shards_result {
                    Ok(Ok(CloseShardsResponse { successes })) => {
                        closed_shards.extend(successes);
                    }
                    Ok(Err(error)) => {
                        error!(%error, "failed to close shards");
                    }
                    Err(_elapsed) => {
                        error!("close shards request timed out");
                    }
                }
            }
            closed_shards
        }
    }
}

fn summarize_shard_ids(shard_ids: &[ShardIds]) -> Vec<&str> {
    shard_ids
        .iter()
        .flat_map(|source_shard_ids| {
            source_shard_ids
                .shard_ids
                .iter()
                .map(|shard_id| shard_id.as_str())
        })
        .collect()
}

/// When rebalancing shards, shards to move are closed some time after new shards are opened.
/// Because we don't want to stall the control plane event loop while waiting for the close shards
/// requests to complete, we use a callback to handle the results of those close shards requests.
#[derive(Debug)]
pub(crate) struct RebalanceShardsCallback {
    pub closed_shards: Vec<ShardPKey>,
    pub rebalance_guard: OwnedMutexGuard<()>,
}

/// Finds the shard with the highest ingestion rate on the ingester with the most number of open
/// shards. If multiple shards have the same ingestion rate, the shard with the lowest (oldest)
/// shard ID is chosen.
fn find_scale_down_candidate(
    source_uid: &SourceUid,
    model: &ControlPlaneModel,
) -> Option<(NodeId, ShardId)> {
    let mut per_leader_candidates: HashMap<&String, (usize, &ShardEntry)> = HashMap::new();

    for shard in model.get_shards_for_source(source_uid)?.values() {
        if shard.is_open() {
            per_leader_candidates
                .entry(&shard.leader_id)
                .and_modify(|(num_shards, candidate)| {
                    *num_shards += 1;

                    if shard
                        .ingestion_rate
                        .cmp(&candidate.ingestion_rate)
                        .then_with(|| shard.shard_id.cmp(&candidate.shard_id))
                        .is_gt()
                    {
                        *candidate = shard;
                    }
                })
                .or_insert((1, shard));
        }
    }
    per_leader_candidates
        .into_iter()
        .min_by_key(|(_leader_id, (num_shards, _shard))| *num_shards)
        .map(|(leader_id, (_num_shards, shard))| {
            (leader_id.clone().into(), shard.shard_id().clone())
        })
}

#[cfg(test)]
mod tests {

    use std::collections::BTreeSet;
    use std::iter::empty;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use quickwit_actors::Universe;
    use quickwit_common::setup_logging_for_tests;
    use quickwit_common::tower::DelayLayer;
    use quickwit_config::{SourceConfig, INGEST_V2_SOURCE_ID};
    use quickwit_ingest::{RateMibPerSec, ShardInfo};
    use quickwit_metastore::IndexMetadata;
    use quickwit_proto::control_plane::GetOrCreateOpenShardsSubrequest;
    use quickwit_proto::ingest::ingester::{
        CloseShardsResponse, IngesterServiceClient, InitShardSuccess, InitShardsResponse,
        MockIngesterService, RetainShardsResponse,
    };
    use quickwit_proto::ingest::{IngestV2Error, Shard, ShardState};
    use quickwit_proto::metastore::{MetastoreError, MockMetastoreService};
    use quickwit_proto::types::{Position, SourceId};

    use super::*;

    #[tokio::test]
    async fn test_ingest_controller_get_or_create_open_shards() {
        let source_id: &'static str = "test-source";

        let index_id_0 = "test-index-0";
        let index_metadata_0 = IndexMetadata::for_test(index_id_0, "ram://indexes/test-index-0");
        let index_uid_0 = index_metadata_0.index_uid.clone();

        let index_id_1 = "test-index-1";
        let index_metadata_1 = IndexMetadata::for_test(index_id_1, "ram://indexes/test-index-1");
        let index_uid_1 = index_metadata_1.index_uid.clone();

        let progress = Progress::default();

        let mut mock_metastore = MockMetastoreService::new();
        mock_metastore.expect_open_shards().once().returning({
            let index_uid_1 = index_uid_1.clone();

            move |request| {
                assert_eq!(request.subrequests.len(), 1);
                assert_eq!(request.subrequests[0].index_uid(), &index_uid_1);
                assert_eq!(&request.subrequests[0].source_id, source_id);

                let subresponses = vec![metastore::OpenShardSubresponse {
                    subrequest_id: 1,
                    open_shard: Some(Shard {
                        index_uid: index_uid_1.clone().into(),
                        source_id: source_id.to_string(),
                        shard_id: Some(ShardId::from(1)),
                        shard_state: ShardState::Open as i32,
                        leader_id: "test-ingester-2".to_string(),
                        ..Default::default()
                    }),
                }];
                let response = metastore::OpenShardsResponse { subresponses };
                Ok(response)
            }
        });
        let metastore = MetastoreServiceClient::from_mock(mock_metastore);

        let mock_ingester = MockIngesterService::new();
        let ingester = IngesterServiceClient::from_mock(mock_ingester);

        let ingester_pool = IngesterPool::default();
        ingester_pool.insert("test-ingester-1".into(), ingester.clone());

        let mut mock_ingester = MockIngesterService::new();
        let index_uid_1_clone = index_uid_1.clone();
        mock_ingester
            .expect_init_shards()
            .once()
            .returning(move |request| {
                assert_eq!(request.subrequests.len(), 1);

                let subrequest = &request.subrequests[0];
                assert_eq!(subrequest.subrequest_id, 1);

                let shard = subrequest.shard();
                assert_eq!(shard.index_uid(), &index_uid_1_clone);
                assert_eq!(shard.source_id, "test-source");
                assert_eq!(shard.shard_id(), ShardId::from(1));
                assert_eq!(shard.leader_id, "test-ingester-2");

                let successes = vec![InitShardSuccess {
                    subrequest_id: request.subrequests[0].subrequest_id,
                    shard: Some(shard.clone()),
                }];
                let response = InitShardsResponse {
                    successes,
                    failures: Vec::new(),
                };
                Ok(response)
            });
        let ingester = IngesterServiceClient::from_mock(mock_ingester);
        ingester_pool.insert("test-ingester-2".into(), ingester.clone());

        let replication_factor = 2;
        let mut ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let mut model = ControlPlaneModel::default();
        model.add_index(index_metadata_0.clone());
        model.add_index(index_metadata_1.clone());

        let mut source_config = SourceConfig::ingest_v2();
        source_config.source_id = source_id.to_string();

        model
            .add_source(&index_uid_0, source_config.clone())
            .unwrap();
        model.add_source(&index_uid_1, source_config).unwrap();

        let shards = vec![
            Shard {
                index_uid: index_uid_0.clone().into(),
                source_id: source_id.to_string(),
                shard_id: Some(ShardId::from(1)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
            Shard {
                index_uid: index_uid_0.clone().into(),
                source_id: source_id.to_string(),
                shard_id: Some(ShardId::from(2)),
                leader_id: "test-ingester-1".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
        ];

        model.insert_shards(&index_uid_0, &source_id.into(), shards);

        let request = GetOrCreateOpenShardsRequest {
            subrequests: Vec::new(),
            closed_shards: Vec::new(),
            unavailable_leaders: Vec::new(),
        };
        let response = ingest_controller
            .get_or_create_open_shards(request, &mut model, &progress)
            .await
            .unwrap();

        assert_eq!(response.successes.len(), 0);
        assert_eq!(response.failures.len(), 0);

        let subrequests = vec![
            GetOrCreateOpenShardsSubrequest {
                subrequest_id: 0,
                index_id: "test-index-0".to_string(),
                source_id: source_id.to_string(),
            },
            GetOrCreateOpenShardsSubrequest {
                subrequest_id: 1,
                index_id: "test-index-1".to_string(),
                source_id: source_id.to_string(),
            },
            GetOrCreateOpenShardsSubrequest {
                subrequest_id: 2,
                index_id: "index-not-found".to_string(),
                source_id: "source-not-found".to_string(),
            },
            GetOrCreateOpenShardsSubrequest {
                subrequest_id: 3,
                index_id: "test-index-0".to_string(),
                source_id: "source-not-found".to_string(),
            },
        ];
        let closed_shards = Vec::new();
        let unavailable_leaders = vec!["test-ingester-0".to_string()];
        let request = GetOrCreateOpenShardsRequest {
            subrequests,
            closed_shards,
            unavailable_leaders,
        };
        let response = ingest_controller
            .get_or_create_open_shards(request, &mut model, &progress)
            .await
            .unwrap();

        assert_eq!(response.successes.len(), 2);
        assert_eq!(response.failures.len(), 2);

        let success = &response.successes[0];
        assert_eq!(success.subrequest_id, 0);
        assert_eq!(success.index_uid(), &index_uid_0);
        assert_eq!(success.source_id, source_id);
        assert_eq!(success.open_shards.len(), 1);
        assert_eq!(success.open_shards[0].shard_id(), ShardId::from(2));
        assert_eq!(success.open_shards[0].leader_id, "test-ingester-1");

        let success = &response.successes[1];
        assert_eq!(success.subrequest_id, 1);
        assert_eq!(success.index_uid(), &index_uid_1);
        assert_eq!(success.source_id, source_id);
        assert_eq!(success.open_shards.len(), 1);
        assert_eq!(success.open_shards[0].shard_id(), ShardId::from(1));
        assert_eq!(success.open_shards[0].leader_id, "test-ingester-2");

        let failure = &response.failures[0];
        assert_eq!(failure.subrequest_id, 2);
        assert_eq!(failure.index_id, "index-not-found");
        assert_eq!(failure.source_id, "source-not-found");
        assert_eq!(
            failure.reason(),
            GetOrCreateOpenShardsFailureReason::IndexNotFound
        );

        let failure = &response.failures[1];
        assert_eq!(failure.subrequest_id, 3);
        assert_eq!(failure.index_id, index_id_0);
        assert_eq!(failure.source_id, "source-not-found");
        assert_eq!(
            failure.reason(),
            GetOrCreateOpenShardsFailureReason::SourceNotFound
        );

        assert_eq!(model.num_shards(), 3);
    }

    #[tokio::test]
    async fn test_ingest_controller_get_open_shards_handles_closed_shards() {
        let metastore = MetastoreServiceClient::mocked();
        let ingester_pool = IngesterPool::default();
        let replication_factor = 2;

        let mut ingest_controller =
            IngestController::new(metastore, ingester_pool, replication_factor);
        let mut model = ControlPlaneModel::default();

        let index_uid = IndexUid::for_test("test-index-0", 0);
        let source_id: SourceId = "test-source".into();

        let shards = vec![Shard {
            shard_id: Some(ShardId::from(1)),
            index_uid: Some(index_uid.clone()),
            source_id: source_id.clone(),
            leader_id: "test-ingester-0".to_string(),
            shard_state: ShardState::Open as i32,
            ..Default::default()
        }];
        model.insert_shards(&index_uid, &source_id, shards);

        let request = GetOrCreateOpenShardsRequest {
            subrequests: Vec::new(),
            closed_shards: vec![ShardIds {
                index_uid: index_uid.clone().into(),
                source_id: source_id.clone(),
                shard_ids: vec![ShardId::from(1), ShardId::from(2)],
            }],
            unavailable_leaders: Vec::new(),
        };
        let progress = Progress::default();

        ingest_controller
            .get_or_create_open_shards(request, &mut model, &progress)
            .await
            .unwrap();

        let shard_1 = model
            .all_shards()
            .find(|shard| shard.shard_id() == ShardId::from(1))
            .unwrap();
        assert!(shard_1.is_closed());
    }

    #[tokio::test]
    async fn test_ingest_controller_get_open_shards_handles_unavailable_leaders() {
        let metastore = MetastoreServiceClient::mocked();

        let ingester_pool = IngesterPool::default();
        let ingester_1 = IngesterServiceClient::mocked();
        ingester_pool.insert("test-ingester-1".into(), ingester_1);

        let replication_factor = 2;

        let mut ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);
        let mut model = ControlPlaneModel::default();

        let index_uid = IndexUid::for_test("test-index-0", 0);
        let source_id: SourceId = "test-source".into();

        let shards = vec![
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(1)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(2)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Closed as i32,
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(3)),
                leader_id: "test-ingester-1".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
        ];
        model.insert_shards(&index_uid, &source_id, shards);

        let request = GetOrCreateOpenShardsRequest {
            subrequests: Vec::new(),
            closed_shards: Vec::new(),
            unavailable_leaders: vec!["test-ingester-0".to_string()],
        };
        let progress = Progress::default();

        ingest_controller
            .get_or_create_open_shards(request, &mut model, &progress)
            .await
            .unwrap();

        let shard_1 = model
            .all_shards()
            .find(|shard| shard.shard_id() == ShardId::from(1))
            .unwrap();
        assert!(shard_1.is_unavailable());

        let shard_2 = model
            .all_shards()
            .find(|shard| shard.shard_id() == ShardId::from(2))
            .unwrap();
        assert!(shard_2.is_closed());

        let shard_3 = model
            .all_shards()
            .find(|shard| shard.shard_id() == ShardId::from(3))
            .unwrap();
        assert!(shard_3.is_open());
    }

    #[test]
    fn test_ingest_controller_allocate_shards() {
        let metastore = MetastoreServiceClient::mocked();
        let ingester_pool = IngesterPool::default();
        let replication_factor = 2;

        let ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let mut model = ControlPlaneModel::default();

        let leader_follower_pairs_opt =
            ingest_controller.allocate_shards(0, &FnvHashSet::default(), &model);
        assert!(leader_follower_pairs_opt.is_none());

        ingester_pool.insert(
            "test-ingester-1".into(),
            IngesterServiceClient::from_mock(MockIngesterService::new()),
        );

        let leader_follower_pairs_opt =
            ingest_controller.allocate_shards(0, &FnvHashSet::default(), &model);
        assert!(leader_follower_pairs_opt.is_none());

        ingester_pool.insert(
            "test-ingester-2".into(),
            IngesterServiceClient::from_mock(MockIngesterService::new()),
        );

        let leader_follower_pairs = ingest_controller
            .allocate_shards(0, &FnvHashSet::default(), &model)
            .unwrap();
        assert!(leader_follower_pairs.is_empty());

        let leader_follower_pairs = ingest_controller
            .allocate_shards(1, &FnvHashSet::default(), &model)
            .unwrap();
        assert_eq!(leader_follower_pairs.len(), 1);
        assert_eq!(leader_follower_pairs[0].0, "test-ingester-1");
        assert_eq!(
            leader_follower_pairs[0].1,
            Some(NodeId::from("test-ingester-2"))
        );

        let leader_follower_pairs = ingest_controller
            .allocate_shards(2, &FnvHashSet::default(), &model)
            .unwrap();
        assert_eq!(leader_follower_pairs.len(), 2);
        assert_eq!(leader_follower_pairs[0].0, "test-ingester-1");
        assert_eq!(
            leader_follower_pairs[0].1,
            Some(NodeId::from("test-ingester-2"))
        );

        assert_eq!(leader_follower_pairs[1].0, "test-ingester-2");
        assert_eq!(
            leader_follower_pairs[1].1,
            Some(NodeId::from("test-ingester-1"))
        );

        let leader_follower_pairs = ingest_controller
            .allocate_shards(3, &FnvHashSet::default(), &model)
            .unwrap();
        assert_eq!(leader_follower_pairs.len(), 3);
        assert_eq!(leader_follower_pairs[0].0, "test-ingester-1");
        assert_eq!(
            leader_follower_pairs[0].1,
            Some(NodeId::from("test-ingester-2"))
        );

        assert_eq!(leader_follower_pairs[1].0, "test-ingester-2");
        assert_eq!(
            leader_follower_pairs[1].1,
            Some(NodeId::from("test-ingester-1"))
        );

        assert_eq!(leader_follower_pairs[2].0, "test-ingester-1");
        assert_eq!(
            leader_follower_pairs[2].1,
            Some(NodeId::from("test-ingester-2"))
        );

        let index_uid = IndexUid::for_test("test-index", 0);
        let source_id: SourceId = "test-source".into();
        let open_shards = vec![Shard {
            index_uid: Some(index_uid.clone()),
            source_id: source_id.clone(),
            shard_id: Some(ShardId::from(1)),
            shard_state: ShardState::Open as i32,
            leader_id: "test-ingester-1".to_string(),
            ..Default::default()
        }];
        model.insert_shards(&index_uid, &source_id, open_shards);

        let leader_follower_pairs = ingest_controller
            .allocate_shards(3, &FnvHashSet::default(), &model)
            .unwrap();
        assert_eq!(leader_follower_pairs.len(), 3);
        assert_eq!(leader_follower_pairs[0].0, "test-ingester-1");
        assert_eq!(
            leader_follower_pairs[0].1,
            Some(NodeId::from("test-ingester-2"))
        );

        assert_eq!(leader_follower_pairs[1].0, "test-ingester-2");
        assert_eq!(
            leader_follower_pairs[1].1,
            Some(NodeId::from("test-ingester-1"))
        );

        assert_eq!(leader_follower_pairs[2].0, "test-ingester-2");
        assert_eq!(
            leader_follower_pairs[2].1,
            Some(NodeId::from("test-ingester-1"))
        );

        let open_shards = vec![
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(2)),
                shard_state: ShardState::Open as i32,
                leader_id: "test-ingester-1".to_string(),
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(3)),
                shard_state: ShardState::Open as i32,
                leader_id: "test-ingester-1".to_string(),
                ..Default::default()
            },
        ];
        model.insert_shards(&index_uid, &source_id, open_shards);

        let leader_follower_pairs = ingest_controller
            .allocate_shards(1, &FnvHashSet::default(), &model)
            .unwrap();
        assert_eq!(leader_follower_pairs.len(), 1);
        assert_eq!(leader_follower_pairs[0].0, "test-ingester-2");
        assert_eq!(
            leader_follower_pairs[0].1,
            Some(NodeId::from("test-ingester-1"))
        );

        ingester_pool.insert(
            "test-ingester-3".into(),
            IngesterServiceClient::from_mock(MockIngesterService::new()),
        );
        let unavailable_leaders = FnvHashSet::from_iter([NodeId::from("test-ingester-2")]);
        let leader_follower_pairs = ingest_controller
            .allocate_shards(4, &unavailable_leaders, &model)
            .unwrap();
        assert_eq!(leader_follower_pairs.len(), 4);
        assert_eq!(leader_follower_pairs[0].0, "test-ingester-3");
        assert_eq!(
            leader_follower_pairs[0].1,
            Some(NodeId::from("test-ingester-1"))
        );

        assert_eq!(leader_follower_pairs[1].0, "test-ingester-3");
        assert_eq!(
            leader_follower_pairs[1].1,
            Some(NodeId::from("test-ingester-1"))
        );

        assert_eq!(leader_follower_pairs[2].0, "test-ingester-3");
        assert_eq!(
            leader_follower_pairs[2].1,
            Some(NodeId::from("test-ingester-1"))
        );

        assert_eq!(leader_follower_pairs[3].0, "test-ingester-1");
        assert_eq!(
            leader_follower_pairs[3].1,
            Some(NodeId::from("test-ingester-3"))
        );
    }

    #[tokio::test]
    async fn test_ingest_controller_init_shards() {
        let metastore = MetastoreServiceClient::mocked();
        let ingester_pool = IngesterPool::default();
        let replication_factor = 1;

        let ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let ingester_id_0 = NodeId::from("test-ingester-0");
        let mut mock_ingester_0 = MockIngesterService::new();
        mock_ingester_0
            .expect_init_shards()
            .once()
            .returning(|mut request| {
                assert_eq!(request.subrequests.len(), 2);

                request
                    .subrequests
                    .sort_by_key(|subrequest| subrequest.subrequest_id);

                let subrequest_0 = &request.subrequests[0];
                assert_eq!(subrequest_0.subrequest_id, 0);

                let shard_0 = request.subrequests[0].shard();
                assert_eq!(shard_0.index_uid(), &("test-index", 0));
                assert_eq!(shard_0.source_id, "test-source");
                assert_eq!(shard_0.shard_id(), ShardId::from(0));
                assert_eq!(shard_0.leader_id, "test-ingester-0");

                let subrequest_1 = &request.subrequests[1];
                assert_eq!(subrequest_1.subrequest_id, 1);

                let shard_1 = request.subrequests[1].shard();
                assert_eq!(shard_1.index_uid(), &("test-index", 0));
                assert_eq!(shard_1.source_id, "test-source");
                assert_eq!(shard_1.shard_id(), ShardId::from(1));
                assert_eq!(shard_1.leader_id, "test-ingester-0");

                let successes = vec![InitShardSuccess {
                    subrequest_id: 0,
                    shard: Some(shard_0.clone()),
                }];
                let failures = vec![InitShardFailure {
                    subrequest_id: 1,
                    index_uid: shard_1.index_uid.clone(),
                    source_id: shard_1.source_id.clone(),
                    shard_id: shard_1.shard_id.clone(),
                }];
                let response = InitShardsResponse {
                    successes,
                    failures,
                };
                Ok(response)
            });
        let ingester_0 = IngesterServiceClient::from_mock(mock_ingester_0);
        ingester_pool.insert(ingester_id_0, ingester_0);

        let ingester_id_1 = NodeId::from("test-ingester-1");
        let mut mock_ingester_1 = MockIngesterService::new();
        mock_ingester_1
            .expect_init_shards()
            .once()
            .returning(|request| {
                assert_eq!(request.subrequests.len(), 1);

                let subrequest = &request.subrequests[0];
                assert_eq!(subrequest.subrequest_id, 2);

                let shard = request.subrequests[0].shard();
                assert_eq!(shard.index_uid(), &("test-index", 0));
                assert_eq!(shard.source_id, "test-source");
                assert_eq!(shard.shard_id(), ShardId::from(2));
                assert_eq!(shard.leader_id, "test-ingester-1");

                Err(IngestV2Error::Internal("internal error".to_string()))
            });
        let ingester_1 = IngesterServiceClient::from_mock(mock_ingester_1);
        ingester_pool.insert(ingester_id_1, ingester_1);

        let ingester_id_2 = NodeId::from("test-ingester-2");
        let mut mock_ingester_2 = MockIngesterService::new();
        mock_ingester_2.expect_init_shards().never();

        let ingester_2 = IngesterServiceClient::tower()
            .stack_init_shards_layer(DelayLayer::new(INIT_SHARDS_REQUEST_TIMEOUT * 2))
            .build_from_mock(mock_ingester_2);
        ingester_pool.insert(ingester_id_2, ingester_2);

        let init_shards_response = ingest_controller
            .init_shards(&[], &Progress::default())
            .await;
        assert_eq!(init_shards_response.successes.len(), 0);
        assert_eq!(init_shards_response.failures.len(), 0);

        // In this test:
        // - ingester 0 will initialize shard 0 successfully and fail to initialize shard 1;
        // - ingester 1 will return an error;
        // - ingester 2 will time out;
        // - ingester 3 will be unavailable.

        let open_shards_subresponses = [
            metastore::OpenShardSubresponse {
                subrequest_id: 0,
                open_shard: Some(Shard {
                    index_uid: IndexUid::for_test("test-index", 0).into(),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(0)),
                    leader_id: "test-ingester-0".to_string(),
                    shard_state: ShardState::Open as i32,
                    ..Default::default()
                }),
            },
            metastore::OpenShardSubresponse {
                subrequest_id: 1,
                open_shard: Some(Shard {
                    index_uid: IndexUid::for_test("test-index", 0).into(),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(1)),
                    leader_id: "test-ingester-0".to_string(),
                    shard_state: ShardState::Open as i32,
                    ..Default::default()
                }),
            },
            metastore::OpenShardSubresponse {
                subrequest_id: 2,
                open_shard: Some(Shard {
                    index_uid: IndexUid::for_test("test-index", 0).into(),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(2)),
                    leader_id: "test-ingester-1".to_string(),
                    shard_state: ShardState::Open as i32,
                    ..Default::default()
                }),
            },
            metastore::OpenShardSubresponse {
                subrequest_id: 3,
                open_shard: Some(Shard {
                    index_uid: IndexUid::for_test("test-index", 0).into(),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(3)),
                    leader_id: "test-ingester-2".to_string(),
                    shard_state: ShardState::Open as i32,
                    ..Default::default()
                }),
            },
            metastore::OpenShardSubresponse {
                subrequest_id: 4,
                open_shard: Some(Shard {
                    index_uid: IndexUid::for_test("test-index", 0).into(),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(4)),
                    leader_id: "test-ingester-3".to_string(),
                    shard_state: ShardState::Open as i32,
                    ..Default::default()
                }),
            },
        ];
        let init_shards_response = ingest_controller
            .init_shards(&open_shards_subresponses, &Progress::default())
            .await;
        assert_eq!(init_shards_response.successes.len(), 1);
        assert_eq!(init_shards_response.failures.len(), 4);

        let success = &init_shards_response.successes[0];
        assert_eq!(success.subrequest_id, 0);

        let mut failures = init_shards_response.failures;
        failures.sort_by_key(|failure| failure.subrequest_id);

        assert_eq!(failures[0].subrequest_id, 1);
        assert_eq!(failures[1].subrequest_id, 2);
        assert_eq!(failures[2].subrequest_id, 3);
        assert_eq!(failures[3].subrequest_id, 4);
    }

    #[tokio::test]
    async fn test_ingest_controller_handle_local_shards_update() {
        let mut mock_metastore = MockMetastoreService::new();
        mock_metastore
            .expect_open_shards()
            .once()
            .returning(|request| {
                assert_eq!(request.subrequests.len(), 1);
                let subrequest = &request.subrequests[0];

                assert_eq!(subrequest.index_uid(), &IndexUid::for_test("test-index", 0));
                assert_eq!(subrequest.source_id, "test-source");
                assert_eq!(subrequest.leader_id, "test-ingester");

                Err(MetastoreError::InvalidArgument {
                    message: "failed to open shards".to_string(),
                })
            });
        let metastore = MetastoreServiceClient::from_mock(mock_metastore);
        let ingester_pool = IngesterPool::default();
        let replication_factor = 1;

        let mut ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let index_uid = IndexUid::for_test("test-index", 0);
        let source_id: SourceId = "test-source".into();

        let source_uid = SourceUid {
            index_uid: index_uid.clone(),
            source_id: source_id.clone(),
        };
        let mut model = ControlPlaneModel::default();
        let progress = Progress::default();

        let shards = vec![Shard {
            index_uid: Some(index_uid.clone()),
            source_id: source_id.clone(),
            shard_id: Some(ShardId::from(1)),
            leader_id: "test-ingester".to_string(),
            shard_state: ShardState::Open as i32,
            ..Default::default()
        }];
        model.insert_shards(&index_uid, &source_id, shards);
        let shard_entries: Vec<ShardEntry> = model.all_shards().cloned().collect();

        assert_eq!(shard_entries.len(), 1);
        assert_eq!(shard_entries[0].ingestion_rate, 0);

        // Test update shard ingestion rate but no scale down because num open shards is 1.
        let shard_infos = BTreeSet::from_iter([ShardInfo {
            shard_id: ShardId::from(1),
            shard_state: ShardState::Open,
            ingestion_rate: RateMibPerSec(1),
        }]);
        let local_shards_update = LocalShardsUpdate {
            leader_id: "test-ingester".into(),
            source_uid: source_uid.clone(),
            shard_infos,
        };
        ingest_controller
            .handle_local_shards_update(local_shards_update, &mut model, &progress)
            .await;

        let shard_entries: Vec<ShardEntry> = model.all_shards().cloned().collect();
        assert_eq!(shard_entries.len(), 1);
        assert_eq!(shard_entries[0].ingestion_rate, 1);

        // Test update shard ingestion rate with failing scale down.
        let shards = vec![Shard {
            index_uid: Some(index_uid.clone()),
            source_id: source_id.clone(),
            shard_id: Some(ShardId::from(2)),
            shard_state: ShardState::Open as i32,
            leader_id: "test-ingester".to_string(),
            ..Default::default()
        }];
        model.insert_shards(&index_uid, &source_id, shards);

        let shard_entries: Vec<ShardEntry> = model.all_shards().cloned().collect();
        assert_eq!(shard_entries.len(), 2);

        let mut mock_ingester = MockIngesterService::new();

        let index_uid_clone = index_uid.clone();
        mock_ingester
            .expect_close_shards()
            .returning(move |request| {
                assert_eq!(request.shard_pkeys.len(), 1);
                assert_eq!(request.shard_pkeys[0].index_uid(), &index_uid_clone);
                assert_eq!(request.shard_pkeys[0].source_id, "test-source");
                assert_eq!(request.shard_pkeys[0].shard_id(), ShardId::from(2));

                Err(IngestV2Error::Internal(
                    "failed to close shards".to_string(),
                ))
            });
        let ingester = IngesterServiceClient::from_mock(mock_ingester);
        ingester_pool.insert("test-ingester".into(), ingester);

        let shard_infos = BTreeSet::from_iter([
            ShardInfo {
                shard_id: ShardId::from(1),
                shard_state: ShardState::Open,
                ingestion_rate: RateMibPerSec(1),
            },
            ShardInfo {
                shard_id: ShardId::from(2),
                shard_state: ShardState::Open,
                ingestion_rate: RateMibPerSec(1),
            },
        ]);
        let local_shards_update = LocalShardsUpdate {
            leader_id: "test-ingester".into(),
            source_uid: source_uid.clone(),
            shard_infos,
        };
        ingest_controller
            .handle_local_shards_update(local_shards_update, &mut model, &progress)
            .await;

        // Test update shard ingestion rate with failing scale up.
        let shard_infos = BTreeSet::from_iter([
            ShardInfo {
                shard_id: ShardId::from(1),
                shard_state: ShardState::Open,
                ingestion_rate: RateMibPerSec(4),
            },
            ShardInfo {
                shard_id: ShardId::from(2),
                shard_state: ShardState::Open,
                ingestion_rate: RateMibPerSec(4),
            },
        ]);
        let local_shards_update = LocalShardsUpdate {
            leader_id: "test-ingester".into(),
            source_uid: source_uid.clone(),
            shard_infos,
        };
        ingest_controller
            .handle_local_shards_update(local_shards_update, &mut model, &progress)
            .await;
    }

    #[tokio::test]
    async fn test_ingest_controller_try_scale_up_shards() {
        let mut mock_metastore = MockMetastoreService::new();

        let index_uid = IndexUid::from_str("test-index:00000000000000000000000000").unwrap();
        let index_uid_clone = index_uid.clone();
        mock_metastore
            .expect_open_shards()
            .once()
            .returning(move |request| {
                assert_eq!(request.subrequests.len(), 1);
                assert_eq!(request.subrequests[0].index_uid(), &index_uid_clone);
                assert_eq!(request.subrequests[0].source_id, INGEST_V2_SOURCE_ID);
                assert_eq!(request.subrequests[0].leader_id, "test-ingester");

                Err(MetastoreError::InvalidArgument {
                    message: "failed to open shards".to_string(),
                })
            });
        let index_uid_clone = index_uid.clone();
        mock_metastore
            .expect_open_shards()
            .returning(move |request| {
                assert_eq!(request.subrequests.len(), 1);
                assert_eq!(request.subrequests[0].index_uid(), &index_uid_clone);
                assert_eq!(request.subrequests[0].source_id, INGEST_V2_SOURCE_ID);
                assert_eq!(request.subrequests[0].leader_id, "test-ingester");

                let subresponses = vec![metastore::OpenShardSubresponse {
                    subrequest_id: 0,
                    open_shard: Some(Shard {
                        index_uid: Some(index_uid.clone()),
                        source_id: INGEST_V2_SOURCE_ID.to_string(),
                        shard_id: Some(ShardId::from(1)),
                        leader_id: "test-ingester".to_string(),
                        shard_state: ShardState::Open as i32,
                        ..Default::default()
                    }),
                }];
                let response = metastore::OpenShardsResponse { subresponses };
                Ok(response)
            });
        let metastore = MetastoreServiceClient::from_mock(mock_metastore);

        let ingester_pool = IngesterPool::default();
        let replication_factor = 1;

        let mut ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let index_uid = IndexUid::for_test("test-index", 0);
        let source_id: SourceId = INGEST_V2_SOURCE_ID.to_string();

        let source_uid = SourceUid {
            index_uid: index_uid.clone(),
            source_id: source_id.clone(),
        };
        let shard_stats = ShardStats {
            num_open_shards: 2,
            ..Default::default()
        };
        let mut model = ControlPlaneModel::default();
        let index_metadata =
            IndexMetadata::for_test(&index_uid.index_id, "ram://indexes/test-index:0");
        model.add_index(index_metadata);

        let souce_config = SourceConfig::ingest_v2();
        model.add_source(&index_uid, souce_config).unwrap();

        let progress = Progress::default();

        // Test could not find leader.
        ingest_controller
            .try_scale_up_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;

        let mut mock_ingester = MockIngesterService::new();

        let index_uid_clone = index_uid.clone();
        mock_ingester
            .expect_init_shards()
            .once()
            .returning(move |request| {
                assert_eq!(request.subrequests.len(), 1);

                let subrequest = &request.subrequests[0];
                assert_eq!(subrequest.subrequest_id, 0);

                let shard = request.subrequests[0].shard();
                assert_eq!(shard.index_uid(), &index_uid_clone);
                assert_eq!(shard.source_id, INGEST_V2_SOURCE_ID);
                assert_eq!(shard.shard_id(), ShardId::from(1));
                assert_eq!(shard.leader_id, "test-ingester");

                Err(IngestV2Error::Internal("failed to init shards".to_string()))
            });
        let index_uid_clone = index_uid.clone();
        mock_ingester
            .expect_init_shards()
            .returning(move |request| {
                assert_eq!(request.subrequests.len(), 1);

                let subrequest = &request.subrequests[0];
                assert_eq!(subrequest.subrequest_id, 0);

                let shard = subrequest.shard();
                assert_eq!(shard.index_uid(), &index_uid_clone);
                assert_eq!(shard.source_id, INGEST_V2_SOURCE_ID);
                assert_eq!(shard.shard_id(), ShardId::from(1));
                assert_eq!(shard.leader_id, "test-ingester");

                let successes = vec![InitShardSuccess {
                    subrequest_id: request.subrequests[0].subrequest_id,
                    shard: Some(shard.clone()),
                }];
                let response = InitShardsResponse {
                    successes,
                    failures: Vec::new(),
                };
                Ok(response)
            });
        let ingester = IngesterServiceClient::from_mock(mock_ingester);
        ingester_pool.insert("test-ingester".into(), ingester);

        // Test failed to open shards.
        ingest_controller
            .try_scale_up_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;
        assert_eq!(model.all_shards().count(), 0);

        // Test failed to init shards.
        ingest_controller
            .try_scale_up_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;
        assert_eq!(model.all_shards().count(), 0);

        // Test successfully opened shard.
        ingest_controller
            .try_scale_up_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;
        assert_eq!(
            model.all_shards().filter(|shard| shard.is_open()).count(),
            1
        );
    }

    #[tokio::test]
    async fn test_ingest_controller_try_scale_down_shards() {
        let metastore = MetastoreServiceClient::mocked();
        let ingester_pool = IngesterPool::default();
        let replication_factor = 1;

        let ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let index_uid = IndexUid::for_test("test-index", 0);
        let source_id: SourceId = "test-source".into();

        let source_uid = SourceUid {
            index_uid: index_uid.clone(),
            source_id: source_id.clone(),
        };
        let shard_stats = ShardStats {
            num_open_shards: 2,
            ..Default::default()
        };
        let mut model = ControlPlaneModel::default();
        let progress = Progress::default();

        // Test could not find a scale down candidate.
        ingest_controller
            .try_scale_down_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;

        let shards = vec![Shard {
            shard_id: Some(ShardId::from(1)),
            index_uid: Some(index_uid.clone()),
            source_id: source_id.clone(),
            leader_id: "test-ingester".to_string(),
            shard_state: ShardState::Open as i32,
            ..Default::default()
        }];
        model.insert_shards(&index_uid, &source_id, shards);

        // Test ingester is unavailable.
        ingest_controller
            .try_scale_down_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;

        let mut mock_ingester = MockIngesterService::new();

        let index_uid_clone = index_uid.clone();
        mock_ingester
            .expect_close_shards()
            .once()
            .returning(move |request| {
                assert_eq!(request.shard_pkeys.len(), 1);
                assert_eq!(request.shard_pkeys[0].index_uid(), &index_uid_clone);
                assert_eq!(request.shard_pkeys[0].source_id, "test-source");
                assert_eq!(request.shard_pkeys[0].shard_id(), ShardId::from(1));

                Err(IngestV2Error::Internal(
                    "failed to close shards".to_string(),
                ))
            });
        let index_uid_clone = index_uid.clone();
        mock_ingester
            .expect_close_shards()
            .once()
            .returning(move |request| {
                assert_eq!(request.shard_pkeys.len(), 1);
                assert_eq!(request.shard_pkeys[0].index_uid(), &index_uid_clone);
                assert_eq!(request.shard_pkeys[0].source_id, "test-source");
                assert_eq!(request.shard_pkeys[0].shard_id(), ShardId::from(1));

                let response = CloseShardsResponse {
                    successes: request.shard_pkeys,
                };
                Ok(response)
            });
        let ingester = IngesterServiceClient::from_mock(mock_ingester);
        ingester_pool.insert("test-ingester".into(), ingester);

        // Test failed to close shard.
        ingest_controller
            .try_scale_down_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;
        assert!(model.all_shards().all(|shard| shard.is_open()));

        // Test successfully closed shard.
        ingest_controller
            .try_scale_down_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;
        assert!(model.all_shards().all(|shard| shard.is_closed()));

        let shards = vec![Shard {
            shard_id: Some(ShardId::from(2)),
            index_uid: Some(index_uid.clone()),
            source_id: source_id.clone(),
            leader_id: "test-ingester".to_string(),
            shard_state: ShardState::Open as i32,
            ..Default::default()
        }];
        model.insert_shards(&index_uid, &source_id, shards);

        // Test rate limited.
        ingest_controller
            .try_scale_down_shards(source_uid.clone(), shard_stats, &mut model, &progress)
            .await;
        assert!(model.all_shards().any(|shard| shard.is_open()));
    }

    #[test]
    fn test_find_scale_down_candidate() {
        let index_uid = IndexUid::for_test("test-index", 0);
        let source_id: SourceId = "test-source".into();

        let source_uid = SourceUid {
            index_uid: index_uid.clone(),
            source_id: source_id.clone(),
        };
        let mut model = ControlPlaneModel::default();

        assert!(find_scale_down_candidate(&source_uid, &model).is_none());

        let shards = vec![
            Shard {
                index_uid: index_uid.clone().into(),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(1)),
                shard_state: ShardState::Open as i32,
                leader_id: "test-ingester-0".to_string(),
                ..Default::default()
            },
            Shard {
                index_uid: index_uid.clone().into(),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(2)),
                shard_state: ShardState::Open as i32,
                leader_id: "test-ingester-0".to_string(),
                ..Default::default()
            },
            Shard {
                index_uid: index_uid.clone().into(),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(3)),
                shard_state: ShardState::Closed as i32,
                leader_id: "test-ingester-0".to_string(),
                ..Default::default()
            },
            Shard {
                index_uid: index_uid.clone().into(),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(4)),
                shard_state: ShardState::Open as i32,
                leader_id: "test-ingester-1".to_string(),
                ..Default::default()
            },
            Shard {
                index_uid: index_uid.clone().into(),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(5)),
                shard_state: ShardState::Open as i32,
                leader_id: "test-ingester-1".to_string(),
                ..Default::default()
            },
            Shard {
                index_uid: index_uid.clone().into(),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(6)),
                shard_state: ShardState::Open as i32,
                leader_id: "test-ingester-1".to_string(),
                ..Default::default()
            },
        ];
        model.insert_shards(&index_uid, &source_id, shards);

        let shard_infos = BTreeSet::from_iter([
            ShardInfo {
                shard_id: ShardId::from(1),
                shard_state: ShardState::Open,
                ingestion_rate: quickwit_ingest::RateMibPerSec(1),
            },
            ShardInfo {
                shard_id: ShardId::from(2),
                shard_state: ShardState::Open,
                ingestion_rate: quickwit_ingest::RateMibPerSec(2),
            },
            ShardInfo {
                shard_id: ShardId::from(3),
                shard_state: ShardState::Open,
                ingestion_rate: quickwit_ingest::RateMibPerSec(3),
            },
            ShardInfo {
                shard_id: ShardId::from(4),
                shard_state: ShardState::Open,
                ingestion_rate: quickwit_ingest::RateMibPerSec(4),
            },
            ShardInfo {
                shard_id: ShardId::from(5),
                shard_state: ShardState::Open,
                ingestion_rate: quickwit_ingest::RateMibPerSec(5),
            },
            ShardInfo {
                shard_id: ShardId::from(6),
                shard_state: ShardState::Open,
                ingestion_rate: quickwit_ingest::RateMibPerSec(6),
            },
        ]);
        model.update_shards(&source_uid, &shard_infos);

        let (leader_id, shard_id) = find_scale_down_candidate(&source_uid, &model).unwrap();
        assert_eq!(leader_id, "test-ingester-0");
        assert_eq!(shard_id, ShardId::from(2));
    }

    #[tokio::test]
    async fn test_sync_with_ingesters() {
        let metastore = MetastoreServiceClient::mocked();
        let ingester_pool = IngesterPool::default();
        let replication_factor = 2;

        let ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let index_uid = IndexUid::for_test("test-index", 0);
        let source_id: SourceId = "test-source".into();
        let mut model = ControlPlaneModel::default();
        let shards = vec![
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(1)),
                shard_state: ShardState::Open as i32,
                leader_id: "node-1".to_string(),
                follower_id: Some("node-2".to_string()),
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(2)),
                shard_state: ShardState::Open as i32,
                leader_id: "node-2".to_string(),
                follower_id: Some("node-3".to_string()),
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: source_id.clone(),
                shard_id: Some(ShardId::from(3)),
                shard_state: ShardState::Open as i32,
                leader_id: "node-2".to_string(),
                follower_id: Some("node-1".to_string()),
                ..Default::default()
            },
        ];
        model.insert_shards(&index_uid, &source_id, shards);

        let mut mock_ingester_1 = MockIngesterService::new();
        let mock_ingester_2 = MockIngesterService::new();
        let mock_ingester_3 = MockIngesterService::new();

        let count_calls = Arc::new(AtomicUsize::new(0));
        let count_calls_clone = count_calls.clone();
        mock_ingester_1
            .expect_retain_shards()
            .once()
            .returning(move |request| {
                assert_eq!(request.retain_shards_for_sources.len(), 1);
                assert_eq!(
                    request.retain_shards_for_sources[0].shard_ids,
                    [ShardId::from(1), ShardId::from(3)]
                );
                count_calls_clone.fetch_add(1, Ordering::Release);
                Ok(RetainShardsResponse {})
            });
        ingester_pool.insert(
            "node-1".into(),
            IngesterServiceClient::from_mock(mock_ingester_1),
        );
        ingester_pool.insert(
            "node-2".into(),
            IngesterServiceClient::from_mock(mock_ingester_2),
        );
        ingester_pool.insert(
            "node-3".into(),
            IngesterServiceClient::from_mock(mock_ingester_3),
        );
        let node_id = "node-1".into();
        let wait_handle = ingest_controller.sync_with_ingester(&node_id, &model);
        wait_handle.wait().await;
        assert_eq!(count_calls.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn test_ingest_controller_advise_reset_shards() {
        let metastore = MetastoreServiceClient::mocked();
        let ingester_pool = IngesterPool::default();
        let replication_factor = 2;

        let ingest_controller = IngestController::new(metastore, ingester_pool, replication_factor);

        let mut model = ControlPlaneModel::default();

        let index_uid = IndexUid::for_test("test-index", 0);
        let source_id_00: SourceId = "test-source-0".into();
        let source_id_01: SourceId = "test-source-1".into();

        let shards = vec![Shard {
            index_uid: Some(index_uid.clone()),
            source_id: source_id_00.clone(),
            shard_id: Some(ShardId::from(1)),
            shard_state: ShardState::Open as i32,
            publish_position_inclusive: Some(Position::offset(1337u64)),
            ..Default::default()
        }];
        model.insert_shards(&index_uid, &source_id_00, shards);

        let advise_reset_shards_request = AdviseResetShardsRequest {
            shard_ids: vec![
                ShardIds {
                    index_uid: Some(index_uid.clone()),
                    source_id: source_id_00.clone(),
                    shard_ids: vec![ShardId::from(1), ShardId::from(2)],
                },
                ShardIds {
                    index_uid: Some(index_uid.clone()),
                    source_id: source_id_01.clone(),
                    shard_ids: vec![ShardId::from(3)],
                },
            ],
        };
        let advise_reset_shards_response =
            ingest_controller.advise_reset_shards(advise_reset_shards_request, &model);

        assert_eq!(advise_reset_shards_response.shards_to_delete.len(), 2);

        let shard_to_delete_00 = &advise_reset_shards_response.shards_to_delete[0];
        assert_eq!(shard_to_delete_00.index_uid(), &index_uid);
        assert_eq!(shard_to_delete_00.source_id, source_id_00);
        assert_eq!(shard_to_delete_00.shard_ids.len(), 1);
        assert_eq!(shard_to_delete_00.shard_ids[0], ShardId::from(2));

        let shard_to_delete_01 = &advise_reset_shards_response.shards_to_delete[1];
        assert_eq!(shard_to_delete_01.index_uid(), &index_uid);
        assert_eq!(shard_to_delete_01.source_id, source_id_01);
        assert_eq!(shard_to_delete_01.shard_ids.len(), 1);
        assert_eq!(shard_to_delete_01.shard_ids[0], ShardId::from(3));

        assert_eq!(advise_reset_shards_response.shards_to_truncate.len(), 1);

        let shard_to_truncate = &advise_reset_shards_response.shards_to_truncate[0];
        assert_eq!(shard_to_truncate.index_uid(), &index_uid);
        assert_eq!(shard_to_truncate.source_id, source_id_00);
        assert_eq!(shard_to_truncate.shard_positions.len(), 1);
        assert_eq!(
            shard_to_truncate.shard_positions[0].shard_id(),
            ShardId::from(1)
        );
        assert_eq!(
            shard_to_truncate.shard_positions[0].publish_position_inclusive(),
            Position::offset(1337u64)
        );
    }

    #[tokio::test]
    async fn test_ingest_controller_close_shards() {
        let metastore = MetastoreServiceClient::mocked();
        let ingester_pool = IngesterPool::default();
        let replication_factor = 1;
        let ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let closed_shards = ingest_controller.close_shards(empty()).await;
        assert_eq!(closed_shards.len(), 0);

        let ingester_id_0 = NodeId::from("test-ingester-0");
        let mut mock_ingester_0 = MockIngesterService::new();
        mock_ingester_0
            .expect_close_shards()
            .once()
            .returning(|mut request| {
                assert_eq!(request.shard_pkeys.len(), 2);

                request
                    .shard_pkeys
                    .sort_by(|left, right| left.shard_id().cmp(right.shard_id()));

                let shard_0 = &request.shard_pkeys[0];
                assert_eq!(shard_0.index_uid(), &IndexUid::for_test("test-index", 0));
                assert_eq!(shard_0.source_id, "test-source");
                assert_eq!(shard_0.shard_id(), ShardId::from(0));

                let shard_1 = &request.shard_pkeys[1];
                assert_eq!(shard_1.index_uid(), &IndexUid::for_test("test-index", 0));
                assert_eq!(shard_1.source_id, "test-source");
                assert_eq!(shard_1.shard_id(), ShardId::from(1));

                let response = CloseShardsResponse {
                    successes: vec![shard_0.clone()],
                };
                Ok(response)
            });
        let ingester_0 = IngesterServiceClient::from_mock(mock_ingester_0);
        ingester_pool.insert(ingester_id_0.clone(), ingester_0);

        let ingester_id_1 = NodeId::from("test-ingester-1");
        let mut mock_ingester_1 = MockIngesterService::new();
        mock_ingester_1
            .expect_close_shards()
            .once()
            .returning(|request| {
                assert_eq!(request.shard_pkeys.len(), 1);

                let shard = &request.shard_pkeys[0];
                assert_eq!(shard.index_uid(), &IndexUid::for_test("test-index", 0));
                assert_eq!(shard.source_id, "test-source");
                assert_eq!(shard.shard_id(), ShardId::from(2));

                Err(IngestV2Error::Internal("internal error".to_string()))
            });
        let ingester_1 = IngesterServiceClient::from_mock(mock_ingester_1);
        ingester_pool.insert(ingester_id_1.clone(), ingester_1);

        let ingester_id_2 = NodeId::from("test-ingester-2");
        let mut mock_ingester_2 = MockIngesterService::new();
        mock_ingester_2.expect_close_shards().never();

        let ingester_2 = IngesterServiceClient::tower()
            .stack_close_shards_layer(DelayLayer::new(CLOSE_SHARDS_REQUEST_TIMEOUT * 2))
            .build_from_mock(mock_ingester_2);
        ingester_pool.insert(ingester_id_2.clone(), ingester_2);

        // In this test:
        // - ingester 0 will close shard 0 successfully and fail to close shard 1;
        // - ingester 1 will return an error;
        // - ingester 2 will time out;
        // - ingester 3 will be unavailable.

        let shards_to_close = vec![
            (
                ingester_id_0.clone(),
                ShardPKey {
                    index_uid: Some(IndexUid::for_test("test-index", 0)),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(0)),
                },
            ),
            (
                ingester_id_0,
                ShardPKey {
                    index_uid: Some(IndexUid::for_test("test-index", 0)),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(1)),
                },
            ),
            (
                ingester_id_1,
                ShardPKey {
                    index_uid: Some(IndexUid::for_test("test-index", 0)),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(2)),
                },
            ),
            (
                ingester_id_2,
                ShardPKey {
                    index_uid: Some(IndexUid::for_test("test-index", 0)),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(3)),
                },
            ),
            (
                NodeId::from("test-ingester-3"),
                ShardPKey {
                    index_uid: Some(IndexUid::for_test("test-index", 0)),
                    source_id: "test-source".to_string(),
                    shard_id: Some(ShardId::from(4)),
                },
            ),
        ];
        let closed_shards = ingest_controller
            .close_shards(shards_to_close.into_iter())
            .await;
        assert_eq!(closed_shards.len(), 1);

        let closed_shard = &closed_shards[0];
        assert_eq!(closed_shard.index_uid(), &("test-index", 0));
        assert_eq!(closed_shard.source_id, "test-source");
        assert_eq!(closed_shard.shard_id(), ShardId::from(0));
    }

    #[tokio::test]
    async fn test_ingest_controller_rebalance_shards() {
        setup_logging_for_tests();

        let mut mock_metastore = MockMetastoreService::new();
        mock_metastore.expect_open_shards().return_once(|request| {
            assert_eq!(request.subrequests.len(), 2);

            let subrequest_0 = &request.subrequests[0];
            assert_eq!(subrequest_0.subrequest_id, 0);
            assert_eq!(subrequest_0.index_uid(), &("test-index", 0));
            assert_eq!(subrequest_0.source_id, INGEST_V2_SOURCE_ID.to_string());
            assert_eq!(subrequest_0.leader_id, "test-ingester-1");
            assert!(subrequest_0.follower_id.is_none());

            let subrequest_1 = &request.subrequests[1];
            assert_eq!(subrequest_1.subrequest_id, 1);
            assert_eq!(subrequest_1.index_uid(), &("test-index", 0));
            assert_eq!(subrequest_1.source_id, INGEST_V2_SOURCE_ID.to_string());
            assert_eq!(subrequest_1.leader_id, "test-ingester-1");
            assert!(subrequest_1.follower_id.is_none());

            let subresponses = vec![
                metastore::OpenShardSubresponse {
                    subrequest_id: 0,
                    open_shard: Some(Shard {
                        index_uid: Some(IndexUid::for_test("test-index", 0)),
                        source_id: INGEST_V2_SOURCE_ID.to_string(),
                        shard_id: subrequest_0.shard_id.clone(),
                        leader_id: "test-ingester-1".to_string(),
                        shard_state: ShardState::Open as i32,
                        ..Default::default()
                    }),
                },
                metastore::OpenShardSubresponse {
                    subrequest_id: 1,
                    open_shard: Some(Shard {
                        index_uid: Some(IndexUid::for_test("test-index", 0)),
                        source_id: INGEST_V2_SOURCE_ID.to_string(),
                        shard_id: subrequest_1.shard_id.clone(),
                        leader_id: "test-ingester-1".to_string(),
                        shard_state: ShardState::Open as i32,
                        ..Default::default()
                    }),
                },
            ];
            let response = metastore::OpenShardsResponse { subresponses };
            Ok(response)
        });
        let metastore = MetastoreServiceClient::from_mock(mock_metastore);
        let ingester_pool = IngesterPool::default();
        let replication_factor = 1;
        let mut ingest_controller =
            IngestController::new(metastore, ingester_pool.clone(), replication_factor);

        let mut model = ControlPlaneModel::default();

        let universe = Universe::with_accelerated_time();
        let (control_plane_mailbox, control_plane_inbox) = universe.create_test_mailbox();
        let progress = Progress::default();

        let close_shards_task_opt = ingest_controller
            .rebalance_shards(&mut model, &control_plane_mailbox, &progress)
            .await;
        assert!(close_shards_task_opt.is_none());

        let index_metadata = IndexMetadata::for_test("test-index", "ram://indexes/test-index");
        let index_uid = index_metadata.index_uid.clone();
        model.add_index(index_metadata);

        let source_config = SourceConfig::ingest_v2();
        model.add_source(&index_uid, source_config).unwrap();

        // In this test, ingester 0 hosts 5 shards but there are two ingesters in the cluster.
        // `rebalance_shards` will attempt to move 2 shards to ingester 1. However, it will fail to
        // init one shard, so only one shard will be actually moved.

        let open_shards = vec![
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: INGEST_V2_SOURCE_ID.to_string(),
                shard_id: Some(ShardId::from(0)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: INGEST_V2_SOURCE_ID.to_string(),
                shard_id: Some(ShardId::from(1)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: INGEST_V2_SOURCE_ID.to_string(),
                shard_id: Some(ShardId::from(2)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: INGEST_V2_SOURCE_ID.to_string(),
                shard_id: Some(ShardId::from(3)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
            Shard {
                index_uid: Some(index_uid.clone()),
                source_id: INGEST_V2_SOURCE_ID.to_string(),
                shard_id: Some(ShardId::from(4)),
                leader_id: "test-ingester-0".to_string(),
                shard_state: ShardState::Open as i32,
                ..Default::default()
            },
        ];
        model.insert_shards(&index_uid, &INGEST_V2_SOURCE_ID.to_string(), open_shards);

        let ingester_id_0 = NodeId::from("test-ingester-0");
        let mut mock_ingester_0 = MockIngesterService::new();
        mock_ingester_0
            .expect_close_shards()
            .once()
            .returning(|request| {
                assert_eq!(request.shard_pkeys.len(), 1);

                let shard = &request.shard_pkeys[0];
                assert_eq!(shard.index_uid(), &("test-index", 0));
                assert_eq!(shard.source_id, INGEST_V2_SOURCE_ID);
                // assert_eq!(shard.shard_id(), ShardId::from(2));

                let response = CloseShardsResponse {
                    successes: vec![shard.clone()],
                };
                Ok(response)
            });
        let ingester_0 = IngesterServiceClient::from_mock(mock_ingester_0);
        ingester_pool.insert(ingester_id_0.clone(), ingester_0);

        let ingester_id_1 = NodeId::from("test-ingester-1");
        let mut mock_ingester_1 = MockIngesterService::new();
        mock_ingester_1.expect_init_shards().return_once(|request| {
            assert_eq!(request.subrequests.len(), 2);

            let subrequest_0 = &request.subrequests[0];
            assert_eq!(subrequest_0.subrequest_id, 0);

            let shard_0 = request.subrequests[0].shard();
            assert_eq!(shard_0.index_uid(), &("test-index", 0));
            assert_eq!(shard_0.source_id, INGEST_V2_SOURCE_ID.to_string());
            assert_eq!(shard_0.leader_id, "test-ingester-1");
            assert!(shard_0.follower_id.is_none());

            let subrequest_1 = &request.subrequests[1];
            assert_eq!(subrequest_1.subrequest_id, 1);

            let shard_1 = request.subrequests[0].shard();
            assert_eq!(shard_1.index_uid(), &("test-index", 0));
            assert_eq!(shard_1.source_id, INGEST_V2_SOURCE_ID.to_string());
            assert_eq!(shard_1.leader_id, "test-ingester-1");
            assert!(shard_1.follower_id.is_none());

            let successes = vec![InitShardSuccess {
                subrequest_id: request.subrequests[0].subrequest_id,
                shard: Some(shard_0.clone()),
            }];
            let failures = vec![InitShardFailure {
                subrequest_id: request.subrequests[1].subrequest_id,
                index_uid: Some(IndexUid::for_test("test-index", 0)),
                source_id: INGEST_V2_SOURCE_ID.to_string(),
                shard_id: Some(shard_1.shard_id().clone()),
            }];
            let response = InitShardsResponse {
                successes,
                failures,
            };
            Ok(response)
        });
        let ingester_1 = IngesterServiceClient::from_mock(mock_ingester_1);
        ingester_pool.insert(ingester_id_1.clone(), ingester_1);

        let close_shards_task = ingest_controller
            .rebalance_shards(&mut model, &control_plane_mailbox, &progress)
            .await
            .unwrap();

        tokio::time::timeout(CLOSE_SHARDS_REQUEST_TIMEOUT * 2, close_shards_task)
            .await
            .unwrap()
            .unwrap();

        let callbacks: Vec<RebalanceShardsCallback> = control_plane_inbox.drain_for_test_typed();
        assert_eq!(callbacks.len(), 1);

        let callback = &callbacks[0];
        assert_eq!(callback.closed_shards.len(), 1);
    }
}
