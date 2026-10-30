// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The controller that runs on the benchmark controller host (unprivileged): it claims the
//! machines, builds an ephemeral WireGuard pair, deploys the endpoint to the peer over ssh,
//! starts the profiled near-side tunnel through sudo, waits for it to finish, and tears
//! the peer side back down.

use std::{
    net::{IpAddr, Ipv4Addr},
    path::Path,
    process::Stdio,
};

use clap::Args as ClapArgs;
use eyre::{Context as _, Result, bail};
use gotatun::x25519::{PublicKey, StaticSecret};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

use crate::lock::MachineLock;

const REMOTE_DIR_PREFIX: &str = "benchy-dial9";

#[derive(Debug, ClapArgs)]
pub struct RunArgs {
    #[arg(long, default_value = "mole@10.0.0.2")]
    peer: String,
    #[arg(long, default_value = "10.0.0.1")]
    alice_address: IpAddr,
    #[arg(long, default_value = "10.0.0.2")]
    bob_address: IpAddr,
    #[arg(long, default_value_t = 51821)]
    listen_port: u16,
    /// The peer machine's listen port, when it differs from ours.
    #[arg(long)]
    peer_port: Option<u16>,
    #[arg(
        long,
        default_value = "d9gotatun0",
        value_parser = crate::util::validate_interface_name
    )]
    interface: String,
    #[arg(long, default_value = "10.0.1.1")]
    tunnel_address: Ipv4Addr,
    #[arg(long, default_value = "10.0.1.2")]
    peer_tunnel_address: Ipv4Addr,
    #[arg(long, default_value_t = 1420)]
    mtu: u16,
    /// Directory the profiled tunnel writes dial9 trace segments to.
    #[arg(long, default_value = "dial9-traces")]
    trace_dir: std::path::PathBuf,
    #[arg(long, default_value_t = 999, value_parser = clap::value_parser!(u64).range(1..=4000))]
    cpu_sample_hz: u64,
    /// Seconds per iperf3 direction (send and receive each).
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..=60))]
    iperf_seconds: u16,
    /// Port of the iperf3 server started on the peer.
    #[arg(long, default_value_t = 5209)]
    iperf_port: u16,
}

/// The ephemeral key material of one run; held in memory only.
struct KeyMaterial {
    alice_private_hex: String,
    bob_private_hex: String,
    alice_public: PublicKey,
    bob_public: PublicKey,
    preshared_key_hex: String,
}

#[tokio::main]
pub async fn run(mut args: RunArgs) -> Result<()> {
    // The workflow passes the BENCHY_* variables from GitHub vars, which arrive empty when
    // they are not visible to the repository. Empty must not beat the defaults - the
    // throughput benchmark's env_value filters them the same way.
    override_from_env(&mut args)?;
    // Benchmark and profiling runs share the controller host and the peer machine.
    let _machine_lock = MachineLock::acquire("/tmp/benchy.lock")?;

    let keys = generate_keys();
    let executable = std::env::current_exe().context("failed to locate the harness executable")?;
    let run_id = std::env::var("GITHUB_RUN_ID").unwrap_or_else(|_| std::process::id().to_string());
    let remote_dir = format!("/tmp/{REMOTE_DIR_PREFIX}-{run_id}");
    let remote_executable = format!("{remote_dir}/gotatun-dial9");

    tokio::fs::create_dir_all(&args.trace_dir).await?;
    if tokio::fs::read_dir(&args.trace_dir)
        .await?
        .next_entry()
        .await?
        .is_some()
    {
        bail!(
            "trace directory must be empty: {}",
            args.trace_dir.display()
        );
    }
    let manifest_path = args.trace_dir.join("run.json");
    let sha = std::env::var("GITHUB_SHA").ok().or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    });
    let mut manifest = serde_json::json!({
        "schema_version": 1, "source_sha": sha, "run_id": std::env::var("GITHUB_RUN_ID").ok(),
        "run_attempt": std::env::var("GITHUB_RUN_ATTEMPT").ok(), "status": "running",
        "profiled_host": "alice", "directions": ["send", "receive"],
        "seconds_per_direction": args.iperf_seconds, "cpu_sample_hz": args.cpu_sample_hz,
        "mtu": args.mtu, "aead_backend": "ring", "trace_limit_mib": 512
    });
    tokio::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?).await?;
    let outcome = async {
        deploy(&args.peer, &executable, &remote_dir, &remote_executable).await?;
        orchestrate(&args, &keys, &remote_executable).await?;
        verify_trace_files(&args.trace_dir).await
    }
    .await;
    cleanup(&args.peer, &remote_dir).await;
    manifest["status"] = serde_json::json!(if outcome.is_ok() { "success" } else { "failed" });
    tokio::fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?).await?;
    outcome
}

