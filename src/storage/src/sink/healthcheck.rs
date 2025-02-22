// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Healthchecks for sinks
use std::fmt::Display;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use differential_dataflow::lattice::Lattice;
use timely::progress::Antichain;
use tokio::sync::Mutex;
use tracing::trace;

use mz_ore::halt;
use mz_ore::now::NowFn;
use mz_persist_client::cache::PersistClientCache;
use mz_persist_client::write::WriteHandle;
use mz_persist_client::{PersistLocation, ShardId, Upper};
use mz_repr::{Datum, GlobalId, Row, Timestamp};
use mz_storage_client::types::sources::SourceData;

/// The Healthchecker is responsible for tracking the current state
/// of a Timely worker for a source, as well as updating the relevant
/// state collection based on it.
#[derive(Debug)]
pub struct Healthchecker {
    /// Internal ID of the source (e.g. s1)
    sink_id: GlobalId,
    /// Current status of this source
    current_status: Option<SinkStatus>,
    /// Last observed upper
    upper: Antichain<Timestamp>,
    /// Write handle of the Healthchecker persist shard
    ///
    /// The schema used matches the one used in regular sources and tables.
    write_handle: WriteHandle<SourceData, (), Timestamp, i64>,
    /// The function that should be used to get the current time when updating upper
    now: NowFn,
}

impl Healthchecker {
    /// Create healthchecker for sink, recorded on `status_shard_id` at `persist_location`.
    ///
    /// This function initializes the Healthchecker in the `SinkStatus::Setup` state without writing to persistent
    /// storage.
    pub async fn new(
        sink_id: GlobalId,
        persist_clients: &Arc<Mutex<PersistClientCache>>,
        persist_location: PersistLocation,
        status_shard_id: ShardId,
        now: NowFn,
    ) -> anyhow::Result<Self> {
        trace!("Initializing healthchecker for sink {sink_id}");
        let persist_client = persist_clients
            .lock()
            .await
            .open(persist_location)
            .await
            .context("error creating persist client for Healthchecker")?;

        let write_handle = persist_client
            .open_writer(status_shard_id)
            .await
            .context("error opening Healthchecker persist shard")?;

        let upper = write_handle.upper().clone();

        Ok(Self {
            sink_id,
            current_status: None,
            upper,
            write_handle,
            now,
        })
    }

    /// Report a SinkStatus::Stalled and then halt with the same message.
    pub async fn report_stall_and_halt<S>(hc: Option<&mut Self>, msg: S) -> !
    where
        S: ToString + std::fmt::Debug,
    {
        if let Some(healthchecker) = hc {
            healthchecker
                .update_status(SinkStatus::Stalled(msg.to_string()))
                .await;
        }

        halt!("{msg:?}")
    }

    /// Process a [`SinkStatus`] emitted by a sink
    pub async fn update_status(&mut self, status_update: SinkStatus) {
        trace!(
            "Processing status update: {status_update:?}, current status is {current_status:?}",
            current_status = &self.current_status
        );
        // Only update status if it is a valid transition
        if SinkStatus::can_transition(self.current_status.as_ref(), &status_update) {
            let ts = (self.now)();
            let update = self.prepare_row(&status_update, ts);
            let mut ts = Timestamp::from(ts);
            loop {
                // Make sure `ts` is at least at self.upper
                for elem in self.upper.elements() {
                    ts.join_assign(elem);
                }
                let new_upper = Antichain::from_elem(ts.step_forward());

                let updates = vec![((update.clone(), ()), ts, 1)];
                match self
                    .write_handle
                    .compare_and_append(updates.iter(), self.upper.clone(), new_upper.clone())
                    .await
                {
                    Ok(Ok(Ok(()))) => {
                        self.upper = new_upper;
                        // Update internal status only after a successful append
                        self.current_status = Some(status_update);
                        break;
                    }
                    Ok(Ok(Err(Upper(actual_upper)))) => {
                        trace!(
                            "Had to retry updating status, old upper {:?}, new upper {:?}",
                            &self.upper,
                            &actual_upper
                        );
                        // Try again with the new upper.
                        // N.B.  This works because we only have one active worker per sink so don't have to worry about
                        //       transitions for a particular sink being valid.  If we have multiple workers, we'd need
                        //       to keep track of the current state and re-aggregate / check if transition is valid --
                        //       like we do for sources.
                        self.upper = actual_upper;
                    }
                    Ok(Err(invalid_use)) => panic!("compare_and_append failed: {invalid_use}"),
                    // An external error means that the operation might have succeeded or failed but we
                    // don't know. In either case it is safe to retry because:
                    // * If it succeeded, then on retry we'll get an `Upper(_)` error as if some other
                    //   process raced us. This is safe and will just cause the healthchecker to sync
                    //   again, and on retry it will notice that the new state was already processed and
                    //   finish successfully.
                    // * If it failed, then we'll succeed on retry and proceed normally.
                    Err(external_err) => {
                        trace!("compare_and_append in update_status failed: {external_err}");
                    }
                };
            }
        }
    }

