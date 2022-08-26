use std::collections::HashMap;
use std::time::Duration;

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
use prometheus::core::{AtomicF64, AtomicU64, Collector, GenericCounterVec, GenericGauge};
use prometheus::{
    exponential_buckets, histogram_opts, opts, register_gauge_with_registry,
    register_histogram_with_registry, register_int_counter_vec_with_registry, Histogram, Registry,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::task::TaskId;

// When execution is done, it need to call clear_record() in BatchTaskMetrics.
// The clear_record() will send the Collector to delete_queue, if the queue is full, the execution
// will be blocked so that user can't get the result immediately.
pub struct BatchTaskMetricsManager {
    registry: Registry,
    sender: UnboundedSender<Box<dyn Collector>>,
}

impl BatchTaskMetricsManager {
    pub fn new(registry: Registry) -> Self {
        // Spawn a deletor.
        // TaskMetricsManager will create BatchTaskMetrics for each BatchExecution and
        // BatchTaskMetrics will create their own Collector. When the BatchExecution is
        // done, BatchTaskMetrics will send their Collectors to the delete_queue.
        // The deletor will unregister the Collectors from the registry periodically.
        // We store the collector in delete_cache first and unregister it next time to make sure the
        // metrics be collected by prometheus.
        let (delete_queue_sender, mut delete_queue_receiver) =
            tokio::sync::mpsc::unbounded_channel::<Box<dyn Collector>>();
        let deletor_registry = registry.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            let mut delete_cache: Vec<Box<dyn Collector>> = Vec::new();
            let mut connect = true;
            while connect {
                // run every minute.
                tracing::info!("BatchTaskMetricsManager Deletor is running...");
                let _ = interval.tick().await;

                // delete all record in delete_cache .
                while let Some(collector) = delete_cache.pop() {
                    if deletor_registry.unregister(collector).is_err() {
                        // Ignore: collector is not registered.
                    }
                }

                // read from delete queue and push into delete_cache.
                loop {
                    match delete_queue_receiver.try_recv() {
                        Ok(collector) => {
                            delete_cache.push(collector);
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            break;
                        }
                        Err(_) => {
                            // Error handle need modify later.
                            error!("delete_queue_receiver is closed");
                            connect = false;
                            break;
                        }
                    }
                }
            }
        });

        Self {
            registry,
            sender: delete_queue_sender,
        }
    }

    pub fn create_task_metrics(&self, id: TaskId) -> BatchTaskMetrics {
        BatchTaskMetrics::new(self.registry.clone(), id, Some(self.sender.clone()))
    }

    /// Create a new `BatchTaskMetricsManager` instance used in tests or other places.
    pub fn for_test() -> Self {
        let (delete_queue_sender, _) = tokio::sync::mpsc::unbounded_channel::<Box<dyn Collector>>();
        Self {
            sender: delete_queue_sender,
            registry: prometheus::Registry::new(),
        }
    }
}

macro_rules! for_each_task_metric {
    ($macro:ident, $($x:tt),*) => {
        $macro! {
            [$($x),*],

            { exchange_recv_row_number, GenericCounterVec<AtomicU64> },
            { task_first_poll_delay, GenericGauge<AtomicF64> },
            { task_fast_poll_duration, GenericGauge<AtomicF64> },
            { task_idle_duration, GenericGauge<AtomicF64> },
            { task_poll_duration, GenericGauge<AtomicF64> },
            { task_scheduled_duration, GenericGauge<AtomicF64> },
            { task_slow_poll_duration, GenericGauge<AtomicF64> },
        }
    };
}

macro_rules! def_task_metrics {
    ([$struct:ident], $( { $metric:ident, $type:ty }, )*) => {
        #[derive(Clone)]
        pub struct $struct {
            sender: Option<UnboundedSender<Box<dyn Collector>>>,
            $( pub $metric: $type, )*
        }
    };
}

