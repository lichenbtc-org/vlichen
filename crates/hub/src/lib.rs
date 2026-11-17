//! vlichen's hub: the validity rollup (`vlichen_l2`: a sequencer, the hub
//! its issuer) on a lichen hub (`lichen_hub`), which runs the contracts.
//!
//! Its part of a contract ([`Rollup`], a [`HubL2`]): the statement's terms
//! (the margin; the checkpoint the contract opens at, in an anchored
//! deployment; the contract's word `D` they make), a mint of `D − fee` at
//! the offer, locked to the funding's adaptor point, a withdrawal's claim
//! recorded on the L2 (its notes retired), and the dispute's front: when
//! `request` confirms, the hub holds the claimed notes at the sequencer and
//! decides the claim at `A*`, the last anchor before its cutoff. False: the
//! hub proves it (the ground found, `vlichen_withdraw::hp`) and lichen
//! asserts. True: the hub records it on the L2 and `request_timeout` pays
//! her.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, ensure, Context, Result};
use bitcoin::hashes::Hash as _;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{OutPoint, Sequence, Txid};
use lichen_contract::bit_tree;
use lichen_hub::{Bits, Ctx, FrontStep, HubL2, ProverInput};
pub use lichen_hub::OpeningFault;
use lichen_proto::hp::{Disputed, HpSetup, HpSetupRequest};
use lichen_proto::{files, Hash, Layer, OpenRequest, Record};
use tiny_http::Method;
use vlichen_l2::Client;
use vlichen_ledger::k256::schnorr::SigningKey;
use vlichen_ledger::vlichen_validity::ack::{self, Ack};
use vlichen_ledger::vlichen_validity::TNote;
use vlichen_ledger::{note, xonly, Mint, Tx};
use vlichen_proto::{statement_outpoint, AliceKeys, Checkpoint, HubProves, Keys, SetupTerms};
use vlichen_withdraw::hp::{Bad, ChainSet, Claim, Counts, Ground, HubInput, Last, Opening, SealSpend, Terms, Why};

pub use lichen_hub::{Config, Hub};

/// The rollup at the hub: its sequencer, its L2 key (it mints and records
/// exits), the roots it has seen signed, and the proofs' inputs decided.
pub struct Rollup {
    pub seq: Client,
    l2_key: SigningKey,
    dir: PathBuf,
    seen: Mutex<vlichen_l2::Seen>,
    /// Each dispute's input, once the claim is found false: why, and the
    /// input (proved from then on).
    decided: Mutex<HashMap<u32, (String, HubInput)>>,
}

/// A dispute's front at the hub: `request` confirmed at `at`; `recorded`
/// once the claim is found to hold (recorded on the L2).
pub struct Front {
    at: u32,
    recorded: bool,
}

struct HeldNote {
    index: u32,
    shielded: bool,
    opening: Opening,
    value: u64,
    nf: Hash,
    key: Hash,
}

enum Decision {
    Assert(Box<HubInput>, String),
    Holds,
    Wait(String),
}

fn tnote(n: &vlichen_ledger::Note) -> TNote {
    TNote { owner: n.owner, value: n.value, memo: n.memo, rho: n.rho }
}

impl Rollup {
    /// The rollup of the sequencer at `sequencer`, whose issuer must be
    /// `l2_key`; its files in `dir` (`deployment.json`, the sequencer's
    /// deployment pinned at first start; `seen.json`).
    pub fn open(dir: &Path, sequencer: &str, l2_key: SigningKey) -> Result<Rollup> {
        std::fs::create_dir_all(dir)?;
        let seq = Client::new(sequencer)?;
        ensure!(seq.deployment.hub == xonly(&l2_key), "the sequencer's issuer is {}, not this hub's key {}", hex::encode(seq.deployment.hub), hex::encode(xonly(&l2_key)));
        // the sequencer may be another machine's: pin its key on first
        // start, and refuse another after
        let pin = dir.join("deployment.json");
        if pin.exists() {
            let d: vlichen_ledger::Deployment = serde_json::from_slice(&std::fs::read(&pin)?).context("deployment.json")?;
            ensure!(d == seq.deployment, "the sequencer at {sequencer} has key {}, not {} as pinned in {}", hex::encode(seq.deployment.sequencer), hex::encode(d.sequencer), pin.display());
        } else {
            files::write_atomic(&pin, &serde_json::to_vec_pretty(&seq.deployment)?)?;
        }
        let seen = match std::fs::read(dir.join("seen.json")) {
            Ok(b) => serde_json::from_slice(&b).context("seen.json")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Rollup { seq, l2_key, dir: dir.to_path_buf(), seen: Mutex::new(seen), decided: Mutex::new(HashMap::new()) })
    }

