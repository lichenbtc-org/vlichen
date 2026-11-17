# vlichen-gui

The vlichen wallet as a page in your browser, for Mac and Linux laptops,
and as an app (Android, and the desktop): see "The app" below.
`vlichen-gui` serves the page on 127.0.0.1 only and opens your browser at
it. The URL carries a token made fresh at each launch, and every call to
the wallet must carry it, so other pages in your browser can't drive the
wallet.

Four tabs: **Home** (your balance, your contracts and opening one, and
your activity), **Send**, **Receive** (your L2 address, with a QR code)
and **Settings** (the network, the sequencer and the hub, its terms, your
Bitcoin, backup).

## Build

You need git and Rust ([rustup](https://rustup.rs); `rust-toolchain.toml`
picks the version). This directory is its own Cargo workspace, on the
wallet's crates in `../crates` and lichen's by git, at the commit vlichen
pins:

```sh
git clone https://github.com/lichenbtc-org/vlichen
cd vlichen/gui
cargo build --release      # target/release/vlichen-gui and vlichen-devstack
```

A hub tells wallets which guests it proves with, and the GUI refuses one
whose guests aren't its own build's: build from the vlichen commit the hub
runs (Settings shows the hub's version).

## Use

```sh
target/release/vlichen-gui
```

In Settings, choose the network, give the deployment's two URLs (the
sequencer's, port 18300, and the hub's, 18310) and connect. The first
time, the GUI shows the deployment's keys: check them against the ones the
hub's operator publishes, then confirm. It makes a wallet pinned to that
deployment. The chain is read from the network's public Esplora
(mutinynet.com's, or mempool.space's for signet); under "chain source" you
can name others, or a node.

At launch the GUI reconnects to the last hub, and says so under the
connection. The header shows "not updating" if refreshes stop; restart the
GUI if it stays.

The wallet has a Bitcoin address of its own ("Your Bitcoin"), the default
payout address of a contract: withdrawals, refunds and reserves arrive
there, and you can send from it, or pay the next contract's deposit from
it when an opening asks. So beyond a faucet for the first deposit, no
other wallet is needed. A contract with nothing left to withdraw can be
closed early (your reserve back), and one past its end whose default the
hub hasn't posted, you can post yourself.

Its data is in `~/Library/Application Support/vlichen-gui` (Mac) or
`~/.local/share/vlichen-gui` (Linux), or `--dir`. There's one wallet file
per network and sequencer key in `wallets/`; each holds secrets, so keep
the directory private and back it up. It listens on 127.0.0.1:18340
(`--listen` for another port, e.g. beside glichen-gui).

## The app (Android, and the desktop)

The same page and wallet as an app, with Tauri (`app/`): the page in a
webview, the wallet's API a command rather than HTTP. Its wallets and
settings live in the app's own data directory, by its identifier
(`io.github.adamisz.lichen`, kept from the app's earlier name so that
updates keep its wallets).

- **Android** (arm64, sideloaded): `app/build-android.sh` builds
  `target/vlichen-<version>-arm64.apk`. It needs JDK 17, the Android SDK
  (platform 35+, build-tools, an NDK), the Tauri CLI
  (`cargo install tauri-cli`) and Rust's `aarch64-linux-android` target.
  Install with `adb install -r`, or copy the file to the phone and open
  it (allow installs from that app).
- **Desktop:** `cargo tauri build --bundles app` in `app/` (the Mac's
  `target/release/bundle/macos/vlichen.app`), or `cargo tauri dev`.

**The signing key** is `~/.android/debug.keystore` on the Mac that built
the first APK (made by `build-android.sh`, password `android`): an update
installs over the app, keeping its wallet, only if signed with the same
key. Back it up. Signed with another, an update needs the app
uninstalled first, which deletes its wallet (unless backed up, below).

**The networks.** Settings offers the deployments built into the app
(`presets()` in `src/app.rs`), whose keys it knows: its wallet is made
without asking you to check them, and refused if they differ. Or another
deployment by its URLs, whose keys you check.

**Opening a contract** shows its steps: pay the deposit (its QR code and
address; seen, confirmed), garbling (78 instances: 40 s on a 4-core
emulator), the upload of the 10 the hub keeps (239 MB; 3.5 MB/s from the
emulator through the relay), the hub's check, the funding, open. About
two minutes on Mutinynet once the deposit confirms. Keep the app in front
until it's done; if it's stopped, "Resume the opening" on Home finishes
it from the deposit.

**Payments.** A payment arrives within seconds; it is final once the
sender acknowledges it, against the next anchor deep enough (about 20
minutes on Mutinynet). The app acknowledges its own by itself while it
runs, and says so: keep it open until then.

**Backup** (Settings): the wallet's file, through the system's file
picker; restored as the wallet for its deployment if the device has none
(or has the same one). It holds the wallet's key.

## Developing: the devstack

`vlichen-devstack` runs a whole deployment locally in one process: a
regtest bitcoind (`bitcoind` on the PATH, or `LICHEN_BITCOIND`), the
sequencer, a hub on BABE's mock statement (no proving), a block every 3
seconds, and a faucet. Each run starts fresh, as a new deployment.

```sh
target/release/vlichen-devstack --dir /tmp/devstack       # Ctrl-C stops it
target/release/vlichen-gui --devstack /tmp/devstack/devstack.json
```

Opening a contract then offers "Pay it from the devstack's faucet".
`--refuse-folds` makes the hub refuse withdrawals, to see what the GUI
does then.
