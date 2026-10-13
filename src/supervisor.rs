//! A tiny process supervisor.
//!
//! `run` stays in the foreground, spawns one child per program, restarts
//! children that exit unexpectedly and tears everything down on SIGINT/SIGTERM.
//! State lives in `<data>/supervisor.pid` and `<data>/<program>/node.pid` so
//! `status` / `stop` work from another shell. A pid file is only trusted if
//! the process it names is still the program that wrote it (checked through
//! /proc/<pid>/cmdline), so stale files after a crash or reboot are harmless.

use crate::config::expand_env;
use anyhow::{Context, Result, bail};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// One managed process, persisted to `<data>/programs.json` by `init`.
/// `env` values may contain `$VAR`s; they are expanded when the process is
/// spawned, so secrets never land in programs.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Program {
    pub name: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub workdir: PathBuf,
    pub log: PathBuf,
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

pub fn load_programs(data: &Path) -> Result<Vec<Program>> {
    let path = data.join("programs.json");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("{} not found, run `midnight-starter init` first", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

fn pid_file(data: &Path, name: &str) -> PathBuf {
    data.join(name).join("node.pid")
}

fn supervisor_pid_file(data: &Path) -> PathBuf {
    data.join("supervisor.pid")
}

fn exe_name(cmd: &str) -> String {
    Path::new(cmd).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
}

fn self_name() -> String {
    std::env::current_exe().map(|p| exe_name(&p.to_string_lossy())).unwrap_or_else(|_| "midnight-starter".into())
}

/// The pid in `path`, if that process is alive and its argv[0] is `exe`.
fn read_pid(path: &Path, exe: &str) -> Option<Pid> {
    let pid: i32 = fs::read_to_string(path).ok()?.trim().parse().ok()?;
    let pid = Pid::from_raw(pid);
    signal::kill(pid, None).ok()?; // signal 0 = existence check
    match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) => {
            let argv0 = cmdline.split(|b| *b == 0).next().unwrap_or_default();
            (exe_name(&String::from_utf8_lossy(argv0)) == exe).then_some(pid)
        }
        Err(_) => Some(pid), // no /proc: fall back to the existence check
    }
}

fn spawn(p: &Program) -> Result<Child> {
    fs::create_dir_all(p.log.parent().unwrap())?;
    let log = OpenOptions::new().create(true).append(true).open(&p.log)?;
    let mut env = BTreeMap::new();
    for (k, v) in &p.env {
        env.insert(k, expand_env(v).with_context(|| format!("{}: env {k}", p.name))?);
    }
    let child = Command::new(&p.cmd)
        .args(&p.args)
        .envs(env)
        .current_dir(&p.workdir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("failed to spawn {} ({})", p.name, p.cmd))?;
    Ok(child)
}

struct Slot<'a> {
    program: &'a Program,
    /// None once we gave up on it (crash loop).
    child: Option<Child>,
    started: Instant,
    quick_exits: u32,
}

/// Foreground supervisor loop.
pub fn run(data: &Path, programs: &[Program], only: &[String]) -> Result<()> {
    if let Some(pid) = read_pid(&supervisor_pid_file(data), &self_name()) {
        bail!("supervisor already running (pid {pid}), use `midnight-starter stop` first");
    }
    if let Some((name, pid)) = live_nodes(data, programs).into_iter().next() {
        bail!("{name} (pid {pid}) is still running without a supervisor, use `midnight-starter stop` first");
    }
    let programs: Vec<&Program> = programs
        .iter()
        .filter(|p| only.is_empty() || only.contains(&p.name))
        .collect();
    if programs.is_empty() {
        bail!("no programs selected");
    }

    unsafe {
        let handler = signal::SigHandler::Handler(on_signal);
        signal::signal(Signal::SIGINT, handler)?;
        signal::signal(Signal::SIGTERM, handler)?;
    }
    fs::write(supervisor_pid_file(data), std::process::id().to_string())?;

    // whatever happens inside, never leave nodes behind
    let mut slots = Vec::new();
    let result = supervise(data, &programs, &mut slots);
    teardown(data, &mut slots);
    let _ = fs::remove_file(supervisor_pid_file(data));
    result
}