    /// The L2's state, its signed roots checked against those seen: a root
    /// at a height seen before other than the one seen is an equivocation,
    /// kept as evidence, and the hub folds nothing after.
    pub fn state(&self, ctx: &dyn Ctx) -> Result<vlichen_l2::State> {
        let state = self.seq.state()?;
        let mut seen = self.seen.lock().unwrap();
        match seen.record(&state.sealed) {
            Ok(true) => files::write_atomic(&self.dir.join("seen.json"), &serde_json::to_vec(&*seen)?)?,
            Ok(false) => {}
            Err(e) => {
                let path = self.dir.join("equivocation.json");
                files::write_atomic(&path, &serde_json::to_vec_pretty(&e)?)?;
                ctx.note(format!("ALARM: {e}; the evidence: {}", path.display()));
                anyhow::bail!("{e}: it signed two histories; the hub folds nothing until this is resolved");
            }
        }
        Ok(state)
    }

    /// A claim of `b` units on contract `record` recorded on the L2 (its
    /// notes retired): each note live, unlocked or its spend acknowledged
    /// in the claim, and hers.
    fn record_claim(&self, ctx: &dyn Ctx, record: &Record, b: u32, posted: &[u8]) -> Result<()> {
        let cid = record.cid();
        let claim = Claim::from_bytes(posted).map_err(|e| anyhow!("the claim: {e:?}"))?;
        ensure!(claim.well_formed(), "the claim isn't well formed");
        let deposit = statement_outpoint(&record.dispute.deposit);
        let e = claim.fingerprint(cid, &deposit, b);
        let state = self.state(ctx)?;
        // a locked note only with its spend's acknowledgment, carried
        let unlocked = |i: u32| -> Result<()> {
            if state.locked(i) {
                let t = state.leaf(&ack::lock_key(i));
                ensure!(claim.acks.iter().any(|a| a.txid == t), "the note at {i} is locked: its spend isn't acknowledged, and the claim carries no acknowledgment of it");
            }
            Ok(())
        };
        let mut notes = vec![];
        for &i in &claim.positions {
            let n = *state.notes.get(i as usize).and_then(|l| l.note()).with_context(|| format!("position {i} isn't a transparent note"))?;
            ensure!(!state.is_spent(&n), "the note at {i} is spent");
            unlocked(i)?;
            notes.push(n);
        }
        let mut total: u64 = notes.iter().map(|n| n.value).sum();
        if let Some(sc) = &claim.shielded {
            for n in &sc.notes {
                let i = n.index;
                ensure!(state.notes.get(i as usize).map(|l| l.leaf()) == Some(sc.leaf(n)), "position {i} isn't the shielded note the claim opens");
                ensure!(!state.spent(&sc.nullifier(n)), "the note at {i} is spent");
                unlocked(i)?;
                total += n.value;
            }
        }
        ensure!(total / record.terms.unit.to_sat() >= u64::from(b), "the notes are worth {total} sats, less than {b} units");
        let tnotes: Vec<TNote> = notes.iter().map(tnote).collect();
        let (_, sigs) = claim.record(&tnotes, &e).context("a note's owner didn't sign the claim")?;
        let spends = notes.into_iter().zip(&claim.positions).map(|(note, &index)| vlichen_ledger::Spend { note, index }).collect();
        let acks = claim.acks.iter().map(vlichen_ledger::Ack::of).collect();
        let tx = Tx::Exit(vlichen_ledger::Exit::new(&self.l2_key, &self.seq.deployment, cid, deposit, b, spends, claim.shielded.clone(), acks, sigs));
        self.seq.submit(&tx).context("recording the claim on the L2")?;
        ctx.note(format!("contract {cid}: the user's withdrawal claim of {b} units recorded on the L2: its notes are retired"));
        Ok(())
    }

