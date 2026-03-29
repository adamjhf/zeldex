use std::env;
use std::net::TcpListener;
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use zeldex::native::{now_unix_seconds, remove_runtime, write_runtime, RuntimeRecord};

fn main() -> Result<()> {
    let forwarded_args = env::args_os().skip(1).collect::<Vec<_>>();
    let port = reserve_port()?;
    let ws_url = format!("ws://127.0.0.1:{port}");

    let mut app_server = Command::new("codex")
        .arg("app-server")
        .arg("--listen")
        .arg(&ws_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn codex app-server")?;

    let status = run_codex(&forwarded_args, &ws_url, app_server.id(), port);
    cleanup_child(&mut app_server);
    let status = status?;

    if let Some(code) = status.code() {
        process::exit(code);
    }
    bail!("codex terminated without an exit code")
}

fn run_codex(
    forwarded_args: &[std::ffi::OsString],
    ws_url: &str,
    app_server_pid: u32,
    port: u16,
) -> Result<ExitStatus> {
    let cwd = env::current_dir()
        .context("get current directory")?
        .display()
        .to_string();
    wait_for_listener(port)?;

    let mut codex = Command::new("codex")
        .arg("--remote")
        .arg(ws_url)
        .args(forwarded_args)
        .spawn()
        .context("spawn remote codex")?;

    let pid = codex.id();
    let runtime = RuntimeRecord {
        pid,
        ws_url: ws_url.to_owned(),
        cwd,
        started_at: now_unix_seconds(),
        app_server_pid,
    };
    if let Err(error) = write_runtime(&runtime) {
        cleanup_runtime_process(pid, &mut codex);
        return Err(error);
    }

    let status = codex.wait().context("wait for codex");
    remove_runtime(pid);
    status
}

fn cleanup_runtime_process(pid: u32, child: &mut Child) {
    remove_runtime(pid);
    cleanup_child(child);
}

fn cleanup_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn reserve_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_listener(port: u16) -> Result<()> {
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for app-server on port {port}")
}
