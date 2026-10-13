//! End-to-end: a 3-validator midnight-starter devnet that follows **Cardano preview**.
//!
//! Ignored by default because it needs live infrastructure and real tADA:
//!
//!   E2E_DB_SYNC_POSTGRES   TLS connection string of a preview cardano-db-sync
//!                          (e.g. postgres://postgres:pw@127.0.0.1:5432/cexplorer?sslmode=require)
//!   OGMIOS_URL             preview Ogmios (default ws://127.0.0.1:1337)
//!   E2E_PAYMENT_SKEY       cardano-cli payment.skey funded with preview tADA
//!   E2E_GENESIS_UTXO       an unspent UTXO of that key (`tx#ix`); it identifies
//!                          the partner chain and is consumed by `governance init`
//!   E2E_WAIT_FOR_CARDANO_COMMITTEE=1
//!                          also wait (up to 3 preview days) until the committee
//!                          is the one registered on Cardano, not just genesis
//!
//! `e2e/preview/docker-compose.yml` runs cardano-node + db-sync + Postgres(TLS) + Ogmios.
//!
//!   cargo test --release --test e2e_preview -- --ignored --nocapture
//!
//! State lives in target/e2e/e2e-preview and survives reruns: the contracts are
//! deployed, the D-parameter set and the validator keys registered once; later
//! runs reuse them.

mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MIN: Duration = Duration::from_secs(60);
// Cardano preview timing (the node's res/preview/pc-chain-config.json)
const PREVIEW_FIRST_EPOCH_MS: u64 = 1_666_656_000_000;
const PREVIEW_EPOCH_MS: u64 = 86_400_000;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set for the preview e2e test (see tests/e2e_preview.rs)"))
}

fn preview_epoch_now() -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    (now - PREVIEW_FIRST_EPOCH_MS) / PREVIEW_EPOCH_MS
}