    /// Hold the notes `request` claims at the sequencer (once): no spend of
    /// them is sequenced while the claim is decided.
    fn hold(&self, ctx: &dyn Ctx, d: &Disputed) -> Result<()> {
        let req = vlichen_proto::link(ctx.chain(), d, "request")?;
        let (_, _, posted) = vlichen_proto::requested(&req)?;
        let Ok(claim) = Claim::from_bytes(&posted) else { return Ok(()) };
        let state = self.seq.state()?;
        let mut nfs: Vec<Hash> = claim.positions.iter().filter_map(|i| state.note(*i)).map(vlichen_ledger::nullifier).collect();
        if let Some(sc) = &claim.shielded {
            nfs.extend(sc.notes.iter().map(|n| sc.nullifier(n)));
        }
        if !nfs.is_empty() {
            self.seq.hold(&self.l2_key, &nfs)?;
        }
        ctx.note_once(format!("contract {}: holding the notes the request claims", d.record.cid()));
        Ok(())
    }

    /// A claim found to hold, recorded on the L2: its notes retired.
    fn record(&self, ctx: &dyn Ctx, d: &Disputed) -> Result<()> {
        let cid = d.record.cid();
        let req = vlichen_proto::link(ctx.chain(), d, "request")?;
        let (b, e, posted) = vlichen_proto::requested(&req)?;
        let claim = Claim::from_bytes(&posted).map_err(|r| anyhow!("the claim: {r:?}"))?;
        ctx.note(format!("contract {cid}: the user's exit claim of {b} units holds: no assertion; request_timeout will pay her"));
        let state = self.seq.state()?;
        let notes: Vec<TNote> = claim.positions.iter().map(|i| state.notes.get(*i as usize).and_then(|l| l.note()).map(tnote).context("a claimed note isn't transparent")).collect::<Result<_>>()?;
        let mut nfs: Vec<Hash> = notes.iter().map(TNote::nullifier).collect();
        if let Some(sc) = &claim.shielded {
            nfs.extend(sc.notes.iter().map(|n| sc.nullifier(n)));
        }
        if let Err(e) = self.seq.hold(&self.l2_key, &nfs) {
            ctx.note(format!("contract {cid}: holding the claimed notes: {e:#}"));
        }
        let deposit = statement_outpoint(&d.record.dispute.deposit);
        let Some((notes, sigs)) = claim.record(&notes, &e) else { return Ok(()) };
        let notes = notes.into_iter().map(|(n, index)| vlichen_ledger::Spend { note: vlichen_ledger::Note { owner: n.owner, value: n.value, memo: n.memo, rho: n.rho }, index }).collect();
        let acks = claim.acks.iter().map(vlichen_ledger::Ack::of).collect();
        let tx = Tx::Exit(vlichen_ledger::Exit::new(&self.l2_key, &self.seq.deployment, cid, deposit, b, notes, claim.shielded.clone(), acks, sigs));
        match self.seq.submit(&tx) {
            Ok(_) => ctx.note(format!("contract {cid}: the exit claim recorded on the L2: its notes are retired")),
            Err(e) => ctx.note(format!("contract {cid}: the exit record: {e:#}")),
        }
        Ok(())
    }

    /// The other contracts' requests on chain: each one's record, `b` and
    /// claim.
    fn other_claims(&self, ctx: &dyn Ctx, cid: u32) -> Vec<(Record, u32, Claim)> {
        let mut out = vec![];
        for (r, _) in ctx.contracts().into_iter().filter(|(r, _)| r.cid() != cid) {
            let Ok(d) = Disputed::new(&r, &ctx.cfg().statement, self) else { continue };
            let Some(req) = ctx.chain().get_tx(&d.txid("request")) else { continue };
            let Ok((b, _, posted)) = vlichen_proto::requested(&req) else { continue };
            if let Ok(c) = Claim::from_bytes(&posted) {
                out.push((r, b, c));
            }
        }
        out
    }

