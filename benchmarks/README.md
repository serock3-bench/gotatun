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

## What each workflow measures

| Workflow / benchmark | Output | Meaning |
| --- | --- | --- |
| Benchmarks / `gotatun-throughput` | Gb/s and logical-core CPU % | Real TCP transfer from Bob to Alice; GotaTun is uninstrumented. |
| Benchmarks / `gotatun-hotpath` | ns/call, allocated B/call, lock ns | Two in-process tunnels exchanging packets, with hotpath instrumentation. |
| Dial9 profile | Trace artifact | Alice's full device under TCP send then receive; manual, no Bencher values. |

The first two benchmarks are in the default set. Select IDs in the manual workflow to run only
one. The network benchmark runs on pushes to fork main and approved PRs; the nightly workflow
refreshes main. Profiling and throughput builds are separate, so Cargo feature unification does
not instrument the network measurement.

## Hotpath diagnostics

`gotatun-hotpath` uses [hotpath-rs](https://hotpath.rs) 0.30.1 with its counting allocator.
It completes a handshake, warms up with 10,000 packets in each direction, then measures 200,000
packets per direction with a 1,200-byte IP payload and the `ring` AEAD backend. No sockets,
TUN devices, or GSO code execute in this workload. It cannot validate a socket optimization.

- `hotpath.<function path>.time`: inclusive average ns/call. Parent timings include child
  timings and instrumentation overhead; do not add them together.
- `hotpath.<function path>.alloc`: exclusive allocated B/call, excluding instrumented children.
  This measures allocated volume, not retained memory. A zero means no observed allocation
  attributed to that function, not that the whole packet path is allocation-free.
- `hotpath.lock.<name>.wait` / `.hold`: acquisition-count-weighted average ns across instances
  from the same source site. This serial workload measures **uncontended** lock overhead,
  not contention or multicore scaling. Hotpath collects these from lock creation, so they
  include the handshake and warmup (two acquisitions per received data packet).

The four expected functions are `handle_outgoing_packet`, `format_packet_data`,
`handle_incoming_packet`, and `receive_packet_data`. The benchmark fails if any has a wrong
call count. Function paths map `::` to `.` in metric IDs. The full report, including call counts,
sampling settings and lock instances, is retained as `profiles/gotatun-hotpath.json` beside the
raw Benchy result in the workflow artifact. Each repeated sample has its own report.

These are diagnostic series, without regression thresholds. Compare them only with the same
workload, backend, profiler version and settings. The ordinary Criterion benches remain
available through `cargo bench`; they are not part of Benchy.

## Network configuration

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
