//! `init`: generate keys, chain spec and the supervisor program list.
//!
//! Layout (per chain, under `<data>/<chain>`):
//!   chain-spec.json                       patched output of `build-spec`
//!   permissioned-candidates-config.json   genesis committee
//!   system-parameters-config.json         D-parameter = N permissioned
//!   mock-registrations.json               committee for the mock follower
//!   nodeN/{seeds/*.seed, node.key, data/, node.log}
//!   programs.json                         what `start` launches
//!   validators.json                       public keys + peer ids (for scripts)

use crate::config::{ChainConfig, MainChain, expand_env};
use crate::supervisor::Program;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Node {
    name: String,
    dir: PathBuf,
    validator: bool,
    peer_id: String,
    keys: BTreeMap<&'static str, Value>,
}

pub struct Resolved {
    pub cmd: PathBuf,
    pub res_dir: PathBuf,
}

pub fn resolve_paths(cfg: &ChainConfig, config_dir: &Path) -> Result<Resolved> {
    let cmd = if cfg.cmd.contains('/') {
        config_dir.join(&cfg.cmd)
    } else {
        which(&cfg.cmd).with_context(|| format!("`{}` not found in PATH; set `cmd` or run `midnight-starter fetch`", cfg.cmd))?
    };
    ensure!(cmd.exists(), "{} does not exist; run `midnight-starter fetch` or fix `cmd`", cmd.display());
    let cmd = cmd.canonicalize()?;
    let res_dir = match &cfg.res_dir {
        Some(d) => config_dir.join(d).canonicalize()?,
        None => cmd.parent().unwrap().to_path_buf(),
    };
    ensure!(
        res_dir.join("res/cfg").is_dir(),
        "{} has no res/cfg; point `res_dir` at the extracted release directory",
        res_dir.display()
    );
    Ok(Resolved { cmd, res_dir })
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
}

fn node_cmd(r: &Resolved) -> Command {
    let mut c = Command::new(&r.cmd);
    c.current_dir(&r.res_dir);
    c
}

