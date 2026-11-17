//! vlichen's part of a lichen contract: the hub's statement's terms (the
//! contract's word `D`: the deployment, in an anchored deployment the
//! contract's checkpoint and unit too; the margin), the user's keys for the
//! dispute's front ([`front`]), and how both sides read `request`. Also
//! what an anchored deployment rests on: the checkpoint a contract opens
//! at, the headers on an anchor, a mint behind the funding's adaptor point,
//! and a request proved in its block ([`exit`]). The rest of the contract
//! is lichen's (`lichen_proto`).

pub mod exit;
pub mod front;

use anyhow::{bail, ensure, Context, Result};
use bitcoin::secp256k1::PublicKey;
use bitcoin::Amount;
use lichen_proto::{Hash, Net, Record};
use lichen_wots::{WotsParams, WotsPublic};
use serde::{Deserialize, Serialize};
use vlichen_ledger::Deployment;

pub use front::{HubProves, Windows};

/// vlichen's version (the workspace's): what its hub and wallet report.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The hub's statement guest's image id, hex (`r0/guests/hp.id`).
pub const HP_IMAGE_ID: &str = include_str!("../../../r0/guests/hp.id");

/// The guests a build proves and checks receipts with: each one's image
/// id, hex (`r0/guests/<g>.id`). A wallet proves with its own, and a
/// contract's statement pins them when it opens, so a hub's must be the
/// same as the wallet's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guests {
    pub spend: String,
    pub chain: String,
    #[serde(default)]
    pub hp: String,
}

impl Guests {
    /// This build's.
    pub fn ours() -> Guests {
        let id = |s: &str| s.trim().to_string();
        Guests { spend: id(vlichen_ledger::SPEND_IMAGE_ID), chain: id(vlichen_ledger::CHAIN_IMAGE_ID), hp: id(HP_IMAGE_ID) }
    }
}

/// How many blocks' work, at the checkpoint's difficulty, a withdrawal
/// shows on top of the anchor of its root.
pub const ANCHOR_DEPTH: u128 = 10;

/// The Bitcoin block a contract opened at, in an anchored deployment: its
/// difficulty fixes the work `W` a withdrawal shows on top of its anchor.
/// Each side checks it against its own view of the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub height: u32,
    pub hash: bitcoin::BlockHash,
    pub bits: u32,
}

impl Checkpoint {
    /// The checkpoint at the chain's tip.
    pub fn tip(chain: &lichen_chain::Chain) -> Result<Checkpoint> {
        let height = chain.height()?;
        let hash = chain.block_hash(height)?;
        let header = chain.header(&hash)?;
        Ok(Checkpoint { height, hash, bits: u32::from_le_bytes(header[72..76].try_into().unwrap()) })
    }
    /// `W`: [`ANCHOR_DEPTH`] blocks of its difficulty.
    pub fn work(&self) -> u128 {
        ANCHOR_DEPTH.saturating_mul(vlichen_ledger::vlichen_validity::btc::work(self.bits))
    }
    /// Check it against `chain`: its block on the best chain with these
    /// bits, at most `stale` blocks below the tip.
    pub fn check(&self, chain: &lichen_chain::Chain, stale: u32) -> Result<()> {
        ensure!(chain.block_hash(self.height)? == self.hash, "the checkpoint {} isn't the best chain's block {}", self.hash, self.height);
        let header = chain.header(&self.hash)?;
        ensure!(u32::from_le_bytes(header[72..76].try_into().unwrap()) == self.bits, "the checkpoint's bits aren't its block's");
        ensure!(chain.height()? <= self.height + stale, "the checkpoint, block {}, is stale", self.height);
        Ok(())
    }
}

/// Headers on top of block `from` (internal order), from the chain, until
/// their work reaches `work`: what a withdrawal shows on top of its
/// anchor. An error, saying how far it has got, if not there yet.
pub fn headers_for(chain: &lichen_chain::Chain, from: &Hash, work: u128) -> Result<Vec<[u8; 80]>> {
    use bitcoin::hashes::Hash as _;
    let block = bitcoin::BlockHash::from_byte_array(*from);
    let mut out = vec![];
    let mut sum = 0u128;
    for h in chain.headers_after(&block, 4 * ANCHOR_DEPTH as usize)? {
        sum = sum.saturating_add(vlichen_ledger::vlichen_validity::btc::work(u32::from_le_bytes(h[72..76].try_into().unwrap())));
        out.push(h);
        if sum >= work && !out.is_empty() {
            return Ok(out);
        }
    }
    bail!("the anchor isn't deep enough yet: {} blocks on it, {sum} of the {work} work the contract asks", out.len())
}

/// The journal's `D` a contract pins: the deployment's id, or in an
/// anchored deployment (a genesis seal) the contract's, of the genesis
/// seal, the checkpoint's `W` and the contract's unit too.
pub fn contract_word(d: &Deployment, checkpoint: Option<&Checkpoint>, unit: Amount) -> Result<Hash> {
    match (d.seal, checkpoint) {
        (None, None) => Ok(d.id()),
        (Some(seal), Some(c)) => Ok(vlichen_ledger::lichen_withdraw_contract_id(&d.id(), &seal, c.work(), unit.to_sat())),
        (Some(_), None) => bail!("an anchored deployment's contract needs a checkpoint"),
        (None, Some(_)) => bail!("a checkpoint in a deployment that doesn't anchor"),
    }
}

