//! The wallet's part of a hub-proves contract (agent notes:
//! docs/HUB_PROVES_PLAN.md, `lichen_proto::hp`): its opening, in which she
//! garbles for the hub's statement, commits to her instances and uploads
//! the ones the hub keeps; and her claims, definite notes by position,
//! signed once: an exit's, and a cooperative withdrawal's
//! ([`Wallet::claim_of`]).

use anyhow::{ensure, Context, Result};
use lichen_proto::Record;
use vlichen_proto::statement_outpoint;
use vlichen_withdraw::hp::{Claim, MAX_NOTES};

use crate::exit::ExitPlan;
use crate::{Rollup, Wallet};
use crate::prelude::*;

/// A note of ours a claim may take: transparent, or shielded.
#[derive(Clone, Copy, Debug)]
pub enum Cand {
    T(vlichen_ledger::Note),
    S(crate::shielded::SOwned),
}

impl Cand {
    pub fn value(&self) -> u64 {
        match self {
            Cand::T(n) => n.value,
            Cand::S(o) => o.value,
        }
    }
}

/// What her opening request carries, and what it rests on.
/// The rollup account: master's `Wallet` methods, on a lichen wallet.
pub trait Claims {
    fn hp_exit_plan(&self, chain: &lichen_chain::Chain, cid: u32) -> Result<ExitPlan>;
    fn hp_safe_size(&self, chain: &lichen_chain::Chain, margin: u32, state: Option<&vlichen_l2::State>) -> Result<Option<u32>>;
    fn hp_wait_anchored(&self, chain: &lichen_chain::Chain, cid: u32, positions: &[u32]) -> Result<()>;
    fn hp_plan_now(&self, chain: &lichen_chain::Chain, cid: u32, patient: bool) -> Result<Option<ExitPlan>>;
    fn cand_nf(&self, c: &Cand) -> [u8; 32];
    fn acks_for(&self, state: &vlichen_l2::State, positions: &[u32]) -> Result<Vec<vlichen_ledger::vlichen_validity::ack::Ack>>;
    fn spent_for_us(&self, state: Option<&vlichen_l2::State>, nf: &[u8; 32]) -> bool;
    fn claimable(&self, state: &vlichen_l2::State) -> Vec<(u32, Cand)>;
    fn acks_kept(&self, kept: &[(u32, Cand)], positions: &[u32]) -> Result<Vec<vlichen_ledger::vlichen_validity::ack::Ack>>;
    fn claim_of(&self, record: &Record, b: u32, positions: Vec<u32>, acks: Vec<vlichen_ledger::vlichen_validity::ack::Ack>) -> Result<ExitPlan>;
    fn claim_with(&self, record: &Record, b: u32, positions: Vec<u32>, shielded: &[crate::shielded::SOwned], acks: Vec<vlichen_ledger::vlichen_validity::ack::Ack>) -> Result<ExitPlan>;
}