fn run_ok(mut c: Command) -> Result<std::process::Output> {
    let out = c.output().with_context(|| format!("running {c:?}"))?;
    if !out.status.success() {
        bail!("{c:?} failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out)
}

fn gen_key(r: &Resolved, scheme: &str) -> Result<Value> {
    let mut c = node_cmd(r);
    c.args(["key", "generate", "--scheme", scheme, "--output-type", "json"]);
    Ok(serde_json::from_slice(&run_ok(c)?.stdout)?)
}

fn inspect_key(r: &Resolved, scheme: &str, seed: &str) -> Result<Value> {
    let mut c = node_cmd(r);
    c.args(["key", "inspect", "--scheme", scheme, "--output-type", "json", seed]);
    Ok(serde_json::from_slice(&run_ok(c)?.stdout)?)
}

fn str_field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str().with_context(|| format!("missing `{k}` in key output: {v}"))
}

fn read_json(p: &Path) -> Result<Value> {
    serde_json::from_str(&fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?)
        .with_context(|| format!("parsing {}", p.display()))
}

fn write_json(p: &Path, v: &Value) -> Result<()> {
    fs::write(p, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

/// RFC 7386-ish deep merge (null deletes) for the `genesis` patch.
pub fn merge(dst: &mut Value, patch: &Value) {
    match (dst, patch) {
        (Value::Object(d), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    d.remove(k);
                } else {
                    merge(d.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (d, p) => *d = p.clone(),
    }
}

/// `keep_keys`: reuse the seeds and node keys already under `<data>/<chain>`
/// (needed once they are registered on Cardano) and only reset chain data.
pub fn init(chain: &str, cfg: &ChainConfig, config_dir: &Path, data: &Path, keep_keys: bool) -> Result<Vec<Program>> {
    let r = resolve_paths(cfg, config_dir)?;
    let home = data.join(chain);
    if home.join("supervisor.pid").exists() && crate::supervisor::is_running(&home) {
        bail!("{chain} is running; `midnight-starter stop` it first");
    }
    if home.exists() {
        if keep_keys {
            for e in fs::read_dir(&home)? {
                let p = e?.path();
                if p.is_dir() {
                    let _ = fs::remove_dir_all(p.join("data"));
                    let _ = fs::remove_file(p.join("node.log"));
                }
            }
        } else {
            fs::remove_dir_all(&home)?;
        }
    }
    fs::create_dir_all(&home)?;
    let home = home.canonicalize()?;

    // Newer node versions (main / 2.x) also carry a BABE key in the committee;
    // follow whatever the shipped template uses.
    let tmpl = read_json(&r.res_dir.join("res/dev/permissioned-candidates-config.json"))?;
    let with_babe = tmpl["initial_permissioned_candidates"][0].get("babe_pub_key").is_some();

    // ---- keys -----------------------------------------------------------
    let mut nodes = Vec::new();
    let all = cfg.validators.iter().map(|v| (v, true)).chain(cfg.full_nodes.iter().map(|v| (v, false)));
    for (i, (v, validator)) in all.enumerate() {
        let dir = home.join(format!("node{i}"));
        fs::create_dir_all(dir.join("seeds"))?;
        let name = v.name.clone().unwrap_or_else(|| format!("node{i}"));

        let mut keys = BTreeMap::new();
        if validator {
            let key = |k: &str, scheme: &str| -> Result<Value> {
                let seed = dir.join(format!("seeds/{k}.seed"));
                if keep_keys && seed.exists() {
                    inspect_key(&r, scheme, fs::read_to_string(&seed)?.trim())
                } else {
                    gen_key(&r, scheme)
                }
            };
            let cc = key("cross_chain", "ecdsa")?;
            keys.insert("aura", key("aura", "sr25519")?);
            keys.insert("grandpa", key("grandpa", "ed25519")?);
            // cross-chain and BEEFY share one ecdsa key, as in the node's own local env
            keys.insert("cross_chain", cc.clone());
            keys.insert("beefy", cc);
            if with_babe {
                keys.insert("babe", key("babe", "sr25519")?);
            }
            for (k, v) in &keys {
                fs::write(dir.join(format!("seeds/{k}.seed")), str_field(v, "secretSeed")?)?;
            }
        }

        let node_key = dir.join("node.key");
        let mut c = node_cmd(&r);
        if keep_keys && node_key.exists() {
            c.args(["key", "inspect-node-key", "--file"]).arg(&node_key);
        } else {
            // generate-node-key insists on a chain spec, hence `--chain dev`.
            c.args(["key", "generate-node-key", "--chain", "dev", "--file"]).arg(&node_key);
        }
        let out = run_ok(c)?;
        // generate-node-key prints the peer id on stderr, inspect-node-key on stdout
        let peer_id = [out.stdout, out.stderr].concat();
        let peer_id = String::from_utf8_lossy(&peer_id)
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("12D3"))
            .map(str::to_string)
            .context("could not read the node's peer id")?;

        println!("{chain}/{name}: peer {peer_id}{}", if validator { " (validator)" } else { "" });
        nodes.push(Node { name, dir, validator, peer_id, keys });
    }

    // ---- genesis committee ---------------------------------------------
    let validators: Vec<&Node> = nodes.iter().filter(|n| n.validator).collect();
    let n = validators.len();
    let pk = |node: &Node, k: &str| -> Result<String> { Ok(str_field(&node.keys[k], "publicKey")?.to_string()) };

    let mut candidates = Vec::new();
    let mut mock = Vec::new();
    let mut summary = Vec::new();
    for node in &validators {
        let mut c = json!({
            "aura_pub_key": pk(node, "aura")?,
            "grandpa_pub_key": pk(node, "grandpa")?,
            "sidechain_pub_key": pk(node, "cross_chain")?,
            "beefy_pub_key": pk(node, "beefy")?,
        });
        if with_babe {
            c["babe_pub_key"] = json!(pk(node, "babe")?);
        }
        mock.push(json!({
            "name": node.name,
            "sidechain_pub_key": c["sidechain_pub_key"],
            "aura_pub_key": c["aura_pub_key"],
            "grandpa_pub_key": c["grandpa_pub_key"],
        }));
        let mut s = c.clone();
        s["name"] = json!(node.name);
        s["peer_id"] = json!(node.peer_id);
        summary.push(s);
        candidates.push(c);
    }
    for node in nodes.iter().filter(|n| !n.validator) {
        summary.push(json!({ "name": node.name, "peer_id": node.peer_id, "validator": false }));
    }

    let mut pcc = tmpl;
    pcc["initial_permissioned_candidates"] = json!(candidates);
    if let MainChain::CardanoPreview(p) = &cfg.mainchain {
        if let Some(id) = &p.permissioned_candidates_policy_id {
            pcc["permissioned_candidates_policy_id"] = json!(id);
        }
    }
    write_json(&home.join("permissioned-candidates-config.json"), &pcc)?;

    let mut spc = read_json(&r.res_dir.join("res/dev/system-parameters-config.json"))?;
    spc["d_parameter"] = json!({ "num_permissioned_candidates": n, "num_registered_candidates": 0 });
    write_json(&home.join("system-parameters-config.json"), &spc)?;

    write_json(
        &home.join("mock-registrations.json"),
        &json!([{
            "registrations": [],
            "permissioned": mock,
            "nonce": "0x1234",
            "d_parameter": { "permissioned": n, "registered": 0 },
        }]),
    )?;
    write_json(&home.join("validators.json"), &json!(summary))?;

    // ---- chain spec -----------------------------------------------------
    // Start from the `dev` preset (undeployed genesis) and swap in our files.
    let mut c = node_cmd(&r);
    c.env("CFG_PRESET", "dev")
        .env("CHAINSPEC_NAME", chain)
        .env("CHAINSPEC_PERMISSIONED_CANDIDATES_CONFIG", home.join("permissioned-candidates-config.json"))
        .env("CHAINSPEC_SYSTEM_PARAMETERS_CONFIG", home.join("system-parameters-config.json"))
        .args(["build-spec", "--disable-default-bootnode"]);
    if let MainChain::CardanoPreview(p) = &cfg.mainchain {
        let res = |o: &Option<String>, f: &str| -> Result<PathBuf> {
            Ok(match o {
                Some(x) => config_dir.join(x).canonicalize().with_context(|| format!("{x} not found"))?,
                None => r.res_dir.join("res/preview").join(f),
            })
        };
        c.env("CHAINSPEC_PC_CHAIN_CONFIG", res(&p.pc_chain_config, "pc-chain-config.json")?)
            .env("CHAINSPEC_FEDERATED_AUTHORITY_CONFIG", res(&p.federated_authority_config, "federated-authority-config.json")?)
            .env("CHAINSPEC_REGISTERED_CANDIDATES_ADDRESSES", res(&p.registered_candidates_addresses, "registered-candidates-addresses.json")?)
            .env("CHAINSPEC_CNIGHT_GENESIS", res(&p.cnight_genesis, "cnight-config.json")?)
            .env("CHAINSPEC_ICS_CONFIG", res(&p.ics_config, "ics-config.json")?)
            .env("CHAINSPEC_RESERVE_CONFIG", res(&p.reserve_config, "reserve-config.json")?);
    }
    let out = run_ok(c)?;
    let mut spec: Value = serde_json::from_slice(&out.stdout).context("build-spec did not print JSON")?;
    let rt = &mut spec["genesis"]["runtimeGenesis"];
    let key = if rt.get("patch").is_some() { "patch" } else { "config" };
    rt[key]["sidechain"]["slotsPerEpoch"] = json!(cfg.slots_per_epoch);
    if matches!(cfg.mainchain, MainChain::CardanoPreview(_))
        && rt[key]["sidechain"]["genesisUtxo"].as_str().is_some_and(|u| u.starts_with("00000000"))
    {
        eprintln!(
            "warning: genesis UTXO is all zeros; set `mainchain.pc_chain_config` to the \
             pc-chain-config.json of your own preview deployment"
        );
    }
    if !cfg.genesis.is_null() {
        merge(&mut spec, &cfg.genesis);
    }
    let spec_path = home.join("chain-spec.json");
    write_json(&spec_path, &spec)?;

    // ---- programs -------------------------------------------------------
    let mut programs = Vec::new();
    let boot = &nodes[0];
    for (i, node) in nodes.iter().enumerate() {
        let v = if node.validator { &cfg.validators[i] } else { &cfg.full_nodes[i - n] };
        let i16 = i as u16;
        let mut args: Vec<String> = vec![
            "--chain".into(), spec_path.display().to_string(),
            "--base-path".into(), node.dir.join("data").display().to_string(),
            "--name".into(), node.name.clone(),
            "--node-key-file".into(), node.dir.join("node.key").display().to_string(),
            "--port".into(), (cfg.ports.p2p + i16).to_string(),
            "--rpc-port".into(), (cfg.ports.rpc + i16).to_string(),
            "--prometheus-port".into(), (cfg.ports.prometheus + i16).to_string(),
            "--rpc-cors=all".into(),
            "--rpc-methods=unsafe".into(),
            "--state-pruning".into(), "archive".into(),
            "--blocks-pruning".into(), "archive".into(),
            "--no-mdns".into(),
        ];
        if node.validator {
            args.push("--validator".into());
        }
        if i > 0 {
            args.push("--bootnodes".into());
            args.push(format!("/ip4/127.0.0.1/tcp/{}/p2p/{}", cfg.ports.p2p, boot.peer_id));
        }
        args.extend(cfg.args.iter().cloned());
        args.extend(v.args.iter().cloned());

        let mut env = BTreeMap::new();
        for k in node.keys.keys() {
            env.insert(
                format!("{}_SEED_FILE", k.to_uppercase()),
                node.dir.join(format!("seeds/{k}.seed")).display().to_string(),
            );
        }
        match &cfg.mainchain {
            MainChain::Mock => {
                env.insert("USE_MAIN_CHAIN_FOLLOWER_MOCK".into(), "true".into());
                env.insert("MOCK_REGISTRATIONS_FILE".into(), home.join("mock-registrations.json").display().to_string());
            }
            MainChain::CardanoPreview(p) => {
                env.insert("USE_MAIN_CHAIN_FOLLOWER_MOCK".into(), "false".into());
                // kept unexpanded: the supervisor expands `$VARS` at spawn time so
                // the Postgres password never lands in programs.json
                expand_env(&p.db_sync_postgres).context("mainchain.db_sync_postgres")?;
                env.insert("DB_SYNC_POSTGRES_CONNECTION_STRING".into(), p.db_sync_postgres.clone());
                env.extend(p.env.clone());
            }
        }
        env.extend(cfg.env.clone());
        env.extend(v.env.clone());

        programs.push(Program {
            name: format!("node{i}"),
            cmd: r.cmd.display().to_string(),
            args,
            env,
            workdir: r.res_dir.clone(),
            log: node.dir.join("node.log"),
        });
    }
    write_json(&home.join("programs.json"), &serde_json::to_value(&programs)?)?;
    Ok(programs)
}
