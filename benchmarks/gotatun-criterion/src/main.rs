// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{path::PathBuf, process::Stdio};

use anyhow::{Context, Result, bail};
use benchy_runner::{
    BenchmarkDefinition, MachineLock, Recorder, default_output_path, hotpath::record_hotpath_report,
};
use clap::Parser;
use tokio::process::Command;

const DEFINITION: BenchmarkDefinition = BenchmarkDefinition {
    repository: "gotatun",
    name: "gotatun-criterion",
    description: "Criterion benches of the crypto primitives and packet reassembly measured with hotpath-rs",
};

/// The gotatun workspace holding the criterion benches this controller profiles.
const GOTATUN_MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");

/// Bench machines configure a runner for cargo targets that runs them under `sudo`, which
/// strips the environment - including the HOTPATH_* variables - from the bench. Passing the
/// bench through `env` instead keeps the environment, like `benchy-cli run` does for its own
/// invocations.
const CARGO_RUNNER_CONFIG: &str = r#"target."cfg(unix)".runner="/usr/bin/env""#;

#[derive(Parser)]
#[command(
    version,
    about = "Criterion benchmark controller profiling one bench per process through hotpath"
)]
struct Cli {
    #[arg(long)]
    output: Option<PathBuf>,
    /// Routine executions per benchmark inside the single profiled criterion iteration.
    #[arg(long, default_value_t = 1000)]
    iterations: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let output = cli
        .output
        .unwrap_or_else(|| default_output_path(DEFINITION.name));

    // Keep the machine exclusive: timing benchmarks need an idle host.
    let _machine_lock = MachineLock::acquire("/tmp/benchy.lock")?;

    let bench_names = bench_targets().await?;
    let mut recorder = Recorder::new(DEFINITION, &output).await;
    recorder.parameter("routines_per_bench", cli.iterations);

    let result = run_benchmarks(&mut recorder, &bench_names, cli.iterations).await;
    match result {
        Ok(count) => {
            recorder.parameter("benchmarks", count);
            recorder.success().await
        }
        Err(error) => {
            recorder.failure(&error).await?;
            Err(error)
        }
    }
}

/// The bench targets of the gotatun package in the repository's root workspace, whose
/// definitions this benchmark profiles in place.
async fn bench_targets() -> Result<Vec<String>> {
    let metadata = Command::new("cargo")
        .args(["metadata", "--manifest-path", GOTATUN_MANIFEST, "--no-deps"])
        .output()
        .await
        .context("failed to run cargo metadata")?;
    if !metadata.status.success() {
        bail!(
            "cargo metadata failed with {}: {}",
            metadata.status,
            String::from_utf8_lossy(&metadata.stderr).trim()
        );
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&metadata.stdout).context("invalid cargo metadata output")?;
    let benches = metadata["packages"]
        .as_array()
        .and_then(|packages| {
            packages
                .iter()
                .find(|package| package["name"] == "gotatun")
                .map(|gotatun| &gotatun["targets"])
        })
        .and_then(|targets| targets.as_array())
        .with_context(|| "cargo metadata has no gotatun package")?;
    let names: Vec<String> = benches
        .iter()
        .filter(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bench"))
        })
        .filter_map(|target| target["name"].as_str().map(str::to_owned))
        .collect();
    if names.is_empty() {
        bail!("the gotatun package declares no bench targets");
    }
    Ok(names)
}

/// Profiles every benchmark of every bench target, one process each, and records the
/// measurements from each report.
async fn run_benchmarks(
    recorder: &mut Recorder,
    bench_names: &[String],
    iterations: u64,
) -> Result<usize> {
    let mut recorded = 0;
    for bench in bench_names {
        let benchmark_ids = list_benchmarks(bench).await?;
        for (index, id) in benchmark_ids.iter().enumerate() {
            println!("::group::criterion {bench}/{id}");
            // The reports stay outside any result directory: export-bencher treats every JSON
            // file there as a benchmark result.
            let report_path =
                std::env::temp_dir().join(format!("gotatun-criterion-{bench}-{index}.json"));
            let run = Command::new("cargo")
                .args([
                    "bench",
                    "--manifest-path",
                    GOTATUN_MANIFEST,
                    "--package",
                    "gotatun",
                    "--bench",
                    bench,
                    "--features",
                    "hotpath",
                    "--locked",
                    "--quiet",
                    "--config",
                    CARGO_RUNNER_CONFIG,
                    "--",
                    "--test",
                    "--exact",
                    id,
                ])
                .env("HOTPATH_CRITERION_ITERATIONS", iterations.to_string())
                .env("HOTPATH_OUTPUT_FORMAT", "json")
                .env("HOTPATH_OUTPUT_PATH", &report_path)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .output()
                .await
                .with_context(|| format!("failed to run the {bench} bench"))?;
            if !run.status.success() {
                bail!(
                    "criterion bench {bench}/{id} failed with {}: {}",
                    run.status,
                    String::from_utf8_lossy(&run.stderr).trim()
                );
            }
            let report = tokio::fs::read(&report_path)
                .await
                .with_context(|| format!("no hotpath report at {}", report_path.display()))?;
            record_hotpath_report(recorder, None, &report)?;
            recorded += 1;
            println!("::endgroup::");
        }
    }
    Ok(recorded)
}

async fn list_benchmarks(bench: &str) -> Result<Vec<String>> {
    let listing = Command::new("cargo")
        .args([
            "bench",
            "--manifest-path",
            GOTATUN_MANIFEST,
            "--package",
            "gotatun",
            "--bench",
            bench,
            "--features",
            "hotpath",
            "--locked",
            "--quiet",
            "--config",
            CARGO_RUNNER_CONFIG,
            "--",
            "--list",
        ])
        .output()
        .await
        .with_context(|| format!("failed to list the {bench} benches"))?;
    if !listing.status.success() {
        bail!(
            "listing the {bench} benches failed with {}: {}",
            listing.status,
            String::from_utf8_lossy(&listing.stderr).trim()
        );
    }
    let ids = String::from_utf8(listing.stdout)
        .context("criterion benchmark listing was not UTF-8")?
        .lines()
        .filter_map(|line| line.split_once(':').map(|(id, _)| id.trim().to_owned()))
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        bail!("the {bench} bench lists no benchmarks");
    }
    Ok(ids)
}