    /// The contract a leaf `exited(c)` names, among the hub's.
    fn exited(ctx: &dyn Ctx, leaf: &Hash) -> Option<u32> {
        ctx.contracts().into_iter().map(|(r, _)| r.cid()).find(|c| vlichen_ledger::vlichen_validity::smt::exited(*c) == *leaf)
    }

    /// A claimed note claimed through another contract too, signed by its
    /// key for that claim.
    fn second(&self, ctx: &dyn Ctx, cid: u32, held: &[HeldNote]) -> Option<(Why, String)> {
        for (r, b2, other) in self.other_claims(ctx, cid) {
            let d2 = statement_outpoint(&r.dispute.deposit);
            let e2 = other.fingerprint(r.cid(), &d2, b2);
            for (w, h) in held.iter().enumerate() {
                if other.claims(h.index, h.shielded) && other.signed_by(&h.key, &e2).is_some() {
                    let why = Why::Second { which: w as u8, opening: h.opening.clone(), other_c: r.cid(), other_deposit: d2, other_b: b2, other: other.clone() };
                    return Some((why, format!("note {} was claimed through contract {} too", h.index, r.cid())));
                }
            }
        }
        None
    }

    /// Mint a sat to the hub (once): a transaction for the sequencer to
    /// seal, so that an anchor follows the cutoff.
    fn nudge(&self, ctx: &dyn Ctx, cid: u32) -> Result<()> {
        let once = format!("contract {cid}: minting a sat to the hub, for an anchor after the cutoff");
        ctx.note_once(once);
        let owner = xonly(&self.l2_key);
        self.seq.submit(&Tx::Mint(Mint::new(&self.l2_key, &self.seq.deployment, vec![note(owner, 1, 0)])))?;
        Ok(())
    }

