# midnight-starter

A launcher for local **Midnight** devnets, written in Rust. You describe the network in YAML, then
`midnight-starter serve` generates keys, the genesis committee and the chain spec, and
runs every `midnight-node` under a built-in supervisor.

```bash
cargo build --release
./target/release/midnight-starter fetch          # downloads midnight-node <version> into ./bin
./target/release/midnight-starter serve --detach # init + start (devnet.yaml)
./target/release/midnight-starter status
./target/release/midnight-starter logs bob -f        # by name or node index
./target/release/midnight-starter rpc system_health --node 2
./target/release/midnight-starter stop
```

## How a Midnight multi-validator devnet works

Midnight is a Substrate chain built as a Cardano *partner chain*:

- **Block production and finality.** AURA produces blocks and GRANDPA finalizes
  them. Each validator needs four keys:

  | Key | Scheme | Keystore id |
  |---|---|---|
  | AURA | sr25519 | `aura` |
  | GRANDPA | ed25519 | `gran` |
  | Cross-chain | ecdsa | `crch` |
  | BEEFY | ecdsa | `beef` |

  The cross-chain and BEEFY keys share one ecdsa key. Node versions 2.x also
  carry a BABE key, which is detected automatically.
- **Where the committee comes from.** Every sidechain epoch, the committee is
  selected from Cardano: permissioned candidates plus registered SPOs, weighted
  by the D-parameter. The node reads that Cardano state from cardano-db-sync's
  Postgres. It can also use a built-in **mock follower**
  (`USE_MAIN_CHAIN_FOLLOWER_MOCK`) that serves the committee from a JSON file.
- **Genesis committee.** The first committee is
  `initial_permissioned_candidates` in the chain spec.

`init` does the following for each node in `<data>/<chain>/nodeN`:

1. Generates the keys with `midnight-node key generate`. The node loads them at
   startup through the `*_SEED_FILE` env vars.
2. Generates a libp2p node key.
3. Writes `permissioned-candidates-config.json`,
   `system-parameters-config.json` (D-parameter = N permissioned) and
   `mock-registrations.json`.
4. Runs `build-spec` from the `dev` preset with those files swapped in.
5. Sets `slotsPerEpoch` and deep-merges your `genesis:` patch into the spec.
6. Writes `programs.json`. Node 0 is the bootnode.

`validators.json` lists every validator's public keys and peer id.

### Port rules

Node *i* uses `ports.p2p + i`, `ports.rpc + i` and `ports.prometheus + i`. The
defaults are 30333, 9944 and 9615, so node0 answers on the standard
`ws://127.0.0.1:9944`.

### Supervisor

`start` runs every node under a small built-in supervisor. It restarts a node that exits, gives up on one that crashes six
times in a row, and stops every node on SIGINT/SIGTERM. With `--detach` the
command returns only after the background supervisor has taken over, and
fails if it couldn't (for instance because one is already running).

Pid files live in `<data>/<chain>/supervisor.pid` and `nodeN/node.pid`. They
are trusted only if the pid still belongs to a `midnight-starter` / `midnight-node`
process, so files left behind by a crash or reboot are ignored. If the
supervisor itself is killed, `stop` finds the orphaned nodes through their pid
files and stops them, and `start`/`init` refuse to run until that is done.

## Mode 1: `mock` (default, no Cardano needed)

`devnet.yaml` sets up 3 validators plus 1 archive node. It was tested with
`midnight-node 1.0.400`: all nodes peered, blocks were authored and finalized,
and the chain ran across several 10-slot epochs.

## Mode 2: `cardano-preview` (real Cardano preview)

See `examples/preview.yaml`. Here the committee is read from Cardano preview, so
you need:

1. **A preview Cardano stack:** cardano-node, cardano-db-sync and Postgres,
   fully synced to preview (network magic 2). Postgres must accept **TLS**
   connections (`sslmode=require`), because the node refuses plain-text
   connections. Midnight's own local env uses cardano-node 10.x/11.x and db-sync
   13.6.x, plus ogmios (and kupo) for deploying contracts.
2. **Your own partner-chain contracts on preview.** Use the partner-chains
   `wizards prepare-configuration` / `setup-main-chain-state` flow, or
   `midnight-node smart-contracts ...`. Register the keys from
   `data/<chain>/validators.json` as permissioned candidates, and set the
   D-parameter to `(3, 0)`.
