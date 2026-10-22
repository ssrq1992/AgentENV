//! Original native operation metrics, shared by repository transports.
use std::time::Instant;

pub struct MetricGuard {
    metric: &'static str,
    label: MetricGuardLabel,
    start: Instant,
    status: &'static str,
    recorded: bool,
}

#[derive(Clone, Copy)]
enum MetricGuardLabel {
    Operation(&'static str),
    OperationArtifact {
        operation: &'static str,
        artifact: &'static str,
    },
    Stage(&'static str),
}

impl MetricGuard {
    pub fn operation(metric: &'static str, operation: &'static str) -> Self {
        // A guard dropped before finish() is treated as cancellation. That
        // includes futures dropped by caller-side timeouts, which is useful
        // signal distinct from an operation returning an error.
        Self {
            metric,
            label: MetricGuardLabel::Operation(operation),
            start: Instant::now(),
            status: "canceled",
            recorded: false,
        }
    }

    /// Operation metric with an additional artifact dimension, used by OSS
    /// upload operations so per-artifact latency and cancellation stay
    /// visible (a dropped guard still records with status "canceled").
    pub fn operation_artifact(
        metric: &'static str,
        operation: &'static str,
        artifact: &'static str,
    ) -> Self {
        Self {
            metric,
            label: MetricGuardLabel::OperationArtifact {
                operation,
                artifact,
            },
            start: Instant::now(),
            status: "canceled",
            recorded: false,
        }
    }

    pub fn stage(metric: &'static str, stage: &'static str) -> Self {
        Self {
            metric,
            label: MetricGuardLabel::Stage(stage),
            start: Instant::now(),
            status: "canceled",
            recorded: false,
        }
    }

    pub fn finish<T, E>(&mut self, result: &Result<T, E>) {
        self.status = result_status(result.is_ok());
        self.record();
    }

    fn record(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let elapsed = self.start.elapsed().as_secs_f64();
        match self.label {
            MetricGuardLabel::Operation(operation) => {
                metrics::histogram!(
                    self.metric,
                    "operation" => operation,
                    "status" => self.status,
                )
                .record(elapsed);
            }
            MetricGuardLabel::OperationArtifact {
                operation,
                artifact,
            } => {
                metrics::histogram!(
                    self.metric,
                    "operation" => operation,
                    "artifact" => artifact,
                    "status" => self.status,
                )
                .record(elapsed);
            }
            MetricGuardLabel::Stage(stage) => {
                metrics::histogram!(
                    self.metric,
                    "stage" => stage,
                    "status" => self.status,
                )
                .record(elapsed);
            }
        }
    }
}

impl Drop for MetricGuard {
    fn drop(&mut self) {
        self.record();
    }
}

pub fn result_status(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "error"
    }
}
