//! The profiled near-side tunnel, run as root by `gotatun-dial9 run` through sudo. Everything
//! below the recorder setup runs inside the dial9-instrumented Tokio runtime: the gotatun
//! device, the TUN interface, the ping workload, and the teardown.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};

use clap::Args as ClapArgs;
use dial9::memory_profiling::{Dial9Allocator, MemoryProfilingConfig};
use dial9::telemetry::{
    CpuProfilingConfig, DiskBuffer, ProcessResourceUsageConfig, RecorderPerfExt, RecorderTokioExt,
    SchedEventConfig, TokioAttachOptions, block_on, recorder,
};
use dial9::tracing_layer::Dial9TracingLayer;
use eyre::{Context as _, Result, bail, eyre};
use gotatun::{
    device::{DeviceBuilder, Peer},
    x25519::{PublicKey, StaticSecret},
};
use ipnetwork::IpNetwork;
use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    process::Command,
    time::sleep,
};

use crate::{tracing_events::record_workload_event, util::decode_key};

/// dial9 wraps the global allocator so allocation sampling can capture stacks.
#[global_allocator]
static ALLOCATOR: Dial9Allocator = Dial9Allocator::system();

const SOCKET_BUFFER_SIZE: usize = 7 * 1024 * 1024;
const READY_MARKER: &str = "DIAL9_TUNNEL_READY";

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum Workload {
    /// ICMP echo requests through the tunnel (the original quiet workload).
    Ping,
    /// iperf3 through the tunnel, sending first and then receiving, for a line-rate
    /// workload that keeps the data path hot in the CPU samples.
    Iperf,
}

#[derive(Debug, ClapArgs)]
pub struct TunnelArgs {
    #[arg(long, default_value = "d9gotatun0", value_parser = crate::util::validate_interface_name)]
    interface: String,
    #[arg(long)]
    private_key: String,
    #[arg(long)]
    peer_public_key: String,
    #[arg(long)]
    preshared_key: String,
    #[arg(long)]
    peer_endpoint: SocketAddr,
    #[arg(long, default_value = "10.0.1.1")]
    tunnel_address: Ipv4Addr,
    #[arg(long, default_value = "10.0.1.2")]
    peer_tunnel_address: Ipv4Addr,
    #[arg(long, default_value_t = 51821)]
    listen_port: u16,
    #[arg(long, default_value_t = 1420)]
    mtu: u16,
    /// Directory where dial9 trace segments are written.
    #[arg(long, default_value = "dial9-traces")]
    trace_dir: PathBuf,
    #[arg(long, default_value_t = 100)]
    ping_count: u16,
    #[arg(long, default_value_t = 10)]
    ping_interval_ms: u16,
    #[arg(long, default_value_t = 56)]
    ping_payload_bytes: u16,
    #[arg(long, default_value_t = 999)]
    cpu_sample_hz: u64,
    /// Workload to record after the tunnel comes up.
    #[arg(long, value_enum, default_value_t = Workload::Ping)]
    workload: Workload,
    /// Seconds per iperf3 direction (send and receive each).
    #[arg(long, default_value_t = 10)]
    iperf_seconds: u16,
    /// Port of the iperf3 server on the peer.
    #[arg(long, default_value_t = 5201)]
    iperf_port: u16,
}

pub fn run(args: TunnelArgs) -> Result<()> {
    // SAFETY: geteuid has no preconditions and does not access memory.
    if unsafe { libc::geteuid() } != 0 {
        bail!(
            "gotatun-dial9 tunnel must run as root (CAP_NET_ADMIN is required); \
             only `gotatun-dial9 run` starts it, through sudo"
        );
    }
    init_tracing()?;

    let writer = DiskBuffer::builder()
        .base_path(&args.trace_dir)
        .max_total_size(128 * 1024 * 1024)
        .max_file_size(64 * 1024 * 1024)
        .rotation_period(Duration::from_secs(60))
        .build()
        .wrap_err("failed to create dial9 trace writer")?;

    let (recorder, runtime) = recorder(writer)
        .with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(args.cpu_sample_hz))
        .with_sched_events(SchedEventConfig::default())
        .with_memory_profiling(
            MemoryProfilingConfig::builder()
                .sample_rate_bytes(64 * 1024)
                .track_liveset(true)
                .build(),
        )
        .with_process_resource_usage(ProcessResourceUsageConfig::default())
        .build()
        .attach_tokio_runtime_with(
            TokioAttachOptions::builder()
                .runtime_name("gotatun-dial9")
                .task_tracking_enabled(true)
                .build(),
            |_| {},
        )
        .wrap_err("failed to create dial9-instrumented Tokio runtime")?;

    let result = block_on(&runtime, run_profiled(args));
    drop(runtime);
    recorder.graceful_shutdown(Duration::from_secs(5));
    result
}

