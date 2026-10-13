//! YAML devnet description.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Top level: `<chain-name>: ChainConfig`.
pub type Config = BTreeMap<String, ChainConfig>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    /// midnight-node binary. Relative paths resolve against the config file.
    #[serde(default = "default_cmd")]
    pub cmd: String,
    /// Directory that contains the node's `res/` tree (the node resolves its
    /// presets relative to its cwd). Defaults to the binary's directory.
    pub res_dir: Option<String>,
    /// Release used by `midnight-starter fetch`.
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub ports: Ports,
    /// Where the committee and Cardano state come from.
    #[serde(default)]
    pub mainchain: MainChain,
    /// Genesis validators; one node is started per entry.
    pub validators: Vec<Validator>,
    /// Extra non-validator (full / archive) nodes.
    #[serde(default)]
    pub full_nodes: Vec<Validator>,
    /// Sidechain epoch length in slots. Short epochs make committee
    /// rotation observable on a devnet; the stock spec uses 300.
    #[serde(default = "default_slots_per_epoch")]
    pub slots_per_epoch: u64,
    /// Deep-merged into the generated chain spec JSON before nodes start.
    #[serde(default)]
    pub genesis: serde_json::Value,
    /// Extra env vars for every node (e.g. RUST_LOG).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Extra CLI args for every node.
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    /// Node i listens on `<base> + i` for each service.
    pub p2p: u16,
    pub rpc: u16,
    pub prometheus: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Ports { p2p: 30333, rpc: 9944, prometheus: 9615 }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Validator {
    pub name: Option<String>,
    /// Per-node extra args / env, appended after the chain-wide ones.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum MainChain {
    /// No Cardano at all: the node's built-in main-chain follower mock serves
    /// the committee from a generated registrations file.
    Mock,
    /// Follow Cardano preview through a cardano-db-sync Postgres.
    CardanoPreview(CardanoPreview),
}

impl Default for MainChain {
    fn default() -> Self {
        MainChain::Mock
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardanoPreview {
    /// db-sync connection string. Postgres must speak TLS (the node refuses
    /// plain-text connections). `$VARS` are expanded from the environment.
    pub db_sync_postgres: String,
    /// Partner-chain config with *your* genesis UTXO and contract addresses
    /// (output of the partner-chains `prepare-configuration` wizard / your
    /// contract deployment). Defaults to the node's `res/preview` one, which
    /// points at Midnight's own preview contracts.
    pub pc_chain_config: Option<String>,
    /// Policy id of your permissioned-candidates contract on preview.
    pub permissioned_candidates_policy_id: Option<String>,
    pub federated_authority_config: Option<String>,
    pub registered_candidates_addresses: Option<String>,
    pub cnight_genesis: Option<String>,
    pub ics_config: Option<String>,
    pub reserve_config: Option<String>,
    /// Overrides for the Cardano preview timing parameters (defaults are
    /// already preview's, from the node's res/cfg/default.toml).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_cmd() -> String {
    "midnight-node".into()
}
fn default_version() -> String {
    "1.0.400".into()
}
fn default_slots_per_epoch() -> u64 {
    10
}

pub fn load(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    for (name, c) in &cfg {
        if c.validators.is_empty() {
            bail!("{name}: at least one validator is required");
        }
    }
    Ok(cfg)
}

/// Expand `$VAR` / `${VAR}` from the process environment.
pub fn expand_env(s: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let (name, tail) = if let Some(r) = rest.strip_prefix('{') {
            let end = r.find('}').context("unterminated ${")?;
            (&r[..end], &r[end + 1..])
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        };
        if name.is_empty() {
            out.push('$');
        } else {
            out.push_str(&std::env::var(name).with_context(|| format!("env var {name} is not set"))?);
        }
        rest = tail;
    }
    out.push_str(rest);
    Ok(out)
}