fn override_from_env(args: &mut RunArgs) -> Result<()> {
    let value = |key| std::env::var(key).ok().filter(|value| !value.is_empty());
    if let Some(peer) = value("BENCHY_PEER") {
        args.peer = peer;
    }
    for (key, destination) in [
        ("BENCHY_ALICE_ADDRESS", &mut args.alice_address),
        ("BENCHY_BOB_ADDRESS", &mut args.bob_address),
    ] {
        if let Some(address) = value(key) {
            let address: IpAddr = address.parse().with_context(|| format!("invalid {key}"))?;
            if address.is_unspecified() {
                bail!("{key} must be a concrete host address");
            }
            *destination = address;
        }
    }
    if let Some(port) = value("BENCHY_WIREGUARD_PORT") {
        args.listen_port = port.parse().context("invalid BENCHY_WIREGUARD_PORT")?;
        if args.listen_port == 0 {
            bail!("BENCHY_WIREGUARD_PORT must be nonzero");
        }
    }
    Ok(())
}

fn generate_keys() -> KeyMaterial {
    let alice_private = StaticSecret::from(rand::random::<[u8; 32]>());
    let bob_private = StaticSecret::from(rand::random::<[u8; 32]>());
    let alice_public = PublicKey::from(&alice_private);
    let bob_public = PublicKey::from(&bob_private);
    KeyMaterial {
        alice_private_hex: hex::encode(alice_private.to_bytes()),
        bob_private_hex: hex::encode(bob_private.to_bytes()),
        alice_public,
        bob_public,
        preshared_key_hex: hex::encode(rand::random::<[u8; 32]>()),
    }
}

async fn orchestrate(args: &RunArgs, keys: &KeyMaterial, remote_executable: &str) -> Result<()> {
    let peer_port = args.peer_port.unwrap_or(args.listen_port);
    let mut processes: Vec<OwnedProcess> = Vec::new();
    let run = async {
        let remote = spawn_remote_endpoint(
            &args.peer,
            remote_executable,
            &endpoint_values(
                args,
                &keys.bob_private_hex,
                &keys.alice_public,
                &keys.preshared_key_hex,
                peer_port,
            ),
        )
        .await?;
        processes.push(OwnedProcess::new(remote, Some(&args.peer)));
        let remote = processes.last_mut().unwrap();
        remote.pid = Some(
            wait_ready(&mut remote.child, "DIAL9_ENDPOINT_READY")
                .await
                .context("remote endpoint failed to start")?,
        );

        let server =
            spawn_remote_iperf_server(&args.peer, args.peer_tunnel_address, args.iperf_port)?;
        processes.push(OwnedProcess::new(server, Some(&args.peer)));
        let server = processes.last_mut().unwrap();
        server.pid = Some(wait_ready(&mut server.child, "DIAL9_IPERF_PID").await?);
        wait_iperf_ready(server, args.peer_tunnel_address, args.iperf_port).await?;

        let tunnel = spawn_local_tunnel(&tunnel_values(args, keys, peer_port))?;
        processes.push(OwnedProcess::new(tunnel, None));
        let tunnel = processes.last_mut().unwrap();
        tunnel.pid = Some(
            wait_ready(&mut tunnel.child, "DIAL9_TUNNEL_READY")
                .await
                .context("profiled tunnel failed to start")?,
        );

        let status = tunnel
            .child
            .wait()
            .await
            .context("failed to wait for the profiled tunnel")?;
        if !status.success() {
            bail!("the profiled tunnel exited with {status}");
        }
        Ok(())
    };
    let outcome = tokio::select! {
        result = run => result,
        signal = tokio::signal::ctrl_c() => {
            match signal {
                Ok(()) => Err(eyre::eyre!("profiling interrupted")),
                Err(error) => Err(error).context("failed to listen for Ctrl-C"),
            }
        }
    };
    let mut cleanup_error = None;
    for process in processes.iter_mut().rev() {
        if let Err(error) = process.stop().await {
            eprintln!("process cleanup failed: {error:#}");
            cleanup_error = Some(error);
        }
    }
    outcome?;
    if let Some(error) = cleanup_error {
        return Err(error);
    }
    println!(
        "profiling run finished; traces in {}; owned processes stopped",
        args.trace_dir.display()
    );
    Ok(())
}