    /// Decide the claim `request` makes, at `A*`: the ground it is false
    /// on, or that it holds, or what is still to come.
    fn decide(&self, ctx: &dyn Ctx, d: &Disputed, at: u32, recorded: bool) -> Result<Decision> {
        let chain = ctx.chain();
        let record = &d.record;
        let cid = record.cid();
        let k = vlichen_proto::keys(record)?;
        let margin = k.terms.margin;
        let deposit = statement_outpoint(&record.dispute.deposit);
        let req_id = d.txid("request");
        let req = chain.get_tx(&req_id).context("the request")?;
        let (b, e, posted) = vlichen_proto::requested(&req)?;
        let checkpoint = k.terms.checkpoint.as_ref().context("a contract's checkpoint")?;
        let dep = self.seq.deployment;
        let terms = Terms { sequencer: dep.sequencer, hub: dep.hub, genesis: dep.seal.context("an anchored deployment")?, work: checkpoint.work(), unit: record.terms.unit.to_sat() };
        // request in its block, with the contract's work on it
        let funding = chain.get_tx(&record.funding.txid).context("the funding")?;
        let request = vlichen_proto::exit::request_proof(chain, &funding, &req_id)?;
        let req_block = vlichen_ledger::vlichen_validity::btc::Header(request.header).hash();
        let headers = match vlichen_proto::headers_for(chain, &req_block, terms.work) {
            Ok(h) => h,
            Err(e) => return Ok(Decision::Wait(format!("deciding the claim: {e:#}"))),
        };
        let input = |ground: Ground| Box::new(HubInput { c: cid, deposit, margin, terms, request: request.clone(), headers: headers.clone(), ground });
        let claim = match Claim::from_bytes(&posted) {
            Ok(c) if c.well_formed() && c.fingerprint(cid, &deposit, b) == e => c,
            _ => return Ok(Decision::Assert(input(Ground::Malformed), "the claim is malformed".into())),
        };
        let state = self.seq.state()?;
        let cutoff = at.saturating_sub(margin);
        // A*: the last anchor recorded whose block is at most the cutoff
        let mut star = None;
        for (i, a) in state.anchors.iter().enumerate() {
            if chain.block_height(&bitcoin::BlockHash::from_byte_array(a.anchor.block))?.is_some_and(|h| h <= cutoff) {
                star = Some(i);
            }
        }
        let Some(i) = star else { return Ok(Decision::Wait("no anchor before the claim's cutoff".into())) };
        let a = state.anchors[i];
        let Some(next) = state.anchors.get(i + 1) else {
            self.nudge(ctx, cid)?;
            return Ok(Decision::Wait("waiting for an anchor after the cutoff, to show A* the last before it".into()));
        };
        let succ = Txid::from_byte_array(next.anchor.seal.txid);
        let spend = vlichen_proto::exit::in_block(chain, &succ)?;
        let a_block = bitcoin::BlockHash::from_byte_array(a.anchor.block);
        let a_height = chain.block_height(&a_block)?.context("A*'s block isn't on the chain")?;
        let s_height = chain.block_height(&spend.block)?.context("the seal's spend isn't on the chain")?;
        ensure!(s_height > cutoff, "the anchor after A* is before the cutoff (not recorded yet?)");
        let star_headers = chain.headers_after(&a_block, (chain.height()? - a_height) as usize)?;
        let h2 = a.anchor.height;
        let s2 = state.sealed.iter().find(|s| s.height == h2).context("A* isn't sealed")?;
        let (chain_root, recent, hist) = state.chain_at(a.from)?;
        let last = Last {
            state: vlichen_withdraw::Anchored {
                height: h2,
                nullifiers: s2.nullifiers,
                signature: s2.signature.as_slice().try_into().context("a signature of 64 bytes")?,
                sequencer: dep.sequencer,
                hub: dep.hub,
                chain_height: a.from,
                chain_root,
                recent,
                hist,
                anchor: a.anchor,
                headers: star_headers,
                work: terms.work,
                unit: terms.unit,
            },
            tree: s2.tree,
            seal: SealSpend { tx: spend.tx, at: s_height - a_height - 1, index: spend.index, path: spend.path, coinbase: spend.coinbase, coinbase_path: spend.coinbase_path },
        };
        let at_star = |why: Why| input(Ground::At { last: last.clone(), why });
        // each claimed note at A*, opened: a transparent note at its
        // position, or the shielded note the claim opens
        let tree = state.tree();
        let mut held: Vec<HeldNote> = vec![];
        for w in 0..claim.len() {
            let index = claim.position(w).context("a claimed note's position")?;
            let path = tree.path_at(index, s2.size);
            let leaf = state.notes.get(index as usize).filter(|_| index < s2.size);
            let found = match (claim.shielded_note(w), &claim.shielded) {
                (Some(n), Some(sc)) => (leaf.map(|l| l.leaf()) == Some(sc.leaf(n))).then(|| HeldNote { index, shielded: true, opening: Opening { note: None, path: path.clone() }, value: n.value, nf: sc.nullifier(n), key: sc.x }),
                _ => leaf.and_then(|l| l.note()).map(|n| HeldNote { index, shielded: false, opening: Opening { note: Some(tnote(n)), path: path.clone() }, value: n.value, nf: vlichen_ledger::nullifier(n), key: n.owner }),
            };
            match found {
                Some(h) => held.push(h),
                None => {
                    let leaf = leaf.map_or(vlichen_ledger::vlichen_validity::tree_empties()[0], |l| l.leaf());
                    return Ok(Decision::Assert(at_star(Why::NotANote { which: w as u8, leaf, path }), format!("position {index} isn't the note claimed at A* (height {h2})")));
                }
            }
        }
        for (w, h) in held.iter().enumerate() {
            if claim.signed_by(&h.key, &e).is_none() {
                return Ok(Decision::Assert(at_star(Why::NotHers { which: w as u8, opening: h.opening.clone() }), format!("note {} isn't the user's", h.index)));
            }
        }
        let total: u64 = held.iter().map(|h| h.value).sum();
        if total / terms.unit < u64::from(b) {
            return Ok(Decision::Assert(at_star(Why::TooMuch { openings: held.iter().map(|h| h.opening.clone()).collect() }), format!("its notes are worth {total} sats, less than {b} units")));
        }
        if let Some((why, said)) = self.second(ctx, cid, &held) {
            return Ok(Decision::Assert(at_star(why), said));
        }
        // once found to hold, only a second claim can still appear
        if recorded {
            return Ok(Decision::Holds);
        }
        // its locks at A*: each claimed output of a spend void then, or
        // pending without an acknowledgment that holds
        let c_h = a.from;
        let s_ch = state.sealed.iter().find(|s| s.height == c_h).context("A*'s chain receipt's height isn't sealed")?;
        let chain_set = ChainSet { tree: s_ch.tree, set: s_ch.nullifiers };
        let open = |k: &Hash, h: u64| self.seq.opening(k, h);
        for (w, h) in held.iter().enumerate() {
            let index = h.index;
            let lock = open(&ack::lock_key(index), h2)?;
            let t = lock.leaf;
            if t == [0; 32] {
                continue;
            }
            let void = open(&ack::void_key(&t), h2)?;
            if void.leaf != [0; 32] {
                return Ok(Decision::Assert(at_star(Why::Voided { which: w as u8, lock, void }), format!("note {index} is an output of a spend void at A* (height {h2})")));
            }
            let acked = open(&ack::acked_key(&t), h2)?;
            if acked.leaf != [0; 32] {
                continue;
            }
            let bad = match claim.acks.iter().find(|a| a.txid == t) {
                None => Some(None),
                Some(a) if !a.signed() => Some(Some(Bad::Signature)),
                Some(a) => {
                    let p = open(&ack::tx_key(&t), h2)?;
                    if p.leaf != ack::pending(&a.ak, a.h) {
                        Some(Some(Bad::Pending(p)))
                    } else {
                        let an = open(&ack::anchor_key(a.a), c_h)?;
                        (an.leaf != a.hist_a).then_some(Some(Bad::Anchor(an, chain_set)))
                    }
                }
            };
            if let Some(bad) = bad {
                return Ok(Decision::Assert(at_star(Why::Locked { which: w as u8, lock, acked, bad }), format!("note {index} is locked at A* (height {h2}): its spend unacknowledged")));
            }
        }
        // its spends at A*: each that counts against her
        let known: Vec<Ack> = self.other_claims(ctx, cid).into_iter().flat_map(|(_, _, c)| c.acks).collect();
        for (w, h) in held.iter().enumerate() {
            let leaf = open(&h.nf, h2)?;
            let t = leaf.leaf;
            if t == [0; 32] {
                continue;
            }
            let counts = if let Some(c) = Self::exited(ctx, &t) {
                if c == cid {
                    continue;
                }
                Counts::Exit { c }
            } else {
                let acked = open(&ack::acked_key(&t), h2)?;
                if acked.leaf != [0; 32] {
                    Counts::Acked { acked }
                } else {
                    // pending at A*: it counts if an acknowledgment of it
                    // exists (sequenced since, or in another claim) whose
                    // anchor is applied by A*'s own record
                    let found = match self.seq.ack(&t)? {
                        Some(a) => Some(a.v()?),
                        None => known.iter().find(|a| a.txid == t).copied(),
                    };
                    let Some(a) = found else { continue };
                    let (pending, void, anchor) = (open(&ack::tx_key(&t), h2)?, open(&ack::void_key(&t), h2)?, open(&ack::anchor_key(a.a), c_h)?);
                    if !a.signed() || pending.leaf != ack::pending(&a.ak, a.h) || void.leaf != [0; 32] || anchor.leaf != a.hist_a {
                        continue;
                    }
                    Counts::Shown { ack: a, pending, void, anchor, chain: chain_set }
                }
            };
            let why = Why::Spent { which: w as u8, opening: h.opening.clone(), leaf, counts };
            return Ok(Decision::Assert(at_star(why), format!("note {} was spent by A* (height {h2})", h.index)));
        }
        Ok(Decision::Holds)
    }

