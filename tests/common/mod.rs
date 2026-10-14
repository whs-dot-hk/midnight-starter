//! Shared helpers for the end-to-end tests. They drive the real `midnight-starter`
//! binary (and through it real `midnight-node` processes), never library code.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_midnight-starter");

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `MIDNIGHT_NODE` or `./bin/midnight-node` (from `midnight-starter fetch`).
pub fn node_bin() -> PathBuf {
    let p = std::env::var("MIDNIGHT_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("bin/midnight-node"));
    assert!(
        p.is_file(),
        "{} not found: run `cargo run -- fetch` or set MIDNIGHT_NODE",
        p.display()
    );
    p.canonicalize().unwrap()
}

/// A devnet under `target/e2e/<name>`, stopped on drop so a failing assertion
/// never leaves nodes running.
pub struct Devnet {
    pub dir: PathBuf,
    pub chain: String,
    pub rpc_base: u16,
    pub validators: usize,
}

impl Devnet {
    /// `body` is the YAML under the chain key, without `cmd`.
    pub fn new(name: &str, rpc_base: u16, validators: usize, body: &str, fresh: bool) -> Devnet {
        let dir = repo_root().join("target/e2e").join(name);
        if fresh {
            let net = Devnet { dir: dir.clone(), chain: name.into(), rpc_base, validators };
            if dir.exists() {
                net.try_run(&["stop"]);
                std::fs::remove_dir_all(&dir).unwrap();
            }
        }
        std::fs::create_dir_all(&dir).unwrap();
        let yaml = format!("{name}:\n  cmd: {}\n{}", node_bin().display(), indent(body));
        std::fs::write(dir.join("devnet.yaml"), yaml).unwrap();
        Devnet { dir, chain: name.into(), rpc_base, validators }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.join("data").join(&self.chain)
    }

    pub fn try_run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--env")
            .arg(&self.chain) // never a developer's .env.undeployed
            .arg("--config")
            .arg(self.dir.join("devnet.yaml"))
            .arg("--data")
            .arg(self.dir.join("data"))
            .args(args)
            .output()
            .expect("spawning midnight-starter")
    }

    /// Run a midnight-starter subcommand, panicking with its output on failure.
    pub fn run(&self, args: &[&str]) -> String {
        let out = self.try_run(args);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            out.status.success(),
            "midnight-starter {args:?} failed\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    pub fn rpc(&self, node: usize, method: &str, params: Value) -> Result<Value, String> {
        let out = self.try_run(&["rpc", method, &params.to_string(), "--node", &node.to_string()]);
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
    }

    pub fn best(&self, node: usize) -> Option<u64> {
        self.rpc(node, "chain_getHeader", json!([])).ok().map(|h| block_number(&h))
    }

    pub fn finalized(&self, node: usize) -> Option<u64> {
        let hash = self.rpc(node, "chain_getFinalizedHead", json!([])).ok()?;
        self.rpc(node, "chain_getHeader", json!([hash])).ok().map(|h| block_number(&h))
    }

    pub fn peers(&self, node: usize) -> Option<u64> {
        self.rpc(node, "system_health", json!([])).ok()?["peers"].as_u64()
    }

    pub fn log(&self, node: usize) -> String {
        std::fs::read_to_string(self.home().join(format!("node{node}/node.log"))).unwrap_or_default()
    }

    /// Blocks this node authored, from its "Prepared block for proposing" log lines.
    pub fn authored(&self, node: usize) -> usize {
        self.log(node).matches("Prepared block for proposing").count()
    }

    pub fn validators_json(&self) -> Vec<Value> {
        let raw = std::fs::read_to_string(self.home().join("validators.json")).unwrap();
        serde_json::from_str::<Vec<Value>>(&raw).unwrap()
    }

    /// The sidechain (cross-chain ecdsa) keys of our genesis validators.
    pub fn sidechain_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .validators_json()
            .iter()
            .filter_map(|v| v["sidechain_pub_key"].as_str().map(str::to_string))
            .collect();
        keys.sort();
        keys
    }

    pub fn tail_logs(&self) -> String {
        let n = self.validators + 1;
        (0..n)
            .map(|i| {
                let log = self.log(i);
                let tail: Vec<&str> = log.lines().rev().take(15).collect();
                format!("--- node{i} ---\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for Devnet {
    fn drop(&mut self) {
        if std::env::var("E2E_KEEP_RUNNING").is_err() {
            self.try_run(&["stop"]);
        }
    }
}

pub fn block_number(header: &Value) -> u64 {
    u64::from_str_radix(header["number"].as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap()
}

/// Poll `f` until it returns Some, or panic with `what` after `timeout`.
pub fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            eprintln!("ok: {what} ({}s)", start.elapsed().as_secs());
            return v;
        }
        assert!(start.elapsed() < timeout, "timed out after {timeout:?} waiting for: {what}");
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// The sidechain keys of the committee the chain itself reports for `epoch`.
pub fn committee_keys(net: &Devnet, node: usize, epoch: u64) -> Option<Vec<String>> {
    let c = net.rpc(node, "sidechain_getEpochCommittee", json!([epoch])).ok()?;
    let mut keys: Vec<String> = c["committee"]
        .as_array()?
        .iter()
        .filter_map(|m| m["sidechainPubKey"].as_str().map(str::to_string))
        .collect();
    keys.sort();
    keys.dedup();
    Some(keys)
}

pub fn sidechain_epoch(net: &Devnet, node: usize) -> Option<u64> {
    net.rpc(node, "sidechain_getStatus", json!([])).ok()?["sidechain"]["epoch"].as_u64()
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("  {l}\n")).collect()
}

pub fn path_str(p: &Path) -> String {
    p.display().to_string()
}