fn supervise<'a>(data: &Path, programs: &[&'a Program], slots: &mut Vec<Slot<'a>>) -> Result<()> {
    for p in programs {
        let child = spawn(p)?;
        fs::write(pid_file(data, &p.name), child.id().to_string())?;
        println!("started {:<10} pid {:<8} log {}", p.name, child.id(), p.log.display());
        slots.push(Slot { program: p, child: Some(child), started: Instant::now(), quick_exits: 0 });
    }

    while !SHUTDOWN.load(Ordering::SeqCst) {
        for s in slots.iter_mut() {
            let Some(child) = s.child.as_mut() else { continue };
            let Some(status) = child.try_wait()? else { continue };
            let p = s.program;
            let _ = fs::remove_file(pid_file(data, &p.name));
            s.child = None;
            if SHUTDOWN.load(Ordering::SeqCst) {
                break;
            }
            // simple crash-loop protection: back off when a node dies quickly
            let quick = s.started.elapsed() < Duration::from_secs(10);
            s.quick_exits = if quick { s.quick_exits + 1 } else { 0 };
            if s.quick_exits > 5 {
                eprintln!("{} keeps exiting ({status}), giving up; see {}", p.name, p.log.display());
                continue;
            }
            eprintln!("{} exited ({status}), restarting", p.name);
            std::thread::sleep(Duration::from_secs(if quick { 3 } else { 1 }));
            let child = spawn(p)?;
            fs::write(pid_file(data, &p.name), child.id().to_string())?;
            s.started = Instant::now();
            s.child = Some(child);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("shutting down");
    Ok(())
}

/// SIGTERM every live child, SIGKILL stragglers, clear pid files.
fn teardown(data: &Path, slots: &mut [Slot]) {
    for s in slots.iter() {
        if let Some(c) = &s.child {
            let _ = signal::kill(Pid::from_raw(c.id() as i32), Signal::SIGTERM);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    for s in slots.iter_mut() {
        if let Some(child) = s.child.as_mut() {
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = fs::remove_file(pid_file(data, &s.program.name));
    }
}

/// Re-exec ourselves as a detached supervisor (`start --detach`) and wait
/// until it has either taken over (written its pid file) or failed.
pub fn detach(data: &Path, args: &[String]) -> Result<()> {
    let log_path = data.join("supervisor.log");
    let log = OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut child = Command::new(std::env::current_exe()?)
        .args(args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            let tail = fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = tail.lines().rev().take(5).collect();
            bail!(
                "background supervisor exited ({status}) before starting:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        let written = fs::read_to_string(supervisor_pid_file(data)).ok().and_then(|s| s.trim().parse::<u32>().ok());
        if written == Some(pid) {
            break;
        }
        if Instant::now() > deadline {
            bail!("background supervisor (pid {pid}) did not start within 10s; see {}", log_path.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("supervisor started in background (pid {pid}), log {}", log_path.display());
    Ok(())
}

/// Nodes whose pid file names a live midnight-node process.
fn live_nodes(data: &Path, programs: &[Program]) -> Vec<(String, Pid)> {
    programs
        .iter()
        .filter_map(|p| read_pid(&pid_file(data, &p.name), &exe_name(&p.cmd)).map(|pid| (p.name.clone(), pid)))
        .collect()
}

pub fn status(data: &Path, programs: &[Program]) {
    match read_pid(&supervisor_pid_file(data), &self_name()) {
        Some(pid) => println!("supervisor   RUNNING  pid {pid}"),
        None => println!("supervisor   STOPPED"),
    }
    for p in programs {
        match read_pid(&pid_file(data, &p.name), &exe_name(&p.cmd)) {
            Some(pid) => println!("{:<12} RUNNING  pid {pid}", p.name),
            None => println!("{:<12} STOPPED", p.name),
        }
    }
}

/// Stop the supervisor if it is alive; otherwise stop any nodes it left behind.
pub fn stop(data: &Path) -> Result<()> {
    let sup = supervisor_pid_file(data);
    if let Some(pid) = read_pid(&sup, &self_name()) {
        signal::kill(pid, Signal::SIGTERM)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while read_pid(&sup, &self_name()).is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
        if read_pid(&sup, &self_name()).is_some() {
            bail!("supervisor (pid {pid}) did not exit within 30s");
        }
        println!("stopped");
        return Ok(());
    }
    let _ = fs::remove_file(&sup);
    let programs = load_programs(data).unwrap_or_default();
    let orphans = live_nodes(data, &programs);
    if orphans.is_empty() {
        println!("supervisor not running");
        return Ok(());
    }
    for (name, pid) in &orphans {
        println!("stopping orphaned {name} (pid {pid})");
        let _ = signal::kill(*pid, Signal::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while !live_nodes(data, &programs).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    for (name, pid) in live_nodes(data, &programs) {
        println!("killing {name} (pid {pid})");
        let _ = signal::kill(pid, Signal::SIGKILL);
    }
    for (name, _) in &orphans {
        let _ = fs::remove_file(pid_file(data, name));
    }
    println!("stopped");
    Ok(())
}

/// True if the supervisor or any node of this chain is alive.
pub fn is_running(data: &Path) -> bool {
    read_pid(&supervisor_pid_file(data), &self_name()).is_some()
        || !live_nodes(data, &load_programs(data).unwrap_or_default()).is_empty()
}
