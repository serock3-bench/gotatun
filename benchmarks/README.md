# GotaTun system benchmarks

This is a separate Cargo workspace containing the benchmark definitions for this repository. The
shared runner and temporary result artifacts live in the sibling `benchy` repository.

Run the default benchmark set through `benchy-cli` from `benchy`, or run the first scenario
directly:

```console
cargo run --release --locked \
  --config 'target."cfg(unix)".runner="/usr/bin/env"' \
  --manifest-path benchmarks/Cargo.toml \
  --package gotatun-download-benchmark
```

The two network benchmarks share an uninstrumented controller in `throughput-common`. Each
builds and deploys its own executable, embeds GotaTun, and invokes `ip`, `ssh`, `scp`, `ping`,
`ss` and `iperf3`. Both hosts need passwordless sudo for endpoint setup and cleanup.

Alice is always the iperf client and the measured host. `gotatun-download` uses reverse mode
(Bob → Alice); `gotatun-upload` uses normal mode (Alice → Bob). Both record exactly the same
three measurements: `throughput.network`, `cpu.gotatun`, and `cpu.iperf`. Throughput uses Alice's
receiver summary for download and sender summary for upload. CPU samples come only from
Alice: GotaTun's process ticks and iperf's local CPU total. Alice's local one-second intervals
provide throughput uncertainty in either direction; Bob's JSON report is not requested. Bob needs
no process CPU sampling.
CPU percentages are normalized to one logical core and may exceed 100%.

## What each workflow measures

| Workflow / benchmark | Output | Meaning |
| --- | --- | --- |
| Benchmarks / `gotatun-download` | Gb/s and logical-core CPU % | Bob → Alice, measured on Alice; GotaTun is uninstrumented. |
| Benchmarks / `gotatun-upload` | Same shared measures | Alice → Bob, measured on Alice; GotaTun is uninstrumented. |
| Benchmarks / `gotatun-hotpath` | ns/call, allocated B/call, lock ns | Two in-process tunnels exchanging packets, with hotpath instrumentation. |
| Dial9 profile | Trace artifact | Alice's full device under TCP send then receive; manual, no Bencher values. |

All three numeric benchmarks are in the default set. Select IDs in the manual workflow to run only
one. The network benchmarks run on pushes to fork main and approved PRs; the nightly workflow
refreshes main. Profiling and throughput builds are separate, so Cargo feature unification does
not instrument the network measurement.

## Hotpath diagnostics

`gotatun-hotpath` uses [hotpath-rs](https://hotpath.rs) 0.30.1 with its counting allocator.
It completes a handshake, warms up with 10,000 packets in each direction, then measures 200,000
packets per direction with a 1,200-byte IP payload and the `ring` AEAD backend. No sockets,
TUN devices, or GSO code execute in this workload. It cannot validate a socket optimization.

Each function is a Bencher benchmark such as `gotatun-hotpath/format_packet_data`, with shared
`time` (inclusive ns/call) and `allocated` (exclusive B/call) measures. Parent timings include
children and instrumentation overhead; do not add them. Allocation volume is not retained
memory; a parent's zero excludes allocations attributed to instrumented children.

Locks appear under `gotatun-hotpath/lock/<name>` with shared `lock.wait` and `lock.hold` measures
in ns. Instances from one source site are weighted by acquisition count. This serial workload
measures **uncontended** acquisition, not contention/scaling. Lock statistics include the
handshake and warmup (two acquisitions per received data packet).

The four expected functions are `handle_outgoing_packet`, `format_packet_data`,
`handle_incoming_packet`, and `receive_packet_data`. The benchmark fails if any has a wrong
call count or a timing sampling override. The full report, including original function paths, call counts,
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