fn endpoint_values(
    args: &RunArgs,
    bob_private_hex: &str,
    alice_public: &PublicKey,
    preshared_key_hex: &str,
    peer_port: u16,
) -> Vec<String> {
    [
        "--interface".to_owned(),
        args.interface.clone(),
        "--private-key".to_owned(),
        bob_private_hex.to_owned(),
        "--peer-public-key".to_owned(),
        hex::encode(alice_public.as_bytes()),
        "--preshared-key".to_owned(),
        preshared_key_hex.to_owned(),
        "--peer-endpoint".to_owned(),
        format!("{}:{}", args.alice_address, args.listen_port),
        "--tunnel-address".to_owned(),
        args.peer_tunnel_address.to_string(),
        "--peer-tunnel-address".to_owned(),
        args.tunnel_address.to_string(),
        "--listen-port".to_owned(),
        peer_port.to_string(),
        "--mtu".to_owned(),
        args.mtu.to_string(),
        "--lease-seconds".to_owned(),
        (u64::from(args.iperf_seconds) * 2 + 90).to_string(),
    ]
    .to_vec()
}

fn tunnel_values(args: &RunArgs, keys: &KeyMaterial, peer_port: u16) -> Vec<String> {
    [
        "--interface".to_owned(),
        args.interface.clone(),
        "--private-key".to_owned(),
        keys.alice_private_hex.clone(),
        "--peer-public-key".to_owned(),
        hex::encode(keys.bob_public.as_bytes()),
        "--preshared-key".to_owned(),
        keys.preshared_key_hex.clone(),
        "--peer-endpoint".to_owned(),
        format!("{}:{}", args.bob_address, peer_port),
        "--tunnel-address".to_owned(),
        args.tunnel_address.to_string(),
        "--peer-tunnel-address".to_owned(),
        args.peer_tunnel_address.to_string(),
        "--listen-port".to_owned(),
        args.listen_port.to_string(),
        "--mtu".to_owned(),
        args.mtu.to_string(),
        "--trace-dir".to_owned(),
        args.trace_dir.to_string_lossy().into_owned(),
        "--cpu-sample-hz".to_owned(),
        args.cpu_sample_hz.to_string(),
        "--iperf-seconds".to_owned(),
        args.iperf_seconds.to_string(),
        "--iperf-port".to_owned(),
        args.iperf_port.to_string(),
    ]
    .to_vec()
}

async fn spawn_remote_endpoint(
    peer: &str,
    remote_executable: &str,
    values: &[String],
) -> Result<Child> {
    let command = remote_command_of(remote_executable, "endpoint", values);
    let remote = format!("sudo -n {}", command);
    Command::new("ssh")
        .arg(peer)
        .arg(remote)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start the remote endpoint on {peer}"))
}

