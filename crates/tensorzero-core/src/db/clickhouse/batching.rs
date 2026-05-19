use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use crate::config::BatchWritesConfig;
use crate::db::BatchWriterHandle;
use crate::error::IMPOSSIBLE_ERROR_MESSAGE;
use enum_map::EnumMap;
use futures::{FutureExt, TryFutureExt};
use metrics::{Counter, counter};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::db::batching::{ChannelReceiver, process_channel_with_capacity_and_timeout};
use crate::db::clickhouse::{ClickHouseConnectionInfo, Rows, TableName};
use crate::error::{DelayedError, Error, ErrorDetails};

const DROP_LOG_REASON_COUNT: usize = 2;
const DROP_LOG_NEVER_EMITTED: u64 = u64::MAX;
const DROP_LOG_RATE_LIMIT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
enum DropReason {
    QueueFull,
    ChannelClosed,
}

impl DropReason {
    fn as_str(self) -> &'static str {
        match self {
            DropReason::QueueFull => "queue_full",
            DropReason::ChannelClosed => "channel_closed",
        }
    }

    fn log_slot(self) -> usize {
        match self {
            DropReason::QueueFull => 0,
            DropReason::ChannelClosed => 1,
        }
    }
}

struct DropCounters {
    queue_full: Counter,
    channel_closed: Counter,
}

impl std::fmt::Debug for DropCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropCounters").finish_non_exhaustive()
    }
}

impl DropCounters {
    fn new(table_name: TableName) -> Self {
        Self {
            queue_full: counter!(
                "tensorzero_batch_write_dropped_total",
                "table" => table_name.as_str(),
                "reason" => DropReason::QueueFull.as_str(),
            ),
            channel_closed: counter!(
                "tensorzero_batch_write_dropped_total",
                "table" => table_name.as_str(),
                "reason" => DropReason::ChannelClosed.as_str(),
            ),
        }
    }

    fn increment(&self, reason: DropReason) {
        match reason {
            DropReason::QueueFull => self.queue_full.increment(1),
            DropReason::ChannelClosed => self.channel_closed.increment(1),
        }
    }
}

/// Wraps either a bounded or unbounded mpsc sender.
#[derive(Debug)]
enum ChannelSender {
    Bounded(mpsc::Sender<String>),
    Unbounded(mpsc::UnboundedSender<String>),
}

fn create_channel_pair(capacity: Option<usize>) -> (ChannelSender, ChannelReceiver<String>) {
    match capacity {
        Some(cap) => {
            let (tx, rx) = mpsc::channel(cap);
            (ChannelSender::Bounded(tx), ChannelReceiver::Bounded(rx))
        }
        None => {
            let (tx, rx) = mpsc::unbounded_channel();
            (ChannelSender::Unbounded(tx), ChannelReceiver::Unbounded(rx))
        }
    }
}

/// A `BatchSender` is used to submit entries to the batch writer, which aggregates
/// and submits them to ClickHouse on a schedule defined by a `BatchWritesConfig`.
///
/// By default, channels are unbounded (no data is dropped). If `write_queue_capacity` is set,
/// channels are bounded: when full, new rows are dropped and logged rather than buffering
/// without limit.
///
/// When a `BatchSender` is dropped, it blocks until the batch writer finishes
/// processing all outstanding batches.
#[derive(Debug)]
pub struct BatchSender {
    // This needs to be an `Option`, so that we can drop it
    // (in particular, the sender) from our `Drop` impl.
    // This signals to the writer tasks that the channel is closed,
    // and that they should exit after they finish processing all messages
    // currently in the channel.
    channels: Option<EnumMap<TableName, ChannelSender>>,
    queue_capacity: Option<usize>,
    drop_counters: EnumMap<TableName, DropCounters>,
    drop_log_last_emitted_millis: EnumMap<TableName, [AtomicU64; DROP_LOG_REASON_COUNT]>,
    pub writer_handle: BatchWriterHandle,
}

