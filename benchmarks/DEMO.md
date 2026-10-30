# Benchmark demo

## Baseline and thresholds

The baseline consists of six complete main runs from commit `ad894cd1d5aae3836725596306548360e2643ddd`.
All runs include upload, download and hotpath; each network transfer lasts 30 seconds.
The table reports empirical variation **across the six complete runs**. This calibration SD
is distinct from the within-transfer standard-error bounds uploaded with throughput values.

| Benchmark | Measure | Mean | Across-run SD | CV |
| --- | --- | ---: | ---: | ---: |
| `gotatun-download` | `cpu.gotatun` | 377.461 logical CPU % | 1.138 | 0.30% |
| `gotatun-download` | `cpu.iperf` | 77.700 logical CPU % | 0.568 | 0.73% |
| `gotatun-download` | `throughput.network` | 2.647 Gb/s | 0.028 | 1.06% |
| `gotatun-hotpath/format_packet_data` | `allocated` | 1252.000 B | 0.000 | 0.00% |
| `gotatun-hotpath/format_packet_data` | `time` | 763.971 ns | 2.259 | 0.30% |
| `gotatun-hotpath/handle_incoming_packet` | `allocated` | 0.000 B | 0.000 | all zero |
| `gotatun-hotpath/handle_incoming_packet` | `time` | 919.200 ns | 2.878 | 0.31% |
| `gotatun-hotpath/handle_outgoing_packet` | `allocated` | 0.000 B | 0.000 | all zero |
| `gotatun-hotpath/handle_outgoing_packet` | `time` | 871.071 ns | 3.741 | 0.43% |
| `gotatun-hotpath/lock/receiving-key-counter` | `lock.hold` | 20.000 ns | 0.000 | 0.00% |
| `gotatun-hotpath/lock/receiving-key-counter` | `lock.wait` | 16.000 ns | 0.000 | 0.00% |
| `gotatun-hotpath/receive_packet_data` | `allocated` | 0.000 B | 0.000 | all zero |
| `gotatun-hotpath/receive_packet_data` | `time` | 843.354 ns | 2.814 | 0.33% |
| `gotatun-upload` | `cpu.gotatun` | 275.341 logical CPU % | 3.666 | 1.33% |
| `gotatun-upload` | `cpu.iperf` | 5.722 logical CPU % | 0.267 | 4.66% |
| `gotatun-upload` | `throughput.network` | 2.672 Gb/s | 0.037 | 1.40% |

Thresholds are in the trusted `.github/workflows/publish-benchmarks.yml`, not on Alice.
They compare central values to the historical mean, requiring at least five historical metrics
and using at most ten:

| Shared measure | Alert on |
| --- | --- |
| Throughput | decrease over 7% |
| GotaTun CPU | increase over 10% |
| Time per call | increase over 10% |
| Allocated per call | increase over 5% |
| Lock wait / hold | increase over 30% |
| iperf CPU | diagnostic only; no threshold |

These are practical tolerances, not confidence levels. The wider lock tolerance allows for
small absolute changes and timer quantization. CPU rises can accompany higher throughput;
interpret those two measures together. Zero-allocation cases normally stay exactly zero;
a positive allocation is a change, not a percentage that can meaningfully be displayed.

Baseline workflow runs:

- [41055931535](https://github.com/serock3-bench/gotatun/actions/runs/41055931535)
- [41056026333](https://github.com/serock3-bench/gotatun/actions/runs/41056026333)
- [41056034535](https://github.com/serock3-bench/gotatun/actions/runs/41056034535)
- [41056041210](https://github.com/serock3-bench/gotatun/actions/runs/41056041210)
- [41056049775](https://github.com/serock3-bench/gotatun/actions/runs/41056049775)
- [41056058746](https://github.com/serock3-bench/gotatun/actions/runs/41056058746)

## Walkthrough

1. Open the `gotatun-test` Bencher plot and select `Throughput (Gb/s)` for both
   `gotatun-upload` and `gotatun-download`. Then compare them using the shared GotaTun CPU
   measure. Every network measurement is from Alice's perspective.
2. Open the draft PR for branch `demo/perf-regression`. It deliberately allocates and fills a
   128 KiB scratch buffer on every encrypted packet without changing packet contents. Inspect
   the three added lines, passing correctness tests, raw artifact, Bencher comment and failed
   performance check. No result values are fabricated. **Do not merge this PR.**
3. Open [the GSO PR](https://github.com/serock3-bench/gotatun/pull/6) and its refreshed benchmark
   report. Discuss the throughput/CPU tradeoff actually measured; do not assume the older
   local 23–28% throughput claim applies to Alice/Bob.
4. Open matching main and GSO Dial9 traces. They profile five seconds sending and five seconds
   receiving, with Alice profiled and Bob unprofiled. `WorkloadEvent` marks the phases and
   `TracingEventRecord` contains ordinary tracing events with fields, scope and source locations.
   CPU samples explain where time goes; they do not count syscall invocations.

The profiling workflow uploads traces for seven days. Keep downloaded demo copies locally
if the presentation is later. To view an extracted artifact:

```bash
dial9 serve --local --local-dir /path/to/extracted/dial9-traces
```

The workflow's `iperf-seconds` input changes the profile duration. This is separate from the
30-second uninstrumented network benchmarks used for numeric regression checks.