    fn prepare_row(&self, status_update: &SinkStatus, ts: u64) -> SourceData {
        let timestamp = NaiveDateTime::from_timestamp(
            (ts / 1000)
                .try_into()
                .expect("timestamp seconds does not fit into i64"),
            (ts % 1000 * 1_000_000)
                .try_into()
                .expect("timestamp millis does not fit into a u32"),
        );
        let timestamp = Datum::TimestampTz(
            DateTime::from_utc(timestamp, Utc)
                .try_into()
                .expect("must fit"),
        );
        let sink_id = self.sink_id.to_string();
        let sink_id = Datum::String(&sink_id);
        let status = Datum::String(status_update.name());
        let error = status_update.error().into();
        let metadata = Datum::Null;
        let row = Row::pack_slice(&[timestamp, sink_id, status, error, metadata]);

        SourceData(Ok(row))
    }
}

/// Identify the state a worker for a given source can be at a point in time
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkStatus {
    /// Initial state of a Sink during initialization.
    Setup,
    /// Intended to be the state while the `storaged` process is initializing itself
    /// Pushed by the Healthchecker on creation.
    Starting,
    /// State indicating the sink is running fine. Pushed automatically as long
    /// as rows are being consumed.
    Running,
    /// Represents a stall in the export process that might get resolved.
    /// Existing data is still available and queryable.
    Stalled(String),
    /// Represents a irrecoverable failure in the pipeline. Data from this collection
    /// is not queryable any longer. The only valid transition from Failed is Dropped.
    Failed(String),
    /// Represents a sink that was dropped.
    Dropped,
}

impl SinkStatus {
    fn name(&self) -> &'static str {
        match self {
            SinkStatus::Setup => "setup",
            SinkStatus::Starting => "starting",
            SinkStatus::Running => "running",
            SinkStatus::Stalled(_) => "stalled",
            SinkStatus::Failed(_) => "failed",
            SinkStatus::Dropped => "dropped",
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            SinkStatus::Stalled(e) => Some(&*e),
            SinkStatus::Failed(e) => Some(&*e),
            SinkStatus::Setup => None,
            SinkStatus::Starting => None,
            SinkStatus::Running => None,
            SinkStatus::Dropped => None,
        }
    }

    fn can_transition(old_status: Option<&SinkStatus>, new_status: &SinkStatus) -> bool {
        match old_status {
            None => true,
            // Failed can only transition to Dropped
            Some(SinkStatus::Failed(_)) => matches!(new_status, SinkStatus::Dropped),
            // Dropped is a terminal state
            Some(SinkStatus::Dropped) => false,
            // All other states can transition freely to any other state
            Some(
                old @ SinkStatus::Setup
                | old @ SinkStatus::Starting
                | old @ SinkStatus::Running
                | old @ SinkStatus::Stalled(_),
            ) => old != new_status,
        }
    }
}

