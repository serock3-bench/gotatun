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
    #[arg(long, default_value_t = 100)]
    ping_count: u16,
    #[arg(long, default_value_t = 10)]
    ping_interval_ms: u16,
    #[arg(long, default_value_t = 56)]
    ping_payload_bytes: u16,
    #[arg(long, default_value_t = 999)]
    cpu_sample_hz: u64,
    /// Workload to record after the tunnel comes up.
    #[arg(long, value_enum, default_value_t = crate::tunnel::Workload::Ping)]
    workload: crate::tunnel::Workload,
    /// Seconds per iperf3 direction (send and receive each).
    #[arg(long, default_value_t = 10)]
    iperf_seconds: u16,
    /// Port of the iperf3 server started on the peer.
    #[arg(long, default_value_t = 5201)]
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
    override_from_env(&mut args);
    // Benchmark and profiling runs share the controller host and the peer machine.
    let _machine_lock = MachineLock::acquire("/tmp/benchy.lock")?;

    let keys = generate_keys();
    let executable = std::env::current_exe().context("failed to locate the harness executable")?;
    let run_id = std::env::var("GITHUB_RUN_ID").unwrap_or_else(|_| std::process::id().to_string());
    let remote_dir = format!("/tmp/{REMOTE_DIR_PREFIX}-{run_id}");
    let remote_executable = format!("{remote_dir}/gotatun-dial9");

    deploy(&args.peer, &executable, &remote_dir, &remote_executable).await?;
    let outcome = orchestrate(&args, &keys, &remote_executable, &remote_dir).await;
    cleanup(&args.peer, &remote_dir).await;
    outcome
}

fn override_from_env(args: &mut RunArgs) {
    if let Ok(peer) = std::env::var("BENCHY_PEER") {
        if !peer.is_empty() {
            args.peer = peer;
        }
    }
    if let Ok(address) = std::env::var("BENCHY_ALICE_ADDRESS") {
        if let Ok(address) = address.parse::<IpAddr>() {
            if !address.is_unspecified() {
                args.alice_address = address;
            }
        }
    }
    if let Ok(address) = std::env::var("BENCHY_BOB_ADDRESS") {
        if let Ok(address) = address.parse::<IpAddr>() {
            if !address.is_unspecified() {
                args.bob_address = address;
            }
        }
    }
    if let Ok(port) = std::env::var("BENCHY_WIREGUARD_PORT") {
        if let Ok(port) = port.parse::<u16>() {
            if port != 0 {
                args.listen_port = port;
            }
        }
    }
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

async fn orchestrate(
    args: &RunArgs,
    keys: &KeyMaterial,
    remote_executable: &str,
    remote_dir: &str,
) -> Result<()> {
    let peer_port = args.peer_port.unwrap_or(args.listen_port);

    let mut remote = spawn_remote_endpoint(
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
    let remote_pid = wait_ready(&mut remote, "DIAL9_ENDPOINT_READY")
        .await
        .context("remote endpoint failed to start")?;

    let iperf = match args.workload {
        crate::tunnel::Workload::Ping => None,
        crate::tunnel::Workload::Iperf => {
            let server =
                spawn_remote_iperf_server(&args.peer, args.peer_tunnel_address, args.iperf_port)
                    .await?;
            Some(server)
        }
    };

    let tunnel = spawn_local_tunnel(&tunnel_values(args, keys, peer_port))?;
    // The tunnel is a root child; the controller only shepherds it.
    let mut tunnel = match tunnel {
        Some(child) => child,
        None => bail!("failed to spawn the profiled tunnel"),
    };
    wait_ready(&mut tunnel, "DIAL9_TUNNEL_READY")
        .await
        .context("profiled tunnel failed to start")?;

    let status = tunnel
        .wait()
        .await
        .context("failed to wait for the profiled tunnel")?;
    if let Some(mut iperf) = iperf {
        let _ = iperf.start_kill();
    }
    stop_remote_endpoint(&args.peer, remote_pid).await;

    if !status.success() {
        bail!("the profiled tunnel exited with {status}");
    }
    println!(
        "profiling run finished; traces in {}; peer cleanup done in {remote_dir}",
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
        "--ping-count".to_owned(),
        args.ping_count.to_string(),
        "--ping-interval-ms".to_owned(),
        args.ping_interval_ms.to_string(),
        "--ping-payload-bytes".to_owned(),
        args.ping_payload_bytes.to_string(),
        "--cpu-sample-hz".to_owned(),
        args.cpu_sample_hz.to_string(),
        "--workload".to_owned(),
        format!("{:?}", args.workload).to_lowercase(),
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

fn spawn_local_tunnel(values: &[String]) -> Result<Option<Child>> {
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
    Ok(Some(child))
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
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await? {
            if let Some(pid) = line.strip_prefix(&format!("{marker} ")) {
                return Ok::<u32, std::io::Error>(pid.parse().expect("valid pid"));
            }
        }
        Err(std::io::Error::other(
            "process exited before becoming ready",
        ))
    })
    .await
    .context("timed out waiting for readiness")?
    .context("failed to read readiness")
}

/// Starts the iperf3 server on the peer, bound to its tunnel address (which exists once the
/// endpoint is up). The server is tied to this ssh child: aborting it tears the ssh session
/// down, which closes the remote iperf3.
async fn spawn_remote_iperf_server(
    peer: &str,
    bind: Ipv4Addr,
    port: u16,
) -> Result<tokio::process::Child> {
    let command = format!("iperf3 --server --bind {bind} --port {port}");
    let child = Command::new("ssh")
        .arg(peer)
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to start the iperf3 server on {peer}"))?;
    // Give the server a moment to bind before the client races it.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(child)
}

async fn stop_remote_endpoint(peer: &str, pid: u32) {
    let command = remote_command_of("sudo", "kill", &["-INT".to_owned(), pid.to_string()]);
    let _ = Command::new("ssh").arg(peer).arg(command).output().await;
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
    use super::{remote_command_of, shell_quote};

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
}
