# vLichen

A prototype layer 2 for [lichen](https://github.com/lichenbtc-org/lichen) that has a sequencer that proves validity of updates recursively and commits ledger updates to L1 with "anchoring" transactions for finality

## Introduction

CAVEAT: Do not use with real money. Most importantly, the code is unaudited either by humans or LLMs, almost all written by one LLM (extensively *tested* but not *audited*).

For the overall design see the [design paper](./papers/lichen-l2-v0.1.0.pdf).

*Want to try it* - with a wallet, against a hub someone is running: see [Using a wallet](#using-a-wallet). Or run your own hub and sequencer: see [Running a hub](#running-a-hub).

-------

## Using a wallet

A user runs only the wallet: no Bitcoin node, and no coins held in it. It
reads Bitcoin from the network's public Esplora server and talks to a
deployment's two services, its sequencer and its hub. From the operator
you need their two URLs, the deployment's keys (the wallet pins them at
the start: check the ones it shows you), and which statement the hub runs
(`hub-proves`, its real proof, or `hub-proves-mock` on a test stack).

**Deployments.** None runs this repo's code yet. When there is one, it is
listed here with its network, URLs, keys, statement and the app version
that goes with it.

### The app

The wallet as a page in your browser (Mac and Linux), or as an app
(desktop, and Android). Build it from `gui/`:

```sh
git clone https://github.com/lichenbtc-org/vlichen
cd vlichen/gui && cargo build --release -p vlichen-gui
target/release/vlichen-gui          # opens your browser at the wallet
```

On the first tab, choose the network, give the sequencer's and the hub's
URLs, connect, and check the deployment's keys it shows. The desktop app
is `cargo tauri build` in `gui/app/`, the Android one
`gui/app/build-android.sh`: see [gui/README.md](gui/README.md).

### The command line

```sh
git clone https://github.com/lichenbtc-org/vlichen
cd vlichen && cargo build --release -p vlichen-wallet     # target/release/vlichen

alias vw='target/release/vlichen --network mutinynet --wallet alice.json \
  --sequencer <sequencer URL> --hub <hub URL> --statement hub-proves'

vw init                              # a fresh key; check the sequencer key it prints
vw open 100000 --liquidity 100000    # prints the hub's fee and a deposit address to pay
vw balance                           # and how final: firm, anchored, sealed, locked
vw address                           # yours, to be paid to (--shielded: your shielded one)
vw send <address> <amount>           # on the L2; the payee needn't be online
vw ack --wait                        # your payments count once you acknowledge them
vw withdraw <contract> <amount>      # cooperatively: a claim, and the hub folds (0 closes early)
vw exit <contract>                   # if the hub won't: the claim on Bitcoin
```

The deposit is paid from any Bitcoin wallet or faucet; withdrawals,
refunds and your reserve go to the wallet's own payout address (`vw
payout`), or one you name at `open`. Opening uploads about 240 MB (the
garbled circuits the hub keeps).

Shielded payments (`shield`, `unshield`, and `send` to a shielded
address) need a prover for the spend guest: `--spend-prover
r0/spend-prover.sh` runs it in Docker on a Mac. `roots` and
`compare-roots` let wallets check that the sequencer showed them all the
same history. [Using a wallet](docs/USE_A_WALLET.md) has every step, and
what each costs and waits for; [Shielded transfers](docs/SHIELDED.md)
the private side.

## Running a hub

In this L2 the hub runs its own sequencer. [Running a
hub](docs/RUN_A_HUB.md) deploys the Bitcoin node, the sequencer (with its
chain prover, and its anchors on Bitcoin) and the hub, with Docker
Compose, and makes them reachable to users: one deployment per network,
its keys made on first start. To try the stack on your own machine
first, on regtest:

```sh
cp deploy/.env.example deploy/.env    # set NETWORK=regtest, COMPOSE_PROFILES=regtest
deploy/build.sh
deploy/lichen.sh up
deploy/smoke.sh     # a wallet opens a contract, is minted its deposit, and withdraws cooperatively
```

The sequencer's chain prover proves each sealed height valid, and the
hub proves a claim false only in a dispute, with a Groth16 wrap; both
run RISC Zero, the deployment's on x86 Linux with Docker and about 8 GB:
[the
pipeline](docs/explainers/pipeline.md) covers it, and [the
exit](docs/explainers/exit.md) every ending of a dispute, played on
Mutinynet.

For the quickest look, the GUI's devstack runs a whole regtest
deployment in one process (the sequencer, a hub on the mock statement, a
block every 3 seconds, a faucet):

```sh
cd gui && cargo build --release -p vlichen-gui     # with bitcoind on the PATH
target/release/vlichen-devstack --dir /tmp/devstack
target/release/vlichen-gui --devstack /tmp/devstack/devstack.json
```

## The code

On top of lichen's crates (the contract, BABE and the dispute, the hub
and the wallet), pinned at a commit in `Cargo.toml`:

- `crates/ledger`: the ledger: transparent notes in a commitment tree, nullifiers, sealed and signed state roots;
- `crates/sequencer`: the sequencer (`vlichen-sequencer`), a small HTTP service;
- `crates/l2`: the sequencer's API and its client, for the wallet and the hub;
- `crates/shielded`: shielded notes and the spend statement;
- `crates/validity`: the block statement, one sealed height's transition;
- `crates/withdraw`: the hub's statement, that an exit claim is false at an anchored state;
- `crates/proto`: vlichen's part of a contract: the statement's terms and the keys for the dispute's front;
- `crates/hub`, `crates/wallet`: the hub (`vlichen-hub`) and the wallet (`vlichen`);
- `r0/`: the RISC Zero guests (spend, chain, the hub's) and their prover;
- `gui/`: the wallet's page, the devstack and the app;
- `deploy/`: the Docker deployment ([deploy/README.md](deploy/README.md)).

[docs/CODE.md](docs/CODE.md) has the detail, [the
explainers](docs/explainers/README.md) walk through a contract's life on
Mutinynet, and the [1000-ft view](docs/1000-ft-view.md) is the short
version. Tests run on regtest (`cargo test`, with `bitcoind` on the
PATH); `crates/hub/tests/hp_real.rs` (opt-in, `--ignored`) proves the
hub's statement for real.

A hub and its apps upgrade together: the wallet refuses a hub whose
guests aren't its own build's.
