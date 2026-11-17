//! vlichen's wallet: a lichen wallet (`lichen_wallet`) with the validity
//! rollup ([`Rollup`]): the account on the L2 (the sequencer's state, its
//! roots checked; our notes, transfers, shielded notes, acknowledgments,
//! exit sets: the traits in [`prelude`]), and its part of each contract
//! (the statement's terms checked, the checkpoint among them; the user's
//! keys for `b` and `E`; the mint claimed at the opening; a withdrawal's
//! claim of definite notes; the dispute's front, [`front`]).

pub mod ack;
pub mod claims;
pub mod exit;
pub mod finality;
pub mod front;
pub mod ledger;
pub mod shielded;

use std::sync::{Arc, Mutex};

use anyhow::{ensure, Context, Result};
use bitcoin::secp256k1::SecretKey;
use bitcoin::{OutPoint, Sequence, Transaction};
use lichen_chain::Chain;
use lichen_contract::{bit_tree, reserve_tree};
use lichen_proto::hp::{Disputed, HpOffer, HpSetup};
use lichen_proto::{Hash, Info, Layer, OpenOffer, Record, Statement};
use lichen_wallet::engine::{ContractSecrets, Engine, Outcome};
use lichen_wallet::{FrontEnd, SweepIn, WalletL2};
use serde::{Deserialize, Serialize};
use vlichen_l2::{Client, Seen};
use vlichen_ledger::Deployment;
use vlichen_proto::{AliceKeys, Keys, SetupTerms, Windows};

pub use exit::{ExitPlan, ExitSet};
pub use ledger::{Owned, Signed};
pub use lichen_wallet::{dispute, engine, hub_call, hub_info, send_from, Ask, Deposit, Held, Opening, Quote, Wallet, PAYOUT_FEERATE};
pub use shielded::SpendProver;

/// The account's methods, master's `Wallet`'s: `use vlichen_wallet::prelude::*`.
pub mod prelude {
    pub use crate::ack::Acks;
    pub use crate::claims::Claims;
    pub use crate::exit::Exits;
    pub use crate::finality::Finalities;
    pub use crate::ledger::Ledger;
    pub use crate::shielded::Shielding;
}

pub use ack::ACK_PATIENCE;

/// A vlichen wallet.
pub type RollupWallet = Wallet<Rollup>;

/// A hub's `GET /info` checked for this wallet (a vlichen hub of our
/// version: the same guests): its deployment.
pub fn check_hub(info: &Info) -> Result<Deployment> {
    Ok(Rollup::create(info)?.deployment)
}

/// The rollup in the wallet file: the sequencer and its deployment (pinned
/// when the wallet was made), the roots seen signed, our spends signed,
/// the exit sets kept, the anchors checked against the chain.
#[derive(Clone, Serialize, Deserialize)]
pub struct Rollup {
    pub sequencer: String,
    pub deployment: Deployment,
    #[serde(default)]
    pub(crate) seen: Arc<Mutex<Seen>>,
    #[serde(default, skip_serializing_if = "empty_sets")]
    pub(crate) exit_sets: Arc<Mutex<Vec<ExitSet>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) signed: Vec<Signed>,
    #[serde(default, with = "ledger::anchors_hex")]
    pub(crate) anchors_checked: Arc<Mutex<Vec<ledger::AnchorChecked>>>,
    #[serde(skip)]
    pub(crate) exit_looked: Arc<Mutex<Option<std::time::Instant>>>,
    /// How shielded spends are proved.
    #[serde(skip)]
    pub spend_prover: SpendProver,
    /// For tests: never acknowledge our own spends.
    #[serde(skip)]
    pub acks_unsent: bool,
}

fn empty_sets(s: &Arc<Mutex<Vec<ExitSet>>>) -> bool {
    s.lock().unwrap().is_empty()
}

impl Rollup {
    /// The rollup of the sequencer at `sequencer` (its deployment as it
    /// announces it, pinned from now on).
    pub fn new(sequencer: &str) -> Result<Rollup> {
        let c = Client::new(sequencer)?;
        Ok(Rollup::pinned(&c.url, c.deployment))
    }
    /// The rollup of `deployment`, its sequencer at `sequencer`.
    pub fn pinned(sequencer: &str, deployment: Deployment) -> Rollup {
        Rollup { sequencer: sequencer.trim_end_matches('/').to_string(), deployment, seen: Default::default(), exit_sets: Default::default(), signed: vec![], anchors_checked: Default::default(), exit_looked: Default::default(), spend_prover: Default::default(), acks_unsent: false }
    }
    /// The sequencer's client.
    pub fn seq(&self) -> Client {
        Client::pinned(&self.sequencer, self.deployment)
    }
}