fn init_tracing() -> Result<()> {
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("gotatun=trace,gotatun_dial9=debug,info"));
    // Logs go to stderr; stdout carries only the readiness marker and workload lines.
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(Dial9TracingLayer::new())
        .with(crate::tracing_events::TracingEventLayer)
        .try_init()
        .map_err(|error| eyre!("failed to install tracing subscriber: {error}"))
}

async fn run_profiled(args: TunnelArgs) -> Result<()> {
    record_workload_event("starting", "building tunnel and peer configuration");
    let private_key = StaticSecret::from(decode_key(&args.private_key)?);
    let peer_public_key = PublicKey::from(decode_key(&args.peer_public_key)?);
    let preshared_key = decode_key(&args.preshared_key)?;
    let allowed_ip: IpNetwork = format!("{}/32", args.peer_tunnel_address).parse()?;

    let peer = Peer::new(peer_public_key)
        .with_endpoint(args.peer_endpoint)
        .with_allowed_ip(allowed_ip)
        .with_preshared_key(preshared_key);

    let device = DeviceBuilder::new()
        .with_private_key(private_key)
        .with_peer(peer)
        .with_default_udp()
        .with_listen_port(args.listen_port)
        .udp_recv_buffer_size(SOCKET_BUFFER_SIZE)
        .udp_send_buffer_size(SOCKET_BUFFER_SIZE)
        .create_tun(&args.interface)
        .wrap_err("failed to create TUN device")?
        .build()
        .await
        .wrap_err("failed to build gotatun device")?;
    record_workload_event(
        "device_ready",
        format!("interface={}, mtu={}", args.interface, args.mtu),
    );

    configure_interface(
        &args.interface,
        args.tunnel_address,
        args.peer_tunnel_address,
        args.mtu,
    )
    .await?;
    println!("{READY_MARKER} {}", std::process::id());

    wait_for_tunnel(&args.interface, args.peer_tunnel_address).await?;
    let workload_result = match args.workload {
        Workload::Ping => {
            run_ping(
                &args.interface,
                args.peer_tunnel_address,
                args.ping_count,
                args.ping_interval_ms,
                args.ping_payload_bytes,
            )
            .await
        }
        Workload::Iperf => {
            run_iperf(
                args.peer_tunnel_address,
                args.iperf_seconds,
                args.iperf_port,
            )
            .await
        }
    };

    for peer in device.peers().await {
        tracing::info!(
            tx_bytes = peer.stats.tx_bytes,
            rx_bytes = peer.stats.rx_bytes,
            handshake = peer.stats.last_handshake.is_some(),
            last_handshake_ms = peer
                .stats
                .last_handshake
                .map(|duration| duration.as_millis() as u64),
            "gotatun peer statistics"
        );
    }

    device.stop().await;
    record_workload_event("shutdown", "gotatun device stopped");
    println!("tunnel finished; traces in {}", args.trace_dir.display());
    workload_result
}

/// Assigns the tunnel address and brings the interface up. A /32 plus an explicit route to
/// the peer keeps each side's routing precise, and lets both endpoints of a loopback pair
/// live on one kernel without overlapping subnets.
async fn configure_interface(
    interface: &str,
    address: Ipv4Addr,
    peer_address: Ipv4Addr,
    mtu: u16,
) -> Result<()> {
    run_ip(&[
        "address",
        "replace",
        &format!("{address}/32"),
        "dev",
        interface,
    ])
    .await?;
    run_ip(&[
        "link",
        "set",
        "dev",
        interface,
        "mtu",
        &mtu.to_string(),
        "up",
    ])
    .await?;
    run_ip(&[
        "route",
        "replace",
        &format!("{peer_address}/32"),
        "dev",
        interface,
    ])
    .await?;
    Ok(())
}

async fn run_ip(args: &[&str]) -> Result<()> {
    let output = Command::new("ip")
        .args(args)
        .output()
        .await
        .wrap_err("failed to execute ip")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "ip {} failed with {}: {}",
        args.join(" "),
        output.status,
        stderr.trim()
    );
}

