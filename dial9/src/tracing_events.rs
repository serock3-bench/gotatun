// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use dial9::telemetry::{Dial9Handle, clock_monotonic_ns};
use dial9_trace_format::TraceEvent;

#[derive(Debug, TraceEvent)]
pub struct WorkloadEvent {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    phase: String,
    detail: String,
}

pub fn record_workload_event(phase: impl Into<String>, detail: impl Into<String>) {
    let Some(handle) = Dial9Handle::try_current() else {
        return;
    };
    handle.record_event(WorkloadEvent {
        timestamp_ns: clock_monotonic_ns(),
        phase: phase.into(),
        detail: detail.into(),
    });
}
