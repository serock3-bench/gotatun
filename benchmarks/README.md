# GotaTun system benchmarks

This is a separate Cargo workspace containing the benchmark definitions for this repository. The
shared runner and temporary result artifacts live in the sibling `benchy` repository.

Run the default benchmark set through `benchy-cli` from `benchy`, or run the first scenario
directly:

```console
cargo run --release --locked \
  --config 'target."cfg(unix)".runner="/usr/bin/env"' \
  --manifest-path benchmarks/Cargo.toml \
  --package gotatun-throughput-benchmark
```

The throughput scenario runs on the controller host and deploys its current executable to the peer.
It embeds the GotaTun library, but invokes the host's `ip`, `ssh`, `scp`, `ping`, and `iperf3` tools.
Both hosts need passwordless access to the narrow privileged operations used by the benchmark.
The result records sender- and receiver-reported throughput plus process CPU usage for iperf3 and
GotaTun on the outbound (`UP`) and inbound (`DOWN`) paths. CPU percentages are normalized to one
logical core and may exceed 100% when a process uses multiple cores.

The stable v1 measurement IDs are `throughput.sender`, `throughput.receiver`, `cpu.iperf.up`,
`cpu.iperf.down`, `cpu.gotatun.up`, and `cpu.gotatun.down`. Display labels are recorded separately
and may change without breaking a result series.

## hotpath profiling

A second default benchmark, `gotatun-hotpath`, profiles GotaTun's data path with
[hotpath-rs](https://hotpath.rs). It runs the per-packet data path in-process - no TUN devices,
network, or privileges, just two tunnels exchanging packets through both directions - and records
per-function measurements from the hotpath JSON report written at the end:

- `hotpath.selftest.<function path>.calls` - how often the function ran (`count`)
- `hotpath.selftest.<function path>.time` - average time per call (`seconds`)
- `hotpath.selftest.<function path>.alloc` - average allocation bytes per call (`bytes`)

Rust paths keep their shape with `::` mapped to `.` (the measurement ID grammar does not allow
colons), e.g. `hotpath.selftest.gotatun.noise.session.format_packet_data.time`; display labels
use the short function name. The currently instrumented functions are the per-packet data path:
`Tunn::handle_outgoing_packet`, `Session::format_packet_data`, `Tunn::handle_incoming_packet`,
and `Session::receive_packet_data`. More `#[cfg_attr(feature = "hotpath", hotpath::measure)]`
annotations in the library extend the metric set. The hotpath version is pinned in this workspace
because the report format may change between releases.

Profiling deliberately does **not** run inside the throughput benchmark: when both were combined,
PR testing measured the instrumentation overhead (per-call timing plus the allocation-tracking
allocator) at a double-digit percentage of tunnel throughput. Keeping it in its own benchmark
isolates that cost while still tracking the data-path metrics over time. The instrumented data
path performs no allocations (`alloc` measurements are 0).

It can be run locally without any peer machines:

```console
cargo run --release --locked \
  --config 'target."cfg(unix)".runner="/usr/bin/env"' \
  --manifest-path benchmarks/Cargo.toml \
  --package gotatun-hotpath-benchmark
```

Configuration is read at runtime:

- `BENCHY_PEER` (default `mole@10.0.0.2`)
- `BENCHY_ALICE_ADDRESS` (default `10.0.0.1`)
- `BENCHY_BOB_ADDRESS` (default `10.0.0.2`)
- `BENCHY_INTERFACE` (default `bench0`)
- `BENCHY_WIREGUARD_PORT` (default `51821`)
- `BENCHY_IPERF_PORT` (default `5201`)
- `BENCHY_DURATION` (default `30` seconds)
- `BENCHY_MTU` (default `1440`)

WireGuard key material is generated for each run and is not stored in GitHub. The SSH host identity
is provisioned on the benchmark controller rather than injected into branch-controlled code.