impl Display for SinkStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use itertools::Itertools;
    use mz_build_info::DUMMY_BUILD_INFO;
    use mz_ore::now::SYSTEM_TIME;
    use once_cell::sync::Lazy;

    use mz_ore::metrics::MetricsRegistry;
    use mz_persist_client::{PersistConfig, PersistLocation, ShardId};

    // Test suite
    #[tokio::test(start_paused = true)]
    async fn test_startup() {
        let persist_cache = persist_cache();
        let healthchecker = simple_healthchecker(ShardId::new(), 1, &persist_cache).await;

        assert_eq!(healthchecker.current_status, None);
    }

    fn stalled() -> SinkStatus {
        SinkStatus::Stalled("".into())
    }

    fn failed() -> SinkStatus {
        SinkStatus::Failed("".into())
    }

    #[tokio::test(start_paused = true)]
    async fn test_bootstrap_different_sources() {
        let shard_id = ShardId::new();
        let persist_cache = persist_cache();

        // First healthchecker is for source u1
        let mut healthchecker = simple_healthchecker(shard_id, 1, &persist_cache).await;

        tokio::time::advance(Duration::from_millis(1)).await;

        // Update status to Running
        healthchecker.update_status(SinkStatus::Running).await;

        // Start new healthchecker on the same shard for source u2
        let healthchecker = simple_healthchecker(shard_id, 2, &persist_cache).await;

        // It should ignore the state for source u1, and be at the Setup state
        assert_eq!(healthchecker.current_status, None);
    }

    #[tokio::test(start_paused = true)]
    async fn test_repeated_update() {
        let shard_id = ShardId::new();
        let persist_cache = persist_cache();
        let mut healthchecker = simple_healthchecker(shard_id, 1, &persist_cache).await;
        tokio::time::advance(Duration::from_millis(1)).await;

        // Update status to Running
        healthchecker.update_status(SinkStatus::Running).await;

        // Now update status to Running multiple times, which is a no-op
        tokio::time::advance(Duration::from_millis(1)).await;
        healthchecker.update_status(SinkStatus::Running).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        healthchecker.update_status(SinkStatus::Running).await;

        // Check in the storage collection that there is just a single row
        assert_eq!(
            dump_storage_collection(shard_id, &persist_cache)
                .await
                .len(),
            1
        );

        // Create another healthchecker with a different id, and also set it to Running
        let mut healthchecker = simple_healthchecker(shard_id, 2, &persist_cache).await;
        // Advance past the previous update, since each healthchecker has its own notion of time
        tokio::time::advance(Duration::from_millis(2)).await;
        healthchecker.update_status(SinkStatus::Running).await;

        // Now we should have two rows in the storage collection, one for each source_id
        assert_eq!(
            dump_storage_collection(shard_id, &persist_cache)
                .await
                .len(),
            2
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_forbidden_transition() {
        let shard_id = ShardId::new();
        let persist_cache = persist_cache();
        let mut healthchecker = simple_healthchecker(shard_id, 1, &persist_cache).await;
        tokio::time::advance(Duration::from_millis(1)).await;

        // Update status to Running
        healthchecker.update_status(SinkStatus::Running).await;

        // Now update status to Failed
        tokio::time::advance(Duration::from_millis(1)).await;
        let error = String::from("some error here");
        healthchecker
            .update_status(SinkStatus::Failed(error.clone()))
            .await;
        assert_eq!(
            healthchecker.current_status,
            Some(SinkStatus::Failed(error.clone()))
        );

        // Validate that we can't transition back to Running
        tokio::time::advance(Duration::from_millis(1)).await;
        healthchecker.update_status(SinkStatus::Running).await;
        assert_eq!(
            healthchecker.current_status,
            Some(SinkStatus::Failed(error))
        );

        // Check that the error message is persisted
        let error_message = dump_storage_collection(shard_id, &persist_cache)
            .await
            .into_iter()
            .find_map(|row| {
                let error = row.unpack()[3];
                if !error.is_null() {
                    Some(error.unwrap_str().to_string())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(error_message, "some error here")
    }

    #[test]
    fn test_can_transition() {
        let test_cases = [
            // Allowed transitions
            (
                Some(SinkStatus::Setup),
                vec![
                    SinkStatus::Starting,
                    SinkStatus::Running,
                    stalled(),
                    failed(),
                    SinkStatus::Dropped,
                ],
                true,
            ),
            (
                Some(SinkStatus::Starting),
                vec![
                    SinkStatus::Setup,
                    SinkStatus::Running,
                    stalled(),
                    failed(),
                    SinkStatus::Dropped,
                ],
                true,
            ),
            (
                Some(SinkStatus::Running),
                vec![
                    SinkStatus::Setup,
                    SinkStatus::Starting,
                    stalled(),
                    failed(),
                    SinkStatus::Dropped,
                ],
                true,
            ),
            (
                Some(stalled()),
                vec![
                    SinkStatus::Setup,
                    SinkStatus::Starting,
                    SinkStatus::Running,
                    failed(),
                    SinkStatus::Dropped,
                ],
                true,
            ),
            (Some(failed()), vec![SinkStatus::Dropped], true),
            (
                None,
                vec![
                    SinkStatus::Setup,
                    SinkStatus::Starting,
                    SinkStatus::Running,
                    stalled(),
                    failed(),
                    SinkStatus::Dropped,
                ],
                true,
            ),
            // Forbidden transitions
            (Some(SinkStatus::Setup), vec![SinkStatus::Setup], false),
            (
                Some(SinkStatus::Starting),
                vec![SinkStatus::Starting],
                false,
            ),
            (Some(SinkStatus::Running), vec![SinkStatus::Running], false),
            (Some(stalled()), vec![stalled()], false),
            (
                Some(failed()),
                vec![
                    SinkStatus::Setup,
                    SinkStatus::Starting,
                    SinkStatus::Running,
                    stalled(),
                    failed(),
                ],
                false,
            ),
            (
                Some(SinkStatus::Dropped),
                vec![
                    SinkStatus::Setup,
                    SinkStatus::Starting,
                    SinkStatus::Running,
                    stalled(),
                    failed(),
                    SinkStatus::Dropped,
                ],
                false,
            ),
        ];

        for test_case in test_cases {
            run_test(test_case)
        }

        fn run_test(test_case: (Option<SinkStatus>, Vec<SinkStatus>, bool)) {
            let (from_status, to_status, allowed) = test_case;
            for status in to_status {
                assert_eq!(
                    allowed,
                    SinkStatus::can_transition(from_status.as_ref(), &status),
                    "Bad can_transition: {from_status:?} -> {status:?}; expected allowed: {allowed:?}"
                );
            }
        }
    }

    // Auxiliary functions
    fn persist_cache() -> Arc<Mutex<PersistClientCache>> {
        Arc::new(Mutex::new(PersistClientCache::new(
            PersistConfig::new(&DUMMY_BUILD_INFO, SYSTEM_TIME.clone()),
            &MetricsRegistry::new(),
        )))
    }

    static PERSIST_LOCATION: Lazy<PersistLocation> = Lazy::new(|| PersistLocation {
        blob_uri: "mem://".to_owned(),
        consensus_uri: "mem://".to_owned(),
    });

    async fn new_healthchecker(
        status_shard_id: ShardId,
        source_id: GlobalId,
        persist_clients: &Arc<Mutex<PersistClientCache>>,
    ) -> Healthchecker {
        let start = tokio::time::Instant::now();
        let now_fn = NowFn::from(move || start.elapsed().as_millis() as u64);

        Healthchecker::new(
            source_id,
            persist_clients,
            (*PERSIST_LOCATION).clone(),
            status_shard_id,
            now_fn,
        )
        .await
        .expect("error creating healthchecker")
    }

    async fn simple_healthchecker(
        status_shard_id: ShardId,
        source_id: u64,
        persist_clients: &Arc<Mutex<PersistClientCache>>,
    ) -> Healthchecker {
        new_healthchecker(
            status_shard_id,
            GlobalId::User(source_id),
            &Arc::clone(persist_clients),
        )
        .await
    }

    async fn dump_storage_collection(
        shard_id: ShardId,
        persist_clients: &Arc<Mutex<PersistClientCache>>,
    ) -> Vec<Row> {
        let persist_client = persist_clients
            .lock()
            .await
            .open((*PERSIST_LOCATION).clone())
            .await
            .unwrap();

        let (write_handle, mut read_handle) = persist_client.open(shard_id).await.unwrap();

        let upper = write_handle.upper();
        let readable_upper = Antichain::from_elem(upper.elements()[0] - 1);

        read_handle
            .snapshot_and_fetch(readable_upper)
            .await
            .unwrap()
            .into_iter()
            .map(
                |((v, _), _, _): ((Result<SourceData, String>, Result<(), String>), u64, i64)| {
                    v.unwrap().0.unwrap()
                },
            )
            .collect_vec()
    }
}