impl Layer for Rollup {
    fn front(&self, record: &Record) -> Result<Box<dyn lichen_babe::graph::Front>> {
        vlichen_proto::front(record)
    }
    fn journal(&self, record: &Record) -> Result<Vec<u8>> {
        vlichen_proto::journal(record)
    }
}

fn info_l2<T: serde::de::DeserializeOwned>(info: &Info, name: &str) -> Result<T> {
    let v = info.l2.get(name).with_context(|| format!("the hub names no {name}: it runs no vlichen of this wallet's version ({})", vlichen_proto::VERSION))?;
    serde_json::from_value(v.clone()).with_context(|| format!("the hub's {name}"))
}

fn plan(v: &serde_json::Value) -> Result<ExitPlan> {
    serde_json::from_value(v.clone()).context("the exit plan")
}

/// The mint the hub's offer names (`OpenOffer::l2_offer`): its position.
fn mint_index(l2_offer: &serde_json::Value) -> Result<u32> {
    let i = l2_offer.get("mint_index").and_then(|v| v.as_u64()).context("the offer names no mint")?;
    u32::try_from(i).context("a mint's position")
}

impl WalletL2 for Rollup {
    fn create(info: &Info) -> Result<Rollup> {
        let sequencer: String = info_l2(info, "sequencer")?;
        let r = Rollup::pinned(&sequencer, info_l2(info, "deployment")?);
        r.check_info(info)?;
        Ok(r)
    }
    fn check_info(&self, info: &Info) -> Result<()> {
        let d: Deployment = info_l2(info, "deployment")?;
        ensure!(d == self.deployment, "the hub is in another deployment (sequencer {}) than this wallet's (sequencer {})", hex::encode(d.sequencer), hex::encode(self.deployment.sequencer));
        let guests: vlichen_proto::Guests = info_l2(info, "guests")?;
        let commit = info.commit.as_deref().map(|c| format!(" ({c})")).unwrap_or_default();
        ensure!(guests == vlichen_proto::Guests::ours(), "the hub proves with other guests than this wallet's: it runs vlichen {}{commit}, this wallet {}; use a wallet of the hub's version", if info.version.is_empty() { "?" } else { &info.version }, vlichen_proto::VERSION);
        Ok(())
    }
    fn version() -> String {
        vlichen_proto::VERSION.to_string()
    }

    fn setup_terms(_w: &Wallet<Self>, _cid: u32, _quote: &Quote, _owner: &Hash) -> Result<serde_json::Value> {
        // the hub fixes them; she checks them
        Ok(serde_json::Value::Null)
    }
    fn check_setup(w: &Wallet<Self>, chain: &Chain, setup: &HpSetup, _asked: &serde_json::Value) -> Result<()> {
        let t: SetupTerms = serde_json::from_value(setup.l2.clone()).context("the setup's terms")?;
        ensure!(t.margin == vlichen_proto::margin(chain.network), "the hub's margin, {} blocks, isn't the network's", t.margin);
        if let Some(c) = &t.checkpoint {
            c.check(chain, 2016)?;
        }
        let d = vlichen_proto::contract_word(&w.l2.deployment, t.checkpoint.as_ref(), bitcoin::Amount::from_sat(setup.unit))?;
        ensure!(d == t.d, "the setup's D isn't the deployment's, at its checkpoint and unit");
        Ok(())
    }
    fn alice_keys(s: &ContractSecrets) -> serde_json::Value {
        serde_json::to_value(front::alice_keys(s)).expect("keys serialize")
    }
    fn setup_journal(cid: u32, setup: &HpSetup, _keys: &serde_json::Value) -> Result<Vec<u8>> {
        let t: SetupTerms = serde_json::from_value(setup.l2.clone()).context("the setup's terms")?;
        Ok(vlichen_proto::journal_of(cid, &t.d, &setup.deposit, t.margin).to_vec())
    }
    fn record_keys(_w: &Wallet<Self>, setup: &HpSetup, keys: &serde_json::Value, offer: &HpOffer, _quote: &Quote) -> Result<serde_json::Value> {
        let windows: Windows = serde_json::from_value(offer.l2.clone()).context("the front's windows in the hub's offer")?;
        let alice: AliceKeys = serde_json::from_value(keys.clone())?;
        Ok(serde_json::to_value(Keys { alice, terms: serde_json::from_value(setup.l2.clone())?, windows })?)
    }
    fn check_offer(w: &Wallet<Self>, record: &Record, offer: &OpenOffer) -> Result<()> {
        use prelude::Ledger;
        let k = vlichen_proto::keys(record)?;
        let want = vlichen_proto::windows(record.network, offer.t_close);
        ensure!(k.windows == want, "the front's windows {:?}, not the network's {want:?}", k.windows);
        // D − fee, minted to a key we can spend once this funding is out:
        // sealed, before we sign the funding
        let owner = vlichen_proto::mint_owner(&record.l2_address, &offer.mint_point)?;
        w.wait_mint(mint_index(&offer.l2_offer)?, &owner, record.minted())
    }
    fn minted(w: &mut Wallet<Self>, held: &Held, t: &SecretKey) -> Result<()> {
        use prelude::Ledger;
        let locked = held.mint.as_ref().context("no mint offered")?;
        w.claim_mint_at(mint_index(&locked.l2)?, &held.record.l2_address, t)
    }