macro_rules! delete_task_metrics {
    ([$self:ident], $( { $metric:ident, $type:ty }, )*) => {
        if let Some(sender) = $self.sender.as_ref() {
            $(
                if sender
                    .send(Box::new($self.$metric.clone()))
                    .is_err()
                {
                    error!("Failed to send delete record to delete queue");
                }
            )*
        }
    };
}

for_each_task_metric!(def_task_metrics, BatchTaskMetrics);

impl BatchTaskMetrics {
    pub fn new(
        registry: Registry,
        id: TaskId,
        sender: Option<UnboundedSender<Box<dyn Collector>>>,
    ) -> Self {
        let const_labels = HashMap::from([
            ("query_id".to_string(), id.query_id),
            ("stage_id".to_string(), id.stage_id.to_string()),
            ("task_id".to_string(), id.task_id.to_string()),
        ]);

        let exchange_recv_row_number = register_int_counter_vec_with_registry!(
            opts!(
                "batch_exchange_recv_row_number",
                "Total number of row that have been received from upstream source",
            )
            .const_labels(const_labels.clone()),
            &["source_stage_id", "source_task_id"],
            registry
        )
        .unwrap();

        let task_first_poll_delay = register_gauge_with_registry!(
            opts!(
                "batch_task_first_poll_delay",
                "The total duration (s) elapsed between the instant tasks are instrumented, and the instant they are first polled.",
            ).const_labels(const_labels.clone()),
            registry,
        ).unwrap();

        let task_fast_poll_duration = register_gauge_with_registry!(
            opts!(
                "batch_task_fast_poll_duration",
                "The total duration (s) of fast polls.",
            )
            .const_labels(const_labels.clone()),
            registry,
        )
        .unwrap();

        let task_idle_duration = register_gauge_with_registry!(
            opts!(
                "batch_task_idle_duration",
                "The total duration (s) that tasks idled.",
            )
            .const_labels(const_labels.clone()),
            registry,
        )
        .unwrap();

        let task_poll_duration = register_gauge_with_registry!(
            opts!(
                "batch_task_poll_duration",
                "The total duration (s) elapsed during polls.",
            )
            .const_labels(const_labels.clone()),
            registry,
        )
        .unwrap();

        let task_scheduled_duration = register_gauge_with_registry!(
            opts!(
                "batch_task_scheduled_duration",
                "The total duration (s) that tasks spent waiting to be polled after awakening.",
            )
            .const_labels(const_labels.clone()),
            registry,
        )
        .unwrap();

        let task_slow_poll_duration = register_gauge_with_registry!(
            opts!(
                "batch_task_slow_poll_duration",
                "The total duration (s) of slow polls.",
            )
            .const_labels(const_labels),
            registry,
        )
        .unwrap();

        Self {
            sender,
            exchange_recv_row_number,
            task_first_poll_delay,
            task_fast_poll_duration,
            task_idle_duration,
            task_poll_duration,
            task_scheduled_duration,
            task_slow_poll_duration,
        }
    }

    /// This function execute after the exucution done.
    /// Send all the record to the delete queue.
    pub fn clear_record(&self) {
        for_each_task_metric!(delete_task_metrics, self)
    }

    /// Create a new `BatchTaskMetrics` instance used in tests or other places.
    pub fn for_test() -> Self {
        Self::new(prometheus::Registry::new(), TaskId::default(), None)
    }
}

pub struct BatchMetrics {
    pub row_seq_scan_next_duration: Histogram,
}

impl BatchMetrics {
    pub fn new(registry: Registry) -> Self {
        let opts = histogram_opts!(
            "batch_row_seq_scan_next_duration",
            "Time spent deserializing into a row in cell based table.",
            exponential_buckets(0.0001, 2.0, 20).unwrap() // max 52s
        );
        let row_seq_scan_next_duration = register_histogram_with_registry!(opts, registry).unwrap();

        Self {
            row_seq_scan_next_duration,
        }
    }

    /// Create a new `BatchMetrics` instance used in tests or other places.
    pub fn for_test() -> Self {
        Self::new(prometheus::Registry::new())
    }
}