fn spawn_local_tunnel(values: &[String]) -> Result<Child> {
    let mut command = Command::new("sudo");
    command
        .arg("-n")
        .arg(std::env::current_exe()?)
        .arg("tunnel");
    for value in values {
        command.arg(value);
    }
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start the profiled tunnel through sudo")?;
    Ok(child)
}

fn remote_command_of(executable: &str, subcommand: &str, values: &[String]) -> String {
    std::iter::once(format!("{executable} {subcommand}"))
        .chain(values.iter().map(|v| shell_quote(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn wait_ready(child: &mut Child, marker: &str) -> Result<u32> {
    let stdout = child
        .stdout
        .take()
        .ok_or(eyre::eyre!("child stdout was not captured"))?;
    let mut lines = BufReader::new(stdout).lines();
    let pid = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await? {
            if let Some(pid) = line.strip_prefix(&format!("{marker} ")) {
                return pid.parse::<u32>().map_err(std::io::Error::other);
            }
        }
        Err(std::io::Error::other(
            "process exited before becoming ready",
        ))
    })
    .await
    .context("timed out waiting for readiness")?
    .context("failed to read readiness")?;
    // Keep draining the pipe: dropping the reader closes the child's stdout, and its later
    // println!s would panic on EPIPE.
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            println!("{line}");
        }
    });
    Ok(pid)
}

/// Echo the exec'd process's PID so cleanup targets this run's server, never an unrelated one.
fn spawn_remote_iperf_server(
    peer: &str,
    bind: Ipv4Addr,
    port: u16,
) -> Result<tokio::process::Child> {
    let command = format!(
        "sh -c {}",
        shell_quote(&format!(
            "echo DIAL9_IPERF_PID $$; exec iperf3 --server --bind {bind} --port {port}"
        ))
    );
    let child = Command::new("ssh")
        .arg(peer)
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start the iperf3 server on {peer}"))?;
    Ok(child)
}

