# dial9 profiling harness

`gotatun-dial9` records a [dial9](https://github.com/dial9-rs/dial9) trace of a gotatun
tunnel under a ping workload, using the benchmark machines. It is deliberately manual and
separate from the Bencher-tracked benchmarks: dial9 traces are large binary artifacts, not
metrics series, so they are uploaded as GitHub artifacts (7-day retention) for local
consumption instead.

The `dial9` crate pins `0.5.0-rc0` (matching the traces this harness produces); the CLI
used to view them is currently 0.5.6, which decodes those traces. Keep the CLI usable with
the recorded traces when bumping the pin.

## Running it

The GitHub workflow *Dial9 profile* is manually dispatched from the Actions tab: it runs on
the alice runner (self-hosted `benchy` labels), asserts the machine lock like every other
benchmark, builds this harness, sets up an ephemeral WireGuard pair with bob, and records
the profiled alice-side tunnel while pinging through the tunnel. The trace segments are
uploaded as an artifact named `dial9-traces-<run id>` and deleted from the runner.

Manual dispatch inputs: `ping-count` (100), `ping-interval-ms` (10),
`ping-payload-bytes` (56), and `cpu-sample-hz` (999).

The harness is a single binary with three modes: `run` (the controller invoked by the
workflow, which builds the WireGuard pair, deploys the endpoint to bob over ssh, and starts
the profiled tunnel through sudo), `endpoint` (the plain far-side tunnel on bob, never
profiled), and `tunnel` (the profiled near side, run as root by `run`). Key material is
ephemeral, generated in memory for each run, and only ever passed between the two processes.

Two workloads exist: `ping` (default; the original quiet ICMP probe) and `iperf`, which
runs iperf3 through the tunnel for a line-rate load, first sending from this side and then
receiving (`-R`), so one trace covers both the encrypt/send and the decrypt/receive data
paths. The controller starts the matching iperf3 server on the peer automatically.

The tunnel records CPU profiling, scheduler events, memory profiling with liveset tracking,
process resource usage, Tokio task tracking, and everything gotatun logs (the tracing
layer records the events into the trace alongside). The trace directory is capped at 128 MiB
by the disk buffer.

CPU profiling wants `kernel.perf_event_paranoid <= 2` and scheduler events `<= 1`; run as
root (which both modes are) they usually work. When perf is restricted, dial9 falls back to
its ctimer backend and the workflow prints the current value. Kernel stack symbolization
additionally wants `kernel.kptr_restrict = 0`.

## Consuming the traces locally

Install the `dial9` CLI (prebuilt from [dial9 releases](https://github.com/dial9-rs/dial9/releases),
or `cargo install --locked dial9 --features cli`) and download an artifact:

```bash
gh run download -n dial9-traces-<run id> --repo <this repo> -D ./dial9-traces
```

Then serve them and open the viewer:

```bash
dial9 serve --local-dir ./dial9-traces
```

For scripted analysis, `dial9 agents` describes the bundled analysis skills, and
`dial9 agents toolkit <dir>` extracts the JS toolkit (`node <dir>/analyze.js
./dial9-traces`). The traces also survive `dial9 trace-shape extract`, which sanitizes them
into structural fingerprints you can share without payloads.

## Building and running by hand

The `.cargo/config.toml` in this directory sets `tokio_unstable` and forced frame pointers,
which dial9's tokio instrumentation and its frame-pointer unwinders require - so build from
this directory:

```bash
cargo build --locked --profile profiling   # release + debug info, keeps symbols
sudo ./target/profiling/gotatun-dial9 run --trace-dir ./traces --ping-count 100
```

`run` needs passwordless sudo for the local tunnel and passwordless ssh to the peer, like
the throughput benchmark. Run the modes by hand only for debugging; the two tunnel
addresses must not both exist as local addresses on one kernel, or the kernel answers
locally instead of routing through the TUN.