/// The owner of a mint for `l2_address` behind the funding's adaptor point
/// ([`vlichen_ledger::mint_key`]).
pub fn mint_owner(l2_address: &Hash, mint_point: &PublicKey) -> Result<Hash> {
    vlichen_ledger::mint_key(l2_address, &mint_point.serialize())
}

/// A Bitcoin outpoint as the statement takes it.
pub fn statement_outpoint(o: &bitcoin::OutPoint) -> vlichen_ledger::OutPoint {
    use bitcoin::hashes::Hash as _;
    vlichen_ledger::OutPoint { txid: o.txid.to_byte_array(), vout: o.vout }
}

/// The statement's terms in a setup (`HpSetup::l2`), the hub's: the
/// margin, the checkpoint (in an anchored deployment) and the contract's
/// word `D` they make.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupTerms {
    pub margin: u32,
    pub checkpoint: Option<Checkpoint>,
    #[serde(with = "lichen_babe::hexbytes::fixed")]
    pub d: Hash,
}

/// The user's one-time keys for `b` and `E` (`HpCommit::keys`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliceKeys {
    pub b_key: WotsPublic,
    pub e_key: WotsPublic,
}

impl AliceKeys {
    /// Of the parameters the front takes.
    pub fn check(&self) -> Result<()> {
        ensure!(self.b_key.params == WotsParams::for_bytes(4) && self.b_key.digits.len() == 10, "a key for b of other parameters");
        ensure!(self.e_key.params == WotsParams::for_bytes(32), "a key for E of other parameters");
        Ok(())
    }
}

/// A contract's keys (`HpKeys::l2`): hers, the statement's terms, the
/// front's windows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Keys {
    pub alice: AliceKeys,
    pub terms: SetupTerms,
    pub windows: Windows,
}

/// The front's window on a network (`w_H`, for the hub's assertion) and
/// her request's latest (`t_a`, before `T_close`).
pub fn windows(net: Net, t_close: u32) -> Windows {
    let w_h = match net {
        // blocks every 150 ms in the tests: the request's block needs the
        // contract's work on it (ten blocks) before the hub can assert
        Net::Regtest => 80,
        // 30 s blocks: 2 h
        Net::Mutinynet => 240,
        // 10 min blocks: 6 h
        Net::Signet => 36,
        Net::Testnet4 => 72,
    };
    let tail = lichen_proto::hp::windows(net);
    Windows { w_h, t_a: t_close.saturating_sub(u32::from(w_h) + u32::from(tail.delta2) + u32::from(tail.delta1)) }
}

/// The hub's statement's margin on a network, in blocks: covers her
/// request's confirmation.
pub fn margin(net: Net) -> u32 {
    match net {
        Net::Regtest => 6,
        Net::Mutinynet => 20,
        Net::Signet | Net::Testnet4 => 6,
    }
}

/// The journal of contract `c` in `D`, of deposit coin `deposit` and
/// margin `margin`: `c ‖ D ‖ H(O_D ‖ margin)`.
pub fn journal_of(c: u32, d: &Hash, deposit: &bitcoin::OutPoint, margin: u32) -> [u8; vlichen_withdraw::hp::JOURNAL_LEN] {
    vlichen_withdraw::hp::journal(c, d, &statement_outpoint(deposit), margin)
}

/// A record's keys, typed.
pub fn keys(record: &Record) -> Result<Keys> {
    record.dispute.l2()
}

/// The statement's journal for a contract ([`lichen_proto::Layer::journal`]).
pub fn journal(record: &Record) -> Result<Vec<u8>> {
    let k = keys(record)?;
    Ok(journal_of(record.cid(), &k.terms.d, &record.dispute.deposit, k.terms.margin).to_vec())
}

/// The front of a contract's graph, from its record ([`lichen_proto::Layer::front`]).
pub fn front(record: &Record) -> Result<Box<dyn lichen_babe::graph::Front>> {
    let k = keys(record)?;
    Ok(Box::new(HubProves::new(front::Spec { terms: record.terms, params: lichen_proto::params(record.network), pubs: record.pubs.clone(), b_key: k.alice.b_key.clone(), e_key: k.alice.e_key.clone(), windows: k.windows })?))
}

/// `request`'s claim: `b`, `E` and the claim's bytes.
pub fn requested(request: &bitcoin::Transaction) -> Result<(u32, [u8; 32], Vec<u8>)> {
    let (b, e, posted) = HubProves::read_request(&lichen_proto::setup::link_args(request))?;
    let b = u32::from_be_bytes(b.message().try_into().map_err(|_| anyhow::anyhow!("b's signature signs 4 bytes"))?);
    let e = e.message().try_into().map_err(|_| anyhow::anyhow!("E's signature signs 32 bytes"))?;
    Ok((b, e, posted))
}

/// `request`'s signature on `b`, as the payout's bit outputs take it.
pub fn request_b(request: &bitcoin::Transaction) -> Result<(u32, Vec<Vec<u8>>)> {
    let (b, _, _) = HubProves::read_request(&lichen_proto::setup::link_args(request))?;
    let v = u32::from_be_bytes(b.message().try_into().map_err(|_| anyhow::anyhow!("b's signature signs 4 bytes"))?);
    Ok((v, HubProves::b_wire(&b)))
}

/// A dispute's link `name`, from the chain.
pub fn link(chain: &lichen_chain::Chain, d: &lichen_proto::hp::Disputed, name: &str) -> Result<bitcoin::Transaction> {
    chain.get_tx(&d.txid(name)).with_context(|| format!("{name}, on chain"))
}