    fn withdrawal(w: &mut Wallet<Self>, chain: &Chain, cid: u32, units: u32) -> Result<serde_json::Value> {
        use prelude::Ledger;
        Ok(serde_json::to_value(w.withdrawal_claim(chain, cid, units)?)?)
    }
    fn plan_b(p: &serde_json::Value) -> Result<u32> {
        Ok(plan(p)?.b)
    }
    fn plan_claim(p: &serde_json::Value) -> Result<String> {
        Ok(plan(p)?.posted)
    }
    fn rest_to(_w: &Wallet<Self>) -> Option<Hash> {
        None
    }
    fn folded(_w: &mut Wallet<Self>, _cid: u32, _units: u32) -> Result<()> {
        Ok(())
    }
    fn exit_plan(w: &mut Wallet<Self>, chain: &Chain, cid: u32) -> Result<serde_json::Value> {
        use prelude::Claims;
        Ok(serde_json::to_value(w.hp_exit_plan(chain, cid)?)?)
    }
    fn exited(_w: &mut Wallet<Self>, _cid: u32) -> Result<()> {
        Ok(())
    }
    fn unlocked(_w: &mut Wallet<Self>, _k: usize, _z: &[u8]) -> Result<Option<u64>> {
        Ok(None)
    }
    fn run_front(e: &mut Engine<'_>, chain: &Chain, d: &Disputed, p: &serde_json::Value) -> Result<FrontEnd> {
        front::run(e, chain, d, &plan(p)?)
    }
    fn payout_inputs(w: &Wallet<Self>, _chain: &Chain, held: &Held, _statement: &Statement, _outcome: &Outcome, payout: &Transaction, b: u32) -> Result<(Vec<SweepIn>, Vec<Vec<u8>>, u16)> {
        let txid = payout.compute_txid();
        let k = vlichen_proto::keys(&held.record)?;
        let g = vlichen_proto::front(&held.record)?;
        let core = g.core();
        let (t, ctx) = (core.terms, core.ctx());
        let delta = core.params.delta;
        let at_delta = Sequence::from_height(delta);
        let mut ins: Vec<SweepIn> = vec![];
        // her outputs: the bits set in b, then the reserves
        for i in t.low_bits..t.bits {
            if (b >> i) & 1 == 1 {
                let vout = i - t.low_bits;
                ins.push((OutPoint { txid, vout }, payout.output[vout as usize].clone(), at_delta, Some((bit_tree(&ctx, i, &k.alice.b_key)?, format!("alice_bit_{i}"), true))));
            }
        }
        let vout = t.bits - t.low_bits;
        ins.push((OutPoint { txid, vout }, payout.output[vout as usize].clone(), at_delta, Some((reserve_tree(&ctx, t, &k.alice.b_key)?, "alice_reserve".to_string(), false))));
        Ok((ins, front::b_wire(&w.secrets(&held.nonce), b)?, delta))
    }
}

/// A wallet at `path` with a fresh key, on the sequencer at `sequencer`
/// (no hub named yet: each opening names its own).
pub fn create(path: &std::path::Path, sequencer: &str) -> Result<RollupWallet> {
    Wallet::create_with(path, "", Rollup::new(sequencer)?)
}

/// The wallet at `path`, its sequencer at `sequencer`.
pub fn open(path: &std::path::Path, sequencer: &str) -> Result<RollupWallet> {
    let mut w = Wallet::<Rollup>::open(path, "")?;
    w.l2.sequencer = sequencer.trim_end_matches('/').to_string();
    Ok(w)
}