impl BatchSender {
    pub fn new(
        clickhouse: ClickHouseConnectionInfo,
        config: BatchWritesConfig,
    ) -> Result<Self, DelayedError> {
        // We call `tokio::task::block_in_place` in our `Drop` impl to wait for outstanding
        // batch writes to finish. This does not work on the CurrentThread runtime,
        // so we fail here rather than panicking at shutdown.
        if Handle::current().runtime_flavor() == RuntimeFlavor::CurrentThread {
            return Err(DelayedError::new(ErrorDetails::InternalError {
                message: "Cannot use ClickHouse batching with the CurrentThread Tokio runtime"
                    .to_string(),
            }));
        }
        let capacity = config.write_queue_capacity;
        let mut channels: EnumMap<TableName, _> = enum_map::enum_map! {
            _ => {
                let (tx, rx) = create_channel_pair(capacity);
                (Some(tx), Some(rx))
            }
        };
        let reader_channels = enum_map::enum_map! {
            table_name => { channels[table_name].0.take().ok_or_else(|| {
                DelayedError::new(ErrorDetails::InternalError {
                    message: format!("Failed to take reader channel for table {table_name:?}. {IMPOSSIBLE_ERROR_MESSAGE}"),
                })
            })? }
        };
        let writer_channels = enum_map::enum_map! {
            table_name => { channels[table_name].1.take().ok_or_else(|| {
                DelayedError::new(ErrorDetails::InternalError {
                    message: format!("Failed to take writer channel for table {table_name:?}. {IMPOSSIBLE_ERROR_MESSAGE}"),
                })
            })? }
        };
        let writer: BatchWriter = BatchWriter {
            channels: writer_channels,
        };
        let handle = tokio::runtime::Handle::current();
        // We intentionally don't use a `CancellationToken` here - we want the batch writer
        // to keep running as long a `Sender` is still active (from inside a
        // `ClickHouseConnectionInfo`). We only exit once all of the `Sender`s are dropped,
        // (and we've finished writing our current batch)
        // We use `spawn_blocking` to ensure that when the runtime shuts down, it waits for this task to complete.
        let writer_handle = tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                tracing::debug!("ClickHouse batch write handler started");
                writer.process(clickhouse, config).await;
                tracing::info!("ClickHouse batch write handler finished");
            });
        });
        Ok(Self {
            channels: Some(reader_channels),
            queue_capacity: capacity,
            drop_counters: enum_map::enum_map! {
                table_name => DropCounters::new(table_name)
            },
            drop_log_last_emitted_millis: enum_map::enum_map! {
                _ => new_drop_log_slots()
            },
            writer_handle: writer_handle.map_err(|e| format!("{e:?}")).boxed().shared(),
        })
    }

    pub fn add_to_batch(&self, table_name: TableName, rows: Vec<String>) -> Result<(), Error> {
        let Some(channels) = &self.channels else {
            return Err(Error::new(ErrorDetails::InternalError {
                message: format!("Batch sender dropped. {IMPOSSIBLE_ERROR_MESSAGE}"),
            }));
        };
        let channel = &channels[table_name];
        for row in rows {
            match channel {
                ChannelSender::Bounded(tx) => match tx.try_send(row) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        self.record_dropped_row(table_name, DropReason::QueueFull);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        self.record_dropped_row(table_name, DropReason::ChannelClosed);
                    }
                },
                ChannelSender::Unbounded(tx) => {
                    if let Err(e) = tx.send(row) {
                        tracing::error!(
                            "Error sending row to batch channel: {e}. {IMPOSSIBLE_ERROR_MESSAGE}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn record_dropped_row(&self, table_name: TableName, reason: DropReason) {
        self.drop_counters[table_name].increment(reason);

        #[cfg(test)]
        TEST_BATCH_WRITE_DROPS.fetch_add(1, Ordering::Relaxed);

        if !self.should_emit_drop_log(table_name, reason) {
            return;
        }

        let reason_label = reason.as_str();
        let queue_capacity = self.queue_capacity.unwrap_or(0);
        match reason {
            DropReason::QueueFull => {
                tracing::warn!(
                    table = ?table_name,
                    reason = reason_label,
                    queue_capacity,
                    "ClickHouse batch channel full — dropping row. \
                     Increase `write_queue_capacity` or check ClickHouse performance."
                );
            }
            DropReason::ChannelClosed => {
                tracing::warn!(
                    table = ?table_name,
                    reason = reason_label,
                    queue_capacity,
                    "ClickHouse batch writer has shut down — dropping row."
                );
            }
        }
    }

    fn should_emit_drop_log(&self, table_name: TableName, reason: DropReason) -> bool {
        let now = monotonic_millis();
        let slot = &self.drop_log_last_emitted_millis[table_name][reason.log_slot()];
        let mut last = slot.load(Ordering::Relaxed);
        loop {
            if last != DROP_LOG_NEVER_EMITTED
                && now.saturating_sub(last) < DROP_LOG_RATE_LIMIT.as_millis() as u64
            {
                return false;
            }

            match slot.compare_exchange_weak(last, now, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(observed) => last = observed,
            }
        }
    }
}

fn new_drop_log_slots() -> [AtomicU64; DROP_LOG_REASON_COUNT] {
    std::array::from_fn(|_| AtomicU64::new(DROP_LOG_NEVER_EMITTED))
}

fn monotonic_millis() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

#[cfg(test)]
static TEST_BATCH_WRITE_DROPS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::testing::{capture_logs, get_captured_logs};
    use googletest::prelude::*;
    use metrics::with_local_recorder;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    #[gtest]
    fn bounded_sender_records_drops_and_rate_limits_logs() {
        TEST_BATCH_WRITE_DROPS.store(0, Ordering::Relaxed);
        let _contains_log = capture_logs();

        let (open_tx, _open_rx) = mpsc::channel(10);
        let open_sender = batch_sender_for_test(
            TableName::ChatInference,
            ChannelSender::Bounded(open_tx),
            10,
        );
        open_sender
            .add_to_batch(TableName::ChatInference, vec!["{}".to_string()])
            .expect("open bounded channel should accept rows below capacity");
        expect_that!(TEST_BATCH_WRITE_DROPS.load(Ordering::Relaxed), eq(0));

        let (full_tx, _full_rx) = mpsc::channel(1);
        full_tx
            .try_send("{}".to_string())
            .expect("first row should fill the bounded test channel");
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        with_local_recorder(&recorder, || {
            let full_sender =
                batch_sender_for_test(TableName::ChatInference, ChannelSender::Bounded(full_tx), 1);
            full_sender
                .add_to_batch(
                    TableName::ChatInference,
                    (0..1_000).map(|_| "{}".to_string()).collect(),
                )
                .expect("full bounded channel should drop rows without surfacing an error");
        });
        expect_that!(TEST_BATCH_WRITE_DROPS.load(Ordering::Relaxed), eq(1_000));
        expect_that!(
            dropped_counter_value(snapshotter.snapshot(), "ChatInference", "queue_full"),
            eq(Some(1_000))
        );

        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        with_local_recorder(&recorder, || {
            let closed_sender = batch_sender_for_test(
                TableName::ChatInference,
                ChannelSender::Bounded(closed_tx),
                1,
            );
            closed_sender
                .add_to_batch(TableName::ChatInference, vec!["{}".to_string()])
                .expect("closed bounded channel should drop rows without surfacing an error");
        });
        expect_that!(TEST_BATCH_WRITE_DROPS.load(Ordering::Relaxed), eq(1_001));
        expect_that!(
            dropped_counter_value(snapshotter.snapshot(), "ChatInference", "channel_closed"),
            eq(Some(1))
        );

        let logs = get_captured_logs();
        expect_that!(logs.matches("ClickHouse batch channel full").count(), eq(1));
        expect_that!(
            logs.matches("ClickHouse batch writer has shut down")
                .count(),
            eq(1)
        );
        expect_that!(logs, contains_substring("reason=\"queue_full\""));
        expect_that!(logs, contains_substring("reason=\"channel_closed\""));
        expect_that!(logs, contains_substring("queue_capacity=1"));
        expect_that!(logs, contains_substring("table=ChatInference"));
    }

    fn batch_sender_for_test(
        target_table: TableName,
        target_channel: ChannelSender,
        queue_capacity: usize,
    ) -> BatchSender {
        let mut target_channel = Some(target_channel);
        let channels = enum_map::enum_map! {
            table_name => {
                if table_name == target_table {
                    target_channel
                        .take()
                        .expect("target channel should be consumed exactly once")
                } else {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    ChannelSender::Unbounded(tx)
                }
            }
        };

        BatchSender {
            channels: Some(channels),
            queue_capacity: Some(queue_capacity),
            drop_counters: enum_map::enum_map! {
                table_name => DropCounters::new(table_name)
            },
            drop_log_last_emitted_millis: enum_map::enum_map! {
                _ => new_drop_log_slots()
            },
            writer_handle: futures::future::ready(Ok(())).boxed().shared(),
        }
    }

    fn dropped_counter_value(
        snapshot: metrics_util::debugging::Snapshot,
        expected_table: &str,
        expected_reason: &str,
    ) -> Option<u64> {
        snapshot
            .into_vec()
            .into_iter()
            .find_map(|(key, _, _, value)| {
                if key.key().name() != "tensorzero_batch_write_dropped_total" {
                    return None;
                }

                let table = key
                    .key()
                    .labels()
                    .find(|label| label.key() == "table")
                    .map(|label| label.value());
                let reason = key
                    .key()
                    .labels()
                    .find(|label| label.key() == "reason")
                    .map(|label| label.value());

                if table != Some(expected_table) || reason != Some(expected_reason) {
                    return None;
                }

                match value {
                    DebugValue::Counter(count) => Some(count),
                    _ => None,
                }
            })
    }
}

pub struct BatchWriter {
    channels: EnumMap<TableName, ChannelReceiver<String>>,
}

impl BatchWriter {
    pub async fn process(self, clickhouse: ClickHouseConnectionInfo, config: BatchWritesConfig) {
        let mut join_set = JoinSet::new();
        let batch_timeout = Duration::from_millis(
            config
                .flush_interval_ms
                .unwrap_or_else(crate::config::default_flush_interval_ms),
        );
        let max_rows = config
            .max_rows
            .unwrap_or_else(crate::config::default_max_rows);

        for (table_name, channel) in self.channels {
            let clickhouse = clickhouse.clone();
            let flush = move |buffer: Vec<String>| {
                let clickhouse = clickhouse.clone();
                async move {
                    if let Err(e) = clickhouse
                        .write_non_batched::<()>(Rows::Serialized(&buffer), table_name)
                        .await
                    {
                        // TODO: if this errors, should we retry?
                        // Log the error (converting DelayedError to Error)
                        e.log();
                    }
                    buffer
                }
            };
            join_set.spawn(async move {
                process_channel_with_capacity_and_timeout(channel, max_rows, batch_timeout, flush)
                    .await;
            });
        }
        while let Some(result) = join_set.join_next().await {
            if let Err(e) = result {
                tracing::error!("Error in batch writer: {e}");
            }
        }
    }
}
