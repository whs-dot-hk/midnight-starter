mod config;
mod init;
mod supervisor;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Local multi-validator devnets for Midnight (midnight-node).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Devnet description.
    #[arg(long, short, global = true, default_value = "devnet.yaml")]
    config: PathBuf,
    /// Root data directory; each chain lives in <data>/<chain>.
    #[arg(long, global = true, default_value = "./data")]
    data: PathBuf,
    /// Which chain from the config (needed only if it defines several).
    #[arg(long, global = true)]
    chain: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download and unpack the midnight-node release named by `version`.
    Fetch {
        /// Where to unpack; `cmd` should then point at <dir>/midnight-node.
        #[arg(long, default_value = "./bin")]
        dir: PathBuf,
    },
    /// Generate keys, genesis committee, chain spec and process list (wipes old data).
    Init {
        /// Reuse existing validator seeds and node keys (e.g. once registered on Cardano).
        #[arg(long)]
        keep_keys: bool,
    },
    /// Start every node under the built-in supervisor (foreground unless --detach).
    Start {
        #[arg(long)]
        detach: bool,
        /// Only these programs (e.g. node0 node2).
        only: Vec<String>,
    },
    /// init + start.
    Serve {
        #[arg(long)]
        detach: bool,
    },
    /// Stop the supervisor and all its nodes.
    Stop,
    /// Process status plus best/finalized block and peer count per node.
    Status,
    /// Print a node's log path, or tail it with -f.
    Logs {
        #[arg(default_value = "node0")]
        node: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// Send a raw JSON-RPC call to a node, e.g. `rpc system_health`.
    Rpc {
        method: String,
        /// JSON params array.
        #[arg(default_value = "[]")]
        params: String,
        #[arg(long, default_value_t = 0)]
        node: u16,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_dir = cli
        .config
        .canonicalize()
        .with_context(|| format!("{} not found", cli.config.display()))?
        .parent()
        .unwrap()
        .to_path_buf();
    let cfg = config::load(&cli.config)?;
    let (chain, ccfg) = match &cli.chain {
        Some(c) => (c.clone(), cfg.get(c).with_context(|| format!("chain {c} not in config"))?.clone()),
        None => {
            ensure!(cfg.len() == 1, "config defines {} chains, pick one with --chain", cfg.len());
            let (k, v) = cfg.into_iter().next().unwrap();
            (k, v)
        }
    };
    let home = cli.data.join(&chain);

    match &cli.cmd {
        Cmd::Fetch { dir } => fetch(&ccfg.version, dir),
        Cmd::Init { keep_keys } => {
            init::init(&chain, &ccfg, &config_dir, &cli.data, *keep_keys)?;
            println!("initialized {}", home.display());
            Ok(())
        }
        Cmd::Start { detach, only } => start(&cli, &chain, &home, *detach, only),
        Cmd::Serve { detach } => {
            init::init(&chain, &ccfg, &config_dir, &cli.data, false)?;
            start(&cli, &chain, &home, *detach, &[])
        }
        Cmd::Stop => supervisor::stop(&home),
        Cmd::Status => {
            let programs = supervisor::load_programs(&home)?;
            supervisor::status(&home, &programs);
            println!();
            for i in 0..programs.len() as u16 {
                let port = ccfg.ports.rpc + i;
                match chain_info(port) {
                    Ok(s) => println!("node{i}  rpc :{port}  {s}"),
                    Err(e) => println!("node{i}  rpc :{port}  unreachable ({e})"),
                }
            }
            Ok(())
        }
        Cmd::Logs { node, follow } => {
            let log = home.join(resolve_node(&home, node)?).join("node.log");
            ensure!(log.exists(), "{} does not exist yet", log.display());
            if *follow {
                Command::new("tail").arg("-F").arg(&log).status()?;
            } else {
                println!("{}", log.display());
            }
            Ok(())
        }
        Cmd::Rpc { method, params, node } => {
            let params: Value = serde_json::from_str(params).context("params must be a JSON array")?;
            let res = rpc(ccfg.ports.rpc + node, method, params)?;
            println!("{}", serde_json::to_string_pretty(&res)?);
            Ok(())
        }
    }
}

fn start(cli: &Cli, chain: &str, home: &Path, detach: bool, only: &[String]) -> Result<()> {
    let programs = supervisor::load_programs(home)?;
    if detach {
        // re-exec ourselves as a foreground `start` built from the parsed options
        // (init, if any, already happened in this process)
        let mut args = vec![
            "--config".to_string(),
            cli.config.display().to_string(),
            "--data".to_string(),
            cli.data.display().to_string(),
            "--chain".to_string(),
            chain.to_string(),
            "start".to_string(),
        ];
        args.extend(only.iter().cloned());
        return supervisor::detach(home, &args);
    }
    supervisor::run(home, &programs, only)
}

/// `node1`, `1`, or a validator name from the config (`alice`) -> `node1`.
fn resolve_node(home: &Path, node: &str) -> Result<String> {
    if let Ok(i) = node.trim_start_matches("node").parse::<u32>() {
        return Ok(format!("node{i}"));
    }
    let raw = std::fs::read_to_string(home.join("validators.json"))
        .with_context(|| format!("{} not initialized", home.display()))?;
    let names: Vec<Value> = serde_json::from_str(&raw)?;
    names
        .iter()
        .position(|v| v["name"] == node)
        .map(|i| format!("node{i}"))
        .with_context(|| format!("no node named {node}; see {}", home.join("validators.json").display()))
}

fn fetch(version: &str, dir: &Path) -> Result<()> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => bail!("no midnight-node release for {a}"),
    };
    ensure!(std::env::consts::OS == "linux", "release binaries are linux-only; use the docker image instead");
    let file = format!("midnight-node-{version}-linux-{arch}.tar.gz");
    let url = format!("https://github.com/midnightntwrk/midnight-node/releases/download/node-{version}/{file}");
    std::fs::create_dir_all(dir)?;
    let tarball = dir.join(&file);
    println!("downloading {url}");
    let ok = Command::new("curl").args(["-fL", "--progress-bar", "-o"]).arg(&tarball).arg(&url).status()?;
    ensure!(ok.success(), "download failed");
    let ok = Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(dir).status()?;
    ensure!(ok.success(), "extract failed");
    std::fs::remove_file(&tarball)?;
    println!("unpacked into {} (set `cmd: {}/midnight-node`)", dir.display(), dir.display());
    Ok(())
}

/// Minimal HTTP JSON-RPC client, so the tool needs no TLS/HTTP stack.
fn rpc(port: u16, method: &str, params: Value) -> Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        s,
        "POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    let body = resp.split_once("\r\n\r\n").map(|x| x.1).context("bad HTTP response")?;
    let v: Value = serde_json::from_str(body.trim())?;
    if let Some(e) = v.get("error") {
        bail!("rpc error: {e}");
    }
    Ok(v["result"].clone())
}

fn chain_info(port: u16) -> Result<String> {
    let hex = |v: &Value| u64::from_str_radix(v["number"].as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0);
    let best = hex(&rpc(port, "chain_getHeader", json!([]))?);
    let fin_hash = rpc(port, "chain_getFinalizedHead", json!([]))?;
    let fin = hex(&rpc(port, "chain_getHeader", json!([fin_hash]))?);
    let health = rpc(port, "system_health", json!([]))?;
    Ok(format!("best #{best}  finalized #{fin}  peers {}  syncing {}", health["peers"], health["isSyncing"]))
}
