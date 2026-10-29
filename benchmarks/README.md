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

The benchmark also profiles GotaTun's data path with [hotpath-rs](https://hotpath.rs). Both
endpoints build with the crate features enabled through the benchmark's default `hotpath` feature
(`gotatun/hotpath-alloc`): the library instrumentation is compiled to nothing when the feature is
off, so production builds are unaffected. Each endpoint writes a hotpath JSON report when it shuts
down, and the controller converts both reports into measurements prefixed by their host:

- `hotpath.<host>.<function path>.calls` - how often the function ran (`count`)
- `hotpath.<host>.<function path>.time` - average time per call (`seconds`)
- `hotpath.<host>.<function path>.alloc` - average allocation bytes per call (`bytes`)

Rust paths keep their shape with `::` mapped to `.` (the measurement ID grammar does not allow
colons), e.g. `hotpath.alice.gotatun.noise.session.format_packet_data.time`; display labels use
the short function name. The currently instrumented functions are the per-packet data path:
`Tunn::handle_outgoing_packet`, `Session::format_packet_data`, `Tunn::handle_incoming_packet`,
and `Session::receive_packet_data`. More `#[cfg_attr(feature = "hotpath", hotpath::measure)]`
annotations extend the metric set. The hotpath version is pinned in this workspace because the
report format may change between releases.

Profiling adds per-call instrumentation overhead to the measured functions, so the throughput
numbers include it; comparisons stay valid because every run is built the same way. The
instrumented data path currently performs no allocations (`alloc` measurements are 0).

A `selftest` subcommand exercises the same data path in-process - no TUN devices, privileges, or
peer machines - and produces a result file with the converted measurements plus the raw report in
the system temp directory, so the whole chain can be verified locally:

```console
cargo run --release --locked \
  --config 'target."cfg(unix)".runner="/usr/bin/env"' \
  --manifest-path benchmarks/Cargo.toml \
  --package gotatun-throughput-benchmark -- selftest
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