3. **Your deployment's config files** referenced from the YAML:
   `pc_chain_config` (genesis UTXO and addresses),
   `permissioned_candidates_policy_id` and `federated_authority_config`.
   Without them the spec falls back to the node's `res/preview` files. Those
   have an all-zero genesis UTXO, and `init` warns about it.

The preview timing parameters (`MC__*`, security parameter 432, active-slots
coefficient 0.05) are already the node's defaults. Override them under
`mainchain.env`.

`$VARS` in `db_sync_postgres` (and in any `env` value) are expanded when a
node is spawned, not at `init`, so the Postgres password is never written to
`programs.json`. The variable must be set in the shell that runs `start`.

`init` regenerates keys unless you pass `--keep-keys`. Use that flag after the
keys are registered on preview. It resets only chain data.

## End-to-end tests

Both tests drive the real `midnight-starter` binary and real `midnight-node`
processes. Run `cargo run -- fetch` first, or set `MIDNIGHT_NODE`.

### `tests/e2e_mock.rs` (mock follower, runs anywhere, ~3 min)

```bash
cargo test --release --test e2e_mock -- --nocapture
```

The test starts 3 validators plus 1 full node and checks:

- the spec contents: committee, D-parameter, epoch length and the genesis patch;
- that every node sees 3 peers;
- that 20 blocks are finalized across several 5-slot epochs;
- that every validator authors blocks and the full node doesn't;
- that the on-chain committee is exactly our 3 keys;
- that after `kill -9` on a validator the supervisor restarts it and finality
  resumes (3 GRANDPA voters need all 3);
- `status` and `stop`.

### `tests/e2e_preview.rs` (real Cardano preview, `#[ignore]`)

```bash
cd e2e/preview && ./gen-certs.sh && POSTGRES_PASSWORD=... docker compose up -d   # wait for full sync
export E2E_DB_SYNC_POSTGRES='postgres://postgres:...@127.0.0.1:5432/cexplorer?sslmode=require'
export OGMIOS_URL=ws://127.0.0.1:1337
export E2E_PAYMENT_SKEY=~/preview/payment.skey      # funded with preview tADA (faucet)
export E2E_GENESIS_UTXO=<tx-hash>#<ix>               # unspent UTXO of that key
cargo test --release --test e2e_preview -- --ignored --nocapture
```

The test runs these steps:

1. `smart-contracts governance init` on preview.
2. `get-scripts`, which gives the committee candidate address and the
   permissioned-candidates policy id.
3. `midnight-starter init --keep-keys` with those values. The test asserts that
   they appear in the chain spec.
4. `upsert-permissioned-candidates` with the 3 generated validators, and
   `upsert-d-parameter` (3 permissioned, 0 registered).
5. `start`. The test checks peers, finality and that each validator authors
   blocks.
6. A check that the node really follows preview: its main-chain epoch must
   match preview's wall-clock epoch, and `sidechain_getAriadneParameters` must
   succeed against db-sync.
7. A check that the committee comes from the Cardano registration.

Steps 1 and 4 and the keys are saved in `target/e2e/e2e-preview/state.json`,
so reruns don't repeat the transactions.

Partner-chain committee data lags Cardano by about 2 epochs, which is about 2
days on preview. So step 7 is reported as skipped until then, unless you set
`E2E_WAIT_FOR_CARDANO_COMMITTEE=1`, which makes the test wait for it.

## Config reference

```yaml
<chain-name>:
  cmd: ./bin/midnight-node   # binary; relative to the yaml
  res_dir: ./bin             # dir containing res/ (default: binary's dir)
  version: 1.0.400           # used by `fetch`
  slots_per_epoch: 10
  ports: { p2p: 30333, rpc: 9944, prometheus: 9615 }
  mainchain: { mode: mock }  # or cardano-preview, see examples/preview.yaml
  validators: [ {name: alice, args: [], env: {}}, ... ]
  full_nodes: [ {name: archive} ]
  env: { RUST_LOG: info }    # all nodes
  args: []                   # all nodes
  genesis: {}                # deep-merged into chain-spec.json (null deletes)
```

## Limitations

- **Fixed chain id.** The chain id is always `midnight_undeployed`, because it
  comes from the genesis `.mn` file. A custom id needs a new genesis file from
  the midnight toolkit.
- **Linux only.** Release binaries are Linux-only. On macOS, point `cmd` at a
  self-built node.
