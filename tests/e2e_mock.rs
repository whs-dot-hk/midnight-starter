//! End-to-end: 3 validators + 1 full node with the main-chain follower mock.
//! Needs only the midnight-node release binary (`cargo run -- fetch`).
//!
//!     cargo test --release --test e2e_mock -- --nocapture

mod common;

use common::*;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::time::Duration;

const MIN: Duration = Duration::from_secs(60);

#[test]
fn three_validator_mock_devnet() {
    let net = Devnet::new(
        "e2e-mock",
        19944,
        3,
        r#"
slots_per_epoch: 5
ports: { p2p: 40333, rpc: 19944, prometheus: 19615 }
mainchain: { mode: mock }
validators: [ {name: alice}, {name: bob}, {name: charlie} ]
full_nodes: [ {name: archive} ]
genesis:
  properties: { tokenSymbol: tDUST }
"#,
        true,
    );

    // ---- init: keys, committee and spec ---------------------------------
    net.run(&["init"]);
    let vals = net.validators_json();
    assert_eq!(vals.iter().filter(|v| v.get("aura_pub_key").is_some()).count(), 3);
    let spec: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(net.home().join("chain-spec.json")).unwrap()).unwrap();
    let cfg = &spec["genesis"]["runtimeGenesis"]["config"];
    assert_eq!(cfg["sidechain"]["slotsPerEpoch"], 5);
    assert_eq!(cfg["sessionCommitteeManagement"]["initialAuthorities"].as_array().unwrap().len(), 3);
    assert_eq!(cfg["systemParameters"]["dParameter"]["num_permissioned_candidates"], 3);
    assert_eq!(spec["properties"]["tokenSymbol"], "tDUST", "genesis patch not merged");

    // ---- start ----------------------------------------------------------
    net.run(&["start", "--detach"]);
    for i in 0..4 {
        wait_for(&format!("node{i} has 3 peers"), 2 * MIN, || net.peers(i).filter(|&p| p >= 3));
    }
    // 5-slot epochs: 20 finalized blocks spans several committee rotations
    wait_for("20 blocks finalized on every node", 6 * MIN, || {
        (0..4).all(|i| net.finalized(i).is_some_and(|f| f >= 20)).then_some(())
    });

    // ---- every validator authors, the full node does not ----------------
    for i in 0..3 {
        assert!(net.authored(i) > 0, "validator node{i} authored no blocks\n{}", net.tail_logs());
    }
    assert_eq!(net.authored(3), 0, "full node authored blocks");

    // ---- committee is exactly our 3 validators --------------------------
    let epoch = sidechain_epoch(&net, 0).expect("sidechain_getStatus");
    assert_eq!(committee_keys(&net, 0, epoch).expect("committee"), net.sidechain_keys());

    // ---- supervisor restarts a crashed validator, finality continues ----
    let pid: i32 = std::fs::read_to_string(net.home().join("node1/node.pid")).unwrap().trim().parse().unwrap();
    kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
    wait_for("node1 restarted by supervisor", MIN, || {
        let new: i32 = std::fs::read_to_string(net.home().join("node1/node.pid")).ok()?.trim().parse().ok()?;
        (new != pid).then_some(())
    });
    let before = net.finalized(0).unwrap();
    // GRANDPA with 3 voters needs all 3, so this also proves node1 rejoined
    wait_for("finality advances after node1 restart", 4 * MIN, || {
        net.finalized(1).filter(|&f| f >= before + 5).map(|_| ())
    });

    let status = net.run(&["status"]);
    eprintln!("{status}");
    assert_eq!(status.matches("RUNNING").count(), 5, "{status}");

    // ---- stop -----------------------------------------------------------
    net.run(&["stop"]);
    assert!(net.run(&["status"]).contains("supervisor   STOPPED"));
}