    /// Sweep its bit outputs of a payout the user won (`request_timeout`,
    /// `failure`): for each bit clear in her signed `b`, by its leaf
    /// `hub_bit_i` (her signature on `b`, as `request` posted it, and the
    /// hub's).
    fn sweep_bits(ctx: &dyn Ctx, d: &Disputed) -> Result<Bits> {
        let req = vlichen_proto::link(ctx.chain(), d, "request")?;
        let (b, wire) = vlichen_proto::request_b(&req)?;
        let k = vlichen_proto::keys(&d.record)?;
        let core = d.graph.core();
        let (t, cctx) = (core.terms, core.ctx());
        let mut trees = vec![];
        for i in (t.low_bits..t.bits).filter(|i| (b >> i) & 1 == 0) {
            let tree = bit_tree(&cctx, i, &k.alice.b_key)?;
            trees.push((tree.script_pubkey(), tree, format!("hub_bit_{i}")));
        }
        ctx.sweep(&d.record, trees, &wire, Sequence::ENABLE_RBF_NO_LOCKTIME)
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

fn setup_terms(v: &serde_json::Value) -> Result<SetupTerms> {
    serde_json::from_value(v.clone()).context("the setup's terms")
}

impl HubL2 for Rollup {
    type Front = Front;

    fn info(&self) -> serde_json::Value {
        serde_json::json!({ "sequencer": self.seq.url, "deployment": self.seq.deployment, "guests": vlichen_proto::Guests::ours() })
    }
    fn version(&self) -> String {
        vlichen_proto::VERSION.to_string()
    }
    fn route(&self, _ctx: &dyn Ctx, _method: &Method, _path: &str, _body: &str) -> Option<Result<String>> {
        // the L2's own routes are the sequencer's
        None
    }

    fn check_owner(&self, _ctx: &dyn Ctx, _owner: &Hash) -> Result<()> {
        Ok(())
    }
    fn setup_terms(&self, ctx: &dyn Ctx, req: &HpSetupRequest) -> Result<serde_json::Value> {
        // the statement fixed now: its checkpoint the chain's tip
        let margin = vlichen_proto::margin(ctx.cfg().network);
        let checkpoint = self.seq.deployment.seal.is_some().then(|| Checkpoint::tip(ctx.chain())).transpose()?;
        let d = vlichen_proto::contract_word(&self.seq.deployment, checkpoint.as_ref(), bitcoin::Amount::from_sat(req.unit))?;
        Ok(serde_json::to_value(SetupTerms { margin, checkpoint, d })?)
    }
    fn same_setup(&self, _setup: &HpSetup, _req: &HpSetupRequest) -> bool {
        true
    }
    fn check_keys(&self, keys: &serde_json::Value) -> Result<()> {
        let k: AliceKeys = serde_json::from_value(keys.clone()).context("the user's one-time keys")?;
        k.check()
    }
    fn setup_journal(&self, cid: u32, setup: &HpSetup, _keys: &serde_json::Value) -> Result<Vec<u8>> {
        let t = setup_terms(&setup.l2)?;
        Ok(vlichen_proto::journal_of(cid, &t.d, &setup.deposit, t.margin).to_vec())
    }
    fn open_keys(&self, ctx: &dyn Ctx, setup: &HpSetup, keys: &serde_json::Value, _req: &OpenRequest, t_close: u32) -> Result<(serde_json::Value, serde_json::Value)> {
        let terms = setup_terms(&setup.l2)?;
        let alice: AliceKeys = serde_json::from_value(keys.clone()).context("the user's one-time keys")?;
        let windows = vlichen_proto::windows(ctx.cfg().network, t_close);
        Ok((serde_json::to_value(Keys { alice, terms, windows })?, serde_json::to_value(windows)?))
    }
    fn offered(&self, ctx: &dyn Ctx, record: &Record, mint_point: &PublicKey, fault: OpeningFault) -> Result<serde_json::Value> {
        // D, minted now to a key she can spend once this funding is out
        let d = record.minted() - u64::from(fault == OpeningFault::ShortMint);
        let owner = vlichen_proto::mint_owner(&record.l2_address, mint_point)?;
        let index = self.seq.submit(&Tx::Mint(Mint::new(&self.l2_key, &self.seq.deployment, vec![note(owner, d, 0)])))?[0];
        ctx.note(format!("contract {}: minted {d} sats to the user, locked to the funding", record.cid()));
        Ok(serde_json::json!({ "mint_index": index }))
    }
    fn funded(&self, _ctx: &dyn Ctx, _record: &Record) -> Result<()> {
        Ok(())
    }
    fn take_claim(&self, ctx: &dyn Ctx, record: &Record, b: u32, claim: &str, _rest_to: Option<Hash>) -> Result<()> {
        self.record_claim(ctx, record, b, &hex::decode(claim).context("the claim's hex")?)
    }

    fn front_start(&self, _ctx: &dyn Ctx, _d: &Disputed, at: u32) -> Result<FrontStep<Front>> {
        Ok(FrontStep::Wait(Front { at, recorded: false }))
    }
    fn front_tick(&self, ctx: &dyn Ctx, d: &Disputed, s: Front) -> Result<FrontStep<Front>> {
        let cid = d.record.cid();
        let r_op = OutPoint { txid: d.txid("request"), vout: 0 };
        if !ctx.chain().unspent(&r_op)? && ctx.chain().get_tx(&d.txid("assert")).is_none() {
            ctx.note(format!("contract {cid}: the user's request timed out; she is paid her claim"));
            return Ok(FrontStep::Paid { link: r_op.txid, terminal: "request_timeout".into() });
        }
        let w = vlichen_proto::keys(&d.record)?.windows;
        // the assertion must confirm before her timeout
        let last_chance = s.at + u32::from(w.w_h) - (u32::from(w.w_h) / 4).max(2);
        if ctx.chain().height()? >= last_chance {
            ctx.note_once(format!("contract {cid}: too late to assert; request_timeout will pay the user"));
            return Ok(FrontStep::Wait(s));
        }
        if !s.recorded {
            self.hold(ctx, d)?;
        }
        match self.decide(ctx, d, s.at, s.recorded)? {
            Decision::Wait(why) => {
                ctx.note_once(format!("contract {cid}: {why}"));
                Ok(FrontStep::Wait(s))
            }
            // a lying hub (tests) asserts against a claim that holds
            Decision::Holds if ctx.forging() => Ok(FrontStep::Prove { at: s.at }),
            Decision::Holds => {
                if !s.recorded {
                    self.record(ctx, d)?;
                }
                Ok(FrontStep::Wait(Front { at: s.at, recorded: true }))
            }
            Decision::Assert(input, why) => {
                ctx.note(format!("contract {cid}: the user's exit claim is false: {why}"));
                self.decided.lock().unwrap().insert(cid, (why, *input));
                Ok(FrontStep::Prove { at: s.at })
            }
        }
    }
    fn prover_input(&self, ctx: &dyn Ctx, d: &Disputed) -> Result<ProverInput> {
        let cid = d.record.cid();
        let Some((why, input)) = self.decided.lock().unwrap().get(&cid).cloned() else {
            // after a restart: decided again from the front
            return Ok(ProverInput::NotYet("deciding the claim again".into()));
        };
        let journal = vlichen_proto::journal(&d.record)?;
        match vlichen_withdraw::hp::check(&input, &[0; 32], &mut |_| true) {
            Ok(j) if j[..] == journal[..] => {}
            Ok(_) => anyhow::bail!("the statement's journal isn't the contract's"),
            Err(r) => return Ok(ProverInput::Fails(format!("the ground ({why}) doesn't check natively: {r:?}"))),
        }
        // the chain receipt the guest takes, beside the input (the prover
        // reads it there): RISC Zero's statement only, the mock needs none
        let real = matches!(ctx.cfg().statement.hp_kind()?, lichen_babe::statement::Kind::Risc0(_));
        if let (true, Ground::At { last, .. }) = (real, &input.ground) {
            let h = last.state.chain_height;
            let Some(r) = self.seq.chain_receipt(h)? else { return Ok(ProverInput::NotYet(format!("the claim is false ({why}); waiting for the chain receipt at height {h} to prove it"))) };
            let dir = ctx.hp_dir(cid);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("chain.receipt"), r)?;
        }
        Ok(ProverInput::Holds(input.to_bytes()))
    }
    fn collect(&self, ctx: &dyn Ctx, d: &Disputed, _link: Txid, terminal: &str) -> Result<Bits> {
        match terminal {
            // success pays the hub in full
            "success" => Ok(Bits::None),
            _ => Self::sweep_bits(ctx, d),
        }
    }
    fn describe(&self, s: &Front) -> String {
        if s.recorded {
            "requested: the claim holds, request_timeout pays the user".into()
        } else {
            "requested: the hub decides the claim".into()
        }
    }
}

/// [`HubProves`], re-exported for the tests.
pub type FrontGraph = HubProves;