impl Claims for Wallet<Rollup> {
    fn hp_exit_plan(&self, chain: &lichen_chain::Chain, cid: u32) -> Result<ExitPlan> {
        let start = chain.height()?;
        let mut stall = lichen_chain::Stall::new(start, chain.network);
        let margin = self.contracts.iter().find(|h| h.record.cid() == cid).map_or(0, |h| vlichen_proto::keys(&h.record).map_or(0, |k| k.terms.margin));
        // how long to wait for a note newer than the anchor the margin deep:
        // past it, anchoring seems stopped, and the claim leaves such notes
        // out rather than wait for ever
        let patience = 3 * margin.max(1);
        let mut said = false;
        loop {
            let patient = chain.height()? < start + patience;
            if let Some(p) = self.hp_plan_now(chain, cid, patient)? {
                return Ok(p);
            }
            if !said {
                self.say(format!("waiting for our notes' anchor to be {} blocks deep: the claim is judged at the last anchor that deep", self.contracts.iter().find(|h| h.record.cid() == cid).map_or(0, |h| vlichen_proto::keys(&h.record).map_or(0, |k| k.terms.margin))));
                said = true;
            }
            ensure!(!stall.stalled(chain.height()?), "the chain stalled before our notes' anchor was deep enough to claim");
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    /// The tree's size at the last anchor whose block is at least `margin`
    /// below the tip (`A*` of a request confirmed next), by the state's
    /// anchors, or with no state by the exit sets kept.
    fn hp_safe_size(&self, chain: &lichen_chain::Chain, margin: u32, state: Option<&vlichen_l2::State>) -> Result<Option<u32>> {
        use bitcoin::hashes::Hash as _;
        let tip = chain.height()?;
        let deep = |block: &[u8; 32]| -> Result<bool> { Ok(chain.block_height(&bitcoin::BlockHash::from_byte_array(*block))?.is_some_and(|h| h + margin <= tip)) };
        match state {
            Some(s) => {
                let mut size = None;
                for a in &s.anchors {
                    if deep(&a.anchor.block)? {
                        size = s.sealed.iter().find(|x| x.height == a.anchor.height).map(|x| x.size);
                    }
                }
                Ok(size)
            }
            None => {
                let mut size = None;
                for x in self.exit_sets() {
                    if deep(&x.anchored.anchor.block)? {
                        size = size.max(Some(x.sealed.size));
                    }
                }
                Ok(size)
            }
        }
    }

    /// Wait until the notes at `positions` are in the tree of the last
    /// anchor the margin deep (`A*` of a request confirmed next): a claim
    /// made before, a cooperative withdrawal's, before its exit.
    fn hp_wait_anchored(&self, chain: &lichen_chain::Chain, cid: u32, positions: &[u32]) -> Result<()> {
        let margin = self.contracts.iter().find(|h| h.record.cid() == cid).with_context(|| format!("no contract {cid}"))?.record.clone(); let margin = vlichen_proto::keys(&margin)?.terms.margin;
        let last = positions.iter().max().copied().unwrap_or(0);
        let mut stall = lichen_chain::Stall::new(chain.height()?, chain.network);
        let mut said = false;
        loop {
            let live = self.l2.seq().state().ok().map(|s| self.observe(s)).transpose()?;
            if self.hp_safe_size(chain, margin, live.as_ref())?.is_some_and(|s| last < s) {
                return Ok(());
            }
            if !said {
                self.say(format!("waiting for the claim's notes' anchor to be {margin} blocks deep: the claim is judged at the last anchor that deep"));
                said = true;
            }
            ensure!(!stall.stalled(chain.height()?), "the chain stalled before the claim's notes' anchor was deep enough to claim");
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    /// [`Wallet::hp_exit_plan`], or, while `patient`, none while a note of
    /// ours is newer than the anchor the margin deep; not `patient`, the
    /// plan without such notes.
    fn hp_plan_now(&self, chain: &lichen_chain::Chain, cid: u32, patient: bool) -> Result<Option<ExitPlan>> {
        let h = self.contracts.iter().find(|h| h.record.cid() == cid).with_context(|| format!("no contract {cid}"))?;
        let record = &h.record;
        let live = match self.l2.seq().state() {
            Ok(s) => Some(self.observe(s)?),
            Err(e) => {
                self.say(format!("the sequencer doesn't answer ({e:#}): claiming from the notes kept"));
                None
            }
        };
        let spent = |c: &Cand| self.spent_for_us(live.as_ref(), &self.cand_nf(c));
        let kept: Vec<(u32, Cand)> = match &live {
            Some(_) => vec![],
            None => self.exit_sets().into_iter().flat_map(|x| x.held()).collect(),
        };
        // her notes: in the state, else in the newest exit set kept
        let mut mine: Vec<(u32, Cand)> = match &live {
            Some(s) => self.claimable(s),
            None => kept.clone(),
        };
        mine.sort_by_key(|(i, _)| *i);
        mine.dedup_by_key(|(i, _)| *i);
        let unit = record.terms.unit.to_sat();
        let cap = record.terms.v_max().to_sat();
        let mut notes: Vec<(u32, Cand)> = mine.into_iter().filter(|(_, c)| !spent(c)).collect();
        let safe = self.hp_safe_size(chain, vlichen_proto::keys(record)?.terms.margin, live.as_ref())?;
        let newer = |i: &u32| safe.is_none_or(|s| *i >= s);
        if notes.iter().any(|(i, _)| newer(i)) {
            if patient {
                return Ok(None);
            }
            let left = notes.iter().filter(|(i, _)| newer(i)).count();
            self.say(format!("no anchor the margin deep holds {left} of our notes yet, and anchoring seems stopped: claiming without them"));
            notes.retain(|(i, _)| !newer(i));
        }
        notes.sort_by_key(|(_, c)| std::cmp::Reverse(c.value()));
        let mut chosen: Vec<(u32, Cand)> = vec![];
        let mut total = 0u64;
        for (i, c) in notes {
            if total >= cap || chosen.len() == MAX_NOTES {
                break;
            }
            total += c.value();
            chosen.push((i, c));
        }
        // the claim as large as request's witness takes: the smallest notes
        // left out until it fits
        loop {
            ensure!(!chosen.is_empty(), "no notes of ours to claim");
            let total: u64 = chosen.iter().map(|(_, c)| c.value()).sum();
            let b = u32::try_from(total.min(cap) / unit).context("b fits in a u32")?;
            ensure!(b > 0, "the notes claimable come to less than the contract's unit, {unit} sats");
            let positions: Vec<u32> = chosen.iter().map(|(i, _)| *i).collect();
            let acks = match &live {
                Some(s) => self.acks_for(s, &positions)?,
                None => self.acks_kept(&kept, &positions)?,
            };
            let t: Vec<u32> = chosen.iter().filter(|(_, c)| matches!(c, Cand::T(_))).map(|(i, _)| *i).collect();
            let sh: Vec<crate::shielded::SOwned> = chosen.iter().filter_map(|(_, c)| if let Cand::S(o) = c { Some(*o) } else { None }).collect();
            let plan = self.claim_with(record, b, t, &sh, acks)?;
            if vlichen_withdraw::hp::posted_items(&plan.posted()?).is_some() {
                return Ok(Some(plan));
            }
            let (i, _) = chosen.pop().expect("not empty");
            self.say(format!("the claim is too large for request's witness: leaving out the note at {i}"));
        }
    }

    /// A candidate's nullifier.
    fn cand_nf(&self, c: &Cand) -> [u8; 32] {
        match c {
            Cand::T(n) => vlichen_ledger::nullifier(n),
            Cand::S(o) => self.snullifier(o),
        }
    }

    /// Our acknowledgments of the spends whose outputs are at `positions`:
    /// a claim carries them, so that a note locked at `A*` is shown
    /// acknowledged (agent notes: docs/HUB_PROVES_PLAN.md, "Data
    /// availability").
    fn acks_for(&self, state: &vlichen_l2::State, positions: &[u32]) -> Result<Vec<vlichen_ledger::vlichen_validity::ack::Ack>> {
        use vlichen_ledger::vlichen_validity::ack;
        let mut out: Vec<ack::Ack> = vec![];
        for i in positions {
            let t = state.leaf(&ack::lock_key(*i));
            if t == [0; 32] || out.iter().any(|a| a.txid == t) {
                continue;
            }
            if let Some(a) = self.l2.signed.iter().filter_map(|s| s.ack.as_ref()).find(|a| a.txid == t) {
                out.push(a.v()?);
            }
        }
        Ok(out)
    }

    /// Whether the note of nullifier `nf` is spent for us: by a spend of
    /// ours we acknowledged, or, in `state`, otherwise than by a spend of
    /// ours never acknowledged (which doesn't count against us).
    fn spent_for_us(&self, state: Option<&vlichen_l2::State>, nf: &[u8; 32]) -> bool {
        use vlichen_ledger::vlichen_validity::ack;
        let unacked = self.l2.signed.iter().any(|s| s.nullifiers.contains(nf) && s.ack.is_none());
        self.l2.signed.iter().any(|s| s.nullifiers.contains(nf) && s.ack.is_some())
            || state.is_some_and(|s| {
                let t = s.leaf(nf);
                t != [0; 32] && (!unacked || s.spent(&ack::acked_key(&t)))
            })
    }

    /// Our notes a claim may take at `state`, transparent and shielded:
    /// ours, not spent for us, and unlocked or locked by a spend we hold
    /// the acknowledgment of (a claim carries it: [`Wallet::acks_for`]).
    fn claimable(&self, state: &vlichen_l2::State) -> Vec<(u32, Cand)> {
        use vlichen_ledger::vlichen_validity::ack;
        let me = self.me();
        let ours = state.transparent().filter(|(_, n)| n.owner == me).map(|(i, n)| (i, Cand::T(*n)));
        let shielded = self.shielded_scan(state).into_iter().map(|o| (o.index, Cand::S(o)));
        ours.chain(shielded)
            .filter(|(_, c)| !self.spent_for_us(Some(state), &self.cand_nf(c)))
            .filter(|(i, _)| {
                if !state.locked(*i) {
                    return true;
                }
                let t = state.leaf(&ack::lock_key(*i));
                self.l2.signed.iter().filter_map(|s| s.ack.as_ref()).any(|a| a.txid == t)
            })
            .collect()
    }

    /// Without the sequencer: our acknowledgments of the spends that output
    /// the notes kept at `positions` (each spend's outputs recorded with
    /// it, [`crate::Signed`]); a note locked at `A*` is shown acknowledged.
    fn acks_kept(&self, kept: &[(u32, Cand)], positions: &[u32]) -> Result<Vec<vlichen_ledger::vlichen_validity::ack::Ack>> {
        let mut out: Vec<vlichen_ledger::vlichen_validity::ack::Ack> = vec![];
        for (_, c) in kept.iter().filter(|(i, _)| positions.contains(i)) {
            let nf = self.cand_nf(c);
            if let Some(a) = self.l2.signed.iter().filter(|s| s.outputs.contains(&nf)).find_map(|s| s.ack.as_ref()) {
                if !out.iter().any(|x| x.txid == a.txid) {
                    out.push(a.v()?);
                }
            }
        }
        Ok(out)
    }

    /// A claim of `b` units from contract `record` of the notes at
    /// `positions`, signed: an exit's, or a cooperative withdrawal's.
    fn claim_of(&self, record: &Record, b: u32, positions: Vec<u32>, acks: Vec<vlichen_ledger::vlichen_validity::ack::Ack>) -> Result<ExitPlan> {
        self.claim_with(record, b, positions, &[], acks)
    }

    /// The same with our shielded notes `shielded` too, signed by our exit
    /// key.
    fn claim_with(&self, record: &Record, b: u32, mut positions: Vec<u32>, shielded: &[crate::shielded::SOwned], acks: Vec<vlichen_ledger::vlichen_validity::ack::Ack>) -> Result<ExitPlan> {
        positions.sort_unstable();
        let has_t = !positions.is_empty();
        let mut claim = Claim { positions, acks, sigs: vec![], shielded: (!shielded.is_empty()).then(|| self.sclaim(shielded)) };
        let e = claim.fingerprint(record.cid(), &statement_outpoint(&record.hp().deposit), b);
        let d = vlichen_withdraw::hp::signed_digest(&e);
        let sign = |k: &vlichen_ledger::k256::schnorr::SigningKey| -> Result<[u8; 64]> { Ok(k.sign_raw(&d, &[0; 32]).map_err(|e| anyhow::anyhow!("signing the claim: {e}"))?.to_bytes()) };
        if has_t {
            claim.sigs.push(sign(&self.l2_key())?);
        }
        if claim.shielded.is_some() {
            claim.sigs.push(sign(&self.exit_key())?);
        }
        ensure!(claim.well_formed(), "the claim isn't well formed");
        Ok(ExitPlan { b, e: hex::encode(e), posted: hex::encode(claim.to_bytes()) })
    }
}