async fn wait_iperf_ready(process: &mut OwnedProcess, bind: Ipv4Addr, port: u16) -> Result<()> {
    for _ in 0..20 {
        if let Some(status) = process.child.try_wait()? {
            bail!("our iperf server exited before listening: {status}");
        }
        let output = checked_output(
            "ssh",
            [
                process.peer.clone().unwrap(),
                format!("ss -H -ltnp 'sport = :{port}'"),
            ],
        )
        .await?;
        let listeners = String::from_utf8_lossy(&output.stdout);
        if listener_is_owned(&listeners, bind, port, process.pid.unwrap()) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!("our iperf server did not bind {bind}:{port}; refusing to use another listener")
}

fn listener_is_owned(output: &str, bind: Ipv4Addr, port: u16, pid: u32) -> bool {
    output.lines().any(|line| {
        line.split_whitespace()
            .any(|field| field == format!("{bind}:{port}"))
            && line.contains(&format!("pid={pid},"))
    })
}

struct OwnedProcess {
    child: Child,
    peer: Option<String>,
    pid: Option<u32>,
}

impl OwnedProcess {
    fn new(child: Child, peer: Option<&str>) -> Self {
        Self {
            child,
            peer: peer.map(str::to_owned),
            pid: None,
        }
    }

    async fn signal(&self, signal: &str) -> Result<()> {
        let pid = self
            .pid
            .or_else(|| self.peer.is_none().then(|| self.child.id()).flatten());
        if let Some(pid) = pid {
            let values = [
                "-n".to_owned(),
                "kill".to_owned(),
                signal.to_owned(),
                pid.to_string(),
            ];
            let mut command = if let Some(peer) = &self.peer {
                let mut command = Command::new("ssh");
                command.arg(peer).arg(format!(
                    "sudo {}",
                    values
                        .iter()
                        .map(|s| shell_quote(s))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
                command
            } else {
                let mut command = Command::new("sudo");
                command.args(values);
                command
            };
            let _ = command.output().await?;
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        self.signal("-INT").await?;
        match tokio::time::timeout(std::time::Duration::from_secs(8), self.child.wait()).await {
            Ok(status) => {
                status?;
            }
            Err(_) => {
                self.signal("-KILL").await?;
                self.child.kill().await?;
                bail!("owned process did not stop gracefully");
            }
        }
        Ok(())
    }
}

async fn verify_trace_files(directory: &Path) -> Result<()> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut indices = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(index) = name
            .strip_prefix("trace.")
            .and_then(|s| s.strip_suffix(".bin.gz").or_else(|| s.strip_suffix(".bin")))
        {
            indices.push(
                index
                    .parse::<usize>()
                    .context("invalid trace segment name")?,
            );
        }
    }
    indices.sort_unstable();
    if indices.is_empty() || indices.iter().copied().ne(0..indices.len()) {
        bail!(
            "trace segments are missing or were rolled out of the buffer: {indices:?}; lower event volume or shorten the run"
        );
    }
    Ok(())
}

async fn deploy(
    peer: &str,
    executable: &Path,
    remote_dir: &str,
    remote_executable: &str,
) -> Result<()> {
    checked_output(
        "ssh",
        [
            peer.to_owned(),
            format!("mkdir -p {}", shell_quote(remote_dir)),
        ],
    )
    .await?;
    checked_output(
        "scp",
        [
            executable.as_os_str().to_string_lossy().into_owned(),
            format!("{peer}:{remote_executable}"),
        ],
    )
    .await?;
    checked_output(
        "ssh",
        [
            peer.to_owned(),
            format!("chmod 755 {}", shell_quote(remote_executable)),
        ],
    )
    .await?;
    Ok(())
}

async fn cleanup(peer: &str, remote_dir: &str) {
    let _ = Command::new("ssh")
        .arg(peer)
        .arg(format!("rm -rf {}", shell_quote(remote_dir)))
        .output()
        .await;
}

async fn checked_output(program: &str, args: [String; 2]) -> Result<std::process::Output> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{listener_is_owned, remote_command_of, shell_quote, verify_trace_files};

    #[test]
    fn shell_arguments_are_single_quoted() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn remote_commands_carry_the_subcommand_exactly_once() {
        let values = ["--interface".to_owned(), "d9gotatunb".to_owned()];
        let command = remote_command_of("/tmp/remote/gotatun-dial9", "endpoint", &values);
        assert_eq!(
            command,
            "/tmp/remote/gotatun-dial9 endpoint '--interface' 'd9gotatunb'"
        );
    }

    #[test]
    fn readiness_requires_our_process_and_bound_address() {
        let line = "LISTEN 0 4096 10.0.1.2:5209 0.0.0.0:* users:((\"iperf3\",pid=1234,fd=3))";
        assert!(listener_is_owned(
            line,
            "10.0.1.2".parse().unwrap(),
            5209,
            1234
        ));
        assert!(!listener_is_owned(
            line,
            "10.0.1.2".parse().unwrap(),
            5209,
            123
        ));
        assert!(!listener_is_owned(
            line,
            "10.0.1.1".parse().unwrap(),
            5209,
            1234
        ));
    }

    #[tokio::test]
    async fn rejects_traces_with_rolled_out_or_missing_segments() {
        let dir = std::env::temp_dir().join(format!("dial9-segments-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        assert!(verify_trace_files(&dir).await.is_err());
        tokio::fs::write(dir.join("trace.1.bin.gz"), b"test")
            .await
            .unwrap();
        assert!(verify_trace_files(&dir).await.is_err());
        tokio::fs::write(dir.join("trace.0.bin.gz"), b"test")
            .await
            .unwrap();
        assert!(verify_trace_files(&dir).await.is_ok());
        tokio::fs::write(dir.join("trace.3.bin.gz"), b"test")
            .await
            .unwrap();
        assert!(verify_trace_files(&dir).await.is_err());
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }
}