/// `midnight-node smart-contracts ...` against preview through Ogmios.
fn smart_contracts(args: &[&str]) -> String {
    let bin = node_bin();
    let out = Command::new(&bin)
        .current_dir(bin.parent().unwrap())
        .arg("smart-contracts")
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "smart-contracts {args:?} failed\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// Depth-first search for the first string value whose lowercased key path
/// contains every needle. get-scripts' exact key naming is not documented, so
/// match loosely.
fn find(v: &Value, path: &str, needles: &[&str]) -> Option<String> {
    match v {
        Value::Object(m) => m.iter().find_map(|(k, v)| find(v, &format!("{path}/{}", k.to_lowercase()), needles)),
        Value::String(s) if needles.iter().all(|n| path.replace('_', "").contains(n)) => Some(s.clone()),
        _ => None,
    }
}

struct State {
    path: std::path::PathBuf,
    v: Value,
}

impl State {
    fn load(dir: &Path) -> State {
        let path = dir.join("state.json");
        let v = fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
        State { path, v }
    }
    fn save(&self) {
        fs::write(&self.path, serde_json::to_string_pretty(&self.v).unwrap()).unwrap();
    }
}

#[test]
#[ignore = "needs a synced Cardano preview db-sync, Ogmios and a funded preview key"]
fn three_validator_devnet_following_cardano_preview() {
    let db_sync = env("E2E_DB_SYNC_POSTGRES");
    let ogmios = std::env::var("OGMIOS_URL").unwrap_or_else(|_| "ws://127.0.0.1:1337".into());
    let skey = fs::canonicalize(env("E2E_PAYMENT_SKEY")).expect("E2E_PAYMENT_SKEY file");
    let genesis_utxo = env("E2E_GENESIS_UTXO");
    let skey = path_str(&skey);
    let sc = |args: &[&str]| -> String {
        let mut a = args.to_vec();
        a.extend(["--ogmios-url", &ogmios]);
        smart_contracts(&a)
    };

    let dir = repo_root().join("target/e2e/e2e-preview");
    fs::create_dir_all(&dir).unwrap();
    let mut state = State::load(&dir);
    if state.v["genesis_utxo"].as_str().is_some_and(|u| u != genesis_utxo) {
        panic!("{} belongs to genesis UTXO {}; delete it to start a new chain", dir.display(), state.v["genesis_utxo"]);
    }
    state.v["genesis_utxo"] = json!(genesis_utxo);

    // ---- 1. partner-chain governance on preview (once) ------------------
    if state.v["governance"] != json!(true) {
        eprintln!("governance init on preview with genesis UTXO {genesis_utxo}");
        sc(&["governance", "init", "--threshold", "1", "--payment-key-file", &skey, "--genesis-utxo", &genesis_utxo]);
        state.v["governance"] = json!(true);
        state.save();
    }

    // ---- 2. contract addresses for this genesis UTXO --------------------
    let raw = sc(&["get-scripts", "--genesis-utxo", &genesis_utxo]);
    fs::write(dir.join("get-scripts.out"), &raw).unwrap();
    let scripts: Value = serde_json::from_str(&raw[raw.find('{').expect("get-scripts printed no JSON")..])
        .expect("get-scripts JSON");
    let committee_addr = find(&scripts, "", &["committeecandidate"])
        .filter(|a| a.starts_with("addr_test"))
        .unwrap_or_else(|| panic!("no committee candidate address in get-scripts output:\n{raw}"));
    let policy = find(&scripts, "", &["polic", "permissionedcandidates"])
        .unwrap_or_else(|| panic!("no permissioned candidates policy id in get-scripts output:\n{raw}"));
    let policy = if policy.starts_with("0x") { policy } else { format!("0x{policy}") };
    eprintln!("committee candidate address {committee_addr}, permissioned candidates policy {policy}");

    let deploy = dir.join("deployment");
    fs::create_dir_all(&deploy).unwrap();
    let res = node_bin().parent().unwrap().join("res/preview");
    let mut pcc: Value = serde_json::from_str(&fs::read_to_string(res.join("pc-chain-config.json")).unwrap()).unwrap();
    pcc["chain_parameters"]["genesis_utxo"] = json!(genesis_utxo);
    fs::write(deploy.join("pc-chain-config.json"), pcc.to_string()).unwrap();
    fs::write(
        deploy.join("registered-candidates-addresses.json"),
        json!({ "committee_candidates_address": committee_addr }).to_string(),
    )
    .unwrap();

    // ---- 3. midnight-starter init (keys survive reruns) ----------------------
    let net = Devnet::new(
        "e2e-preview",
        29944,
        3,
        &format!(
            r#"
slots_per_epoch: 10
ports: {{ p2p: 50333, rpc: 29944, prometheus: 29615 }}
mainchain:
  mode: cardano-preview
  db_sync_postgres: "{db_sync}"
  pc_chain_config: deployment/pc-chain-config.json
  registered_candidates_addresses: deployment/registered-candidates-addresses.json
  permissioned_candidates_policy_id: "{policy}"
validators: [ {{name: alice}}, {{name: bob}}, {{name: charlie}} ]
"#
        ),
        false,
    );
    net.try_run(&["stop"]);
    net.run(&["init", "--keep-keys"]);

    let spec: Value =
        serde_json::from_str(&fs::read_to_string(net.home().join("chain-spec.json")).unwrap()).unwrap();
    let cfg = &spec["genesis"]["runtimeGenesis"]["config"];
    assert_eq!(cfg["sidechain"]["genesisUtxo"], json!(genesis_utxo), "genesis UTXO not in spec");
    let scripts_in_spec = &cfg["sessionCommitteeManagement"]["mainChainScripts"];
    assert_eq!(scripts_in_spec["committee_candidate_address"], json!(committee_addr));
    assert_eq!(
        scripts_in_spec["permissioned_candidates_policy_id"].as_str().unwrap().to_lowercase(),
        policy.to_lowercase()
    );

    // ---- 4. register our validators as permissioned candidates (once per key set)
    let candidates: String = net
        .validators_json()
        .iter()
        .filter(|v| v.get("aura_pub_key").is_some())
        .map(|v| {
            let h = |k: &str| v[k].as_str().unwrap().trim_start_matches("0x").to_string();
            format!("{},aura:{},gran:{}\n", h("sidechain_pub_key"), h("aura_pub_key"), h("grandpa_pub_key"))
        })
        .collect();
    let cand_file = dir.join("permissioned-candidates.csv");
    if state.v["registered_candidates"].as_str() != Some(candidates.as_str()) {
        fs::write(&cand_file, &candidates).unwrap();
        eprintln!("upserting 3 permissioned candidates on preview");
        sc(&[
            "upsert-permissioned-candidates",
            "--permissioned-candidates-file", &path_str(&cand_file),
            "--payment-key-file", &skey,
            "--genesis-utxo", &genesis_utxo,
        ]);
        state.v["registered_candidates"] = json!(candidates);
        state.v["registered_in_mc_epoch"] = json!(preview_epoch_now());
        state.save();
    }
    let registered_epoch = state.v["registered_in_mc_epoch"].as_u64().unwrap();

    // D-parameter (3 permissioned, 0 registered) lives on Cardano too; the
    // genesis spec value only covers the first epochs.
    if state.v["d_parameter"] != json!([3, 0]) {
        eprintln!("upserting D-parameter (3, 0) on preview");
        sc(&[
            "upsert-d-parameter",
            "--permissioned-candidates-count", "3",
            "--registered-candidates-count", "0",
            "--payment-key-file", &skey,
            "--genesis-utxo", &genesis_utxo,
        ]);
        state.v["d_parameter"] = json!([3, 0]);
        state.save();
    }

    // ---- 5. start and check liveness ------------------------------------
    net.run(&["start", "--detach"]);
    for i in 0..3 {
        wait_for(&format!("node{i} has 2 peers"), 3 * MIN, || net.peers(i).filter(|&p| p >= 2));
    }
    wait_for("10 blocks finalized on every node", 10 * MIN, || {
        (0..3).all(|i| net.finalized(i).is_some_and(|f| f >= 10)).then_some(())
    });
    for i in 0..3 {
        assert!(net.authored(i) > 0, "validator node{i} authored no blocks\n{}", net.tail_logs());
    }

    // ---- 6. the node follows *preview*, not a mock ----------------------
    let status = net.rpc(0, "sidechain_getStatus", json!([])).expect("sidechain_getStatus");
    let mc_epoch = status["mainchain"]["epoch"].as_u64().unwrap();
    let wall = preview_epoch_now();
    assert!(
        mc_epoch.abs_diff(wall) <= 1,
        "node's main-chain epoch {mc_epoch} != preview wall-clock epoch {wall}: not following preview"
    );

    // Ariadne parameters are read from db-sync: this fails if db-sync is
    // unreachable, lagging or not on preview.
    let ariadne = |epoch: u64| net.rpc(0, "sidechain_getAriadneParameters", json!([epoch]));
    let params = ariadne(mc_epoch).unwrap_or_else(|e| panic!("db-sync query failed: {e}\n{}", net.tail_logs()));
    eprintln!("ariadne parameters for preview epoch {mc_epoch}: {params}");
    assert_eq!(params["dParameter"]["numPermissionedCandidates"], 3);

    let ours = net.sidechain_keys();
    let on_cardano = |p: &Value| -> Vec<String> {
        let mut k: Vec<String> = p["permissionedCandidates"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|c| c["isValid"] == json!(true))
                    .filter_map(|c| c["sidechainPublicKey"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        k.sort();
        k
    };

    // ---- 7. committee comes from our Cardano registration ---------------
    // Committee data lags the main chain by ~2 epochs (= 2 days on preview).
    if on_cardano(&params) != ours {
        let wait = std::env::var("E2E_WAIT_FOR_CARDANO_COMMITTEE").is_ok();
        eprintln!(
            "registration (preview epoch {registered_epoch}) not yet visible to committee selection \
             at epoch {mc_epoch}; expected from about epoch {}",
            registered_epoch + 2
        );
        if !wait {
            eprintln!("SKIPPED committee-from-Cardano check; set E2E_WAIT_FOR_CARDANO_COMMITTEE=1 to wait for it");
            return;
        }
        wait_for("Cardano preview lists our 3 validators as permissioned candidates", 3 * 24 * 60 * MIN, || {
            // keep asserting the chain is alive while we wait
            assert!(net.finalized(0).is_some(), "node0 stopped responding\n{}", net.tail_logs());
            std::thread::sleep(Duration::from_secs(300));
            let e = net.rpc(0, "sidechain_getStatus", json!([])).ok()?["mainchain"]["epoch"].as_u64()?;
            (on_cardano(&ariadne(e).ok()?) == ours).then_some(())
        });
    }
    // the selected committee of a fresh sidechain epoch is exactly our validators
    let start = sidechain_epoch(&net, 0).unwrap();
    let epoch = wait_for("a new sidechain epoch", 5 * MIN, || sidechain_epoch(&net, 0).filter(|&e| e > start + 1));
    assert_eq!(committee_keys(&net, 0, epoch).expect("committee"), ours);
    let before = net.finalized(0).unwrap();
    wait_for("finality continues under the Cardano-selected committee", 5 * MIN, || {
        net.finalized(0).filter(|&f| f >= before + 10).map(|_| ())
    });
}