/// The first ping of a fresh tunnel triggers the handshake and may drop; wait for the first
/// successful echo before the measured workload starts.
async fn wait_for_tunnel(interface: &str, target: Ipv4Addr) -> Result<()> {
    let target = target.to_string();
    for _ in 0..10 {
        let status = Command::new("/usr/bin/ping")
            .args(["-n", "-I", interface, "-c", "1", "-W", "1", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .wrap_err("failed to execute ping")?;
        if status.success() {
            record_workload_event("tunnel_ready", format!("target={target}"));
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
    }
    bail!("tunnel did not become ready");
}

async fn run_ping(
    interface: &str,
    target: Ipv4Addr,
    count: u16,
    interval_ms: u16,
    payload_bytes: u16,
) -> Result<()> {
    let interval = format!("{:.3}", f64::from(interval_ms) / 1_000.0);
    let count = count.to_string();
    let payload_bytes = payload_bytes.to_string();
    let target = target.to_string();
    record_workload_event(
        "ping_started",
        format!(
            "target={target}, count={count}, interval_ms={interval_ms}, \
             payload_bytes={payload_bytes}, interface={interface}"
        ),
    );

    let mut child = Command::new("/usr/bin/ping")
        .args([
            "-n",
            "-I",
            interface,
            "-c",
            &count,
            "-i",
            &interval,
            "-s",
            &payload_bytes,
            "-W",
            "3",
            &target,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .wrap_err("failed to start /usr/bin/ping")?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(log_lines("stdout", BufReader::new(stdout)));
    let stderr_task = tokio::spawn(log_lines("stderr", BufReader::new(stderr)));

    let mut interrupted = false;
    let status = {
        let wait = child.wait();
        tokio::pin!(wait);
        tokio::select! {
            status = &mut wait => Some(status.wrap_err("failed while waiting for ping")?),
            signal = tokio::signal::ctrl_c() => {
                signal.wrap_err("failed to listen for Ctrl-C")?;
                interrupted = true;
                None
            }
        }
    };

    // If Ctrl-C won the select, dropping a kill_on_drop child terminates ping and closes its pipes.
    drop(child);
    stdout_task.await.wrap_err("ping stdout task failed")??;
    stderr_task.await.wrap_err("ping stderr task failed")??;

    if interrupted {
        record_workload_event("ping_interrupted", "received Ctrl-C");
        bail!("ping interrupted by Ctrl-C");
    }

    let status = status.expect("status is present unless interrupted");
    record_workload_event("ping_finished", format!("status={status}"));
    if !status.success() {
        bail!("ping exited unsuccessfully with {status}");
    }
    Ok(())
}

/// Runs iperf3 through the tunnel in both directions, so one trace covers the send path
/// (this side encrypts, batched by the UDP layer) and the receive path (this side decrypts
/// and writes to the TUN). The matching server on the peer is the controller's job.
async fn run_iperf(target: Ipv4Addr, seconds: u16, port: u16) -> Result<()> {
    let seconds = seconds.to_string();
    let port = port.to_string();
    let target = target.to_string();
    for reverse in [false, true] {
        let direction = if reverse { "receive" } else { "send" };
        record_workload_event(
            "iperf_started",
            format!("target={target}, direction={direction}, seconds={seconds}"),
        );
        let mut args = vec![
            "-c".to_owned(),
            target.clone(),
            "-p".to_owned(),
            port.clone(),
            "-t".to_owned(),
            seconds.clone(),
        ];
        if reverse {
            args.push("-R".to_owned());
        }
        let mut child = Command::new("/usr/bin/iperf3")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .wrap_err("failed to start /usr/bin/iperf3")?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stdout_task = tokio::spawn(log_lines("stdout", BufReader::new(stdout)));
        let stderr_task = tokio::spawn(log_lines("stderr", BufReader::new(stderr)));

        let mut interrupted = false;
        let status = {
            let wait = child.wait();
            tokio::pin!(wait);
            tokio::select! {
                status = &mut wait => Some(status.wrap_err("failed while waiting for iperf3")?),
                signal = tokio::signal::ctrl_c() => {
                    signal.wrap_err("failed to listen for Ctrl-C")?;
                    interrupted = true;
                    None
                }
            }
        };

        drop(child);
        stdout_task.await.wrap_err("iperf3 stdout task failed")??;
        stderr_task.await.wrap_err("iperf3 stderr task failed")??;

        if interrupted {
            record_workload_event("iperf_interrupted", "received Ctrl-C");
            bail!("iperf3 interrupted by Ctrl-C");
        }
        let status = status.expect("status is present unless interrupted");
        record_workload_event(
            "iperf_finished",
            format!("direction={direction}, status={status}"),
        );
        if !status.success() {
            bail!("iperf3 ({direction}) exited unsuccessfully with {status}");
        }
    }
    Ok(())
}

async fn log_lines(
    stream: &'static str,
    reader: impl tokio::io::AsyncBufRead + Unpin,
) -> std::io::Result<()> {
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        tracing::info!(target: "gotatun_dial9::workload", stream, line, "workload output");
    }
    Ok(())
}
