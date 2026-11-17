//! The rollup's account in the wallet: the sequencer's state, its roots
//! checked and recorded; our notes, transfers, the mint claimed at the
//! opening, and a withdrawal's claim.

use anyhow::{bail, ensure, Context, Result};
use bitcoin::secp256k1::SecretKey;
use lichen_chain::Chain;
use vlichen_l2::{Equivocation, Seen, State};
use vlichen_ledger::k256::schnorr::SigningKey;
use vlichen_ledger::{mint_secret, note, sign, xonly, Hash, Note, Spend, Transfer, Tx};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::prelude::*;
use crate::{ack, exit};
use crate::{Rollup, Wallet};

/// wallet acknowledges it ([`Wallet::acknowledge`]): its acknowledgment
/// once signed, and whether it is settled (acknowledged on the L2, void, or
/// needing none).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Signed {
    pub nullifiers: Vec<Hash>,
    #[serde(default)]
    pub ack: Option<vlichen_ledger::Ack>,
    #[serde(default)]
    pub done: bool,
    /// Its transparent outputs, by their nullifiers: a claim of one made
    /// without the sequencer (from the exit set kept) finds its
    /// acknowledgment by them.
    #[serde(default)]
    pub outputs: Vec<Hash>,
}

impl Signed {
    pub fn new(nullifiers: Vec<Hash>, outputs: &[Note]) -> Signed {
        Signed { nullifiers, ack: None, done: false, outputs: outputs.iter().map(vlichen_ledger::nullifier).collect() }
    }
}

/// An anchor checked on Bitcoin, and the height of its block.
#[derive(Clone, Copy, Debug)]
pub struct AnchorChecked {
    pub anchor: vlichen_ledger::AnchorState,
    /// The history hash it committed.
    pub hist: Hash,
    pub block_height: u32,
}

impl AnchorChecked {
    const LEN: usize = 32 + 4 + 32 + 32 + 4 + 8 + 32 + 32 + 32 + 4;
    /// Kept in the wallet's file as hex (the guest's `AnchorState` has no
    /// serde): a CLI run, or an app's launch, needn't check every anchor
    /// on Bitcoin again.
    fn to_hex(&self) -> String {
        let a = &self.anchor;
        let mut v = Vec::with_capacity(Self::LEN);
        v.extend_from_slice(&a.genesis.txid);
        v.extend_from_slice(&a.genesis.vout.to_le_bytes());
        v.extend_from_slice(&a.sequencer);
        v.extend_from_slice(&a.seal.txid);
        v.extend_from_slice(&a.seal.vout.to_le_bytes());
        v.extend_from_slice(&a.height.to_le_bytes());
        v.extend_from_slice(&a.root);
        v.extend_from_slice(&a.block);
        v.extend_from_slice(&self.hist);
        v.extend_from_slice(&self.block_height.to_le_bytes());
        hex::encode(v)
    }
    fn from_hex(s: &str) -> Option<AnchorChecked> {
        let v = hex::decode(s).ok().filter(|v| v.len() == Self::LEN)?;
        let mut at = 0;
        let mut take = |n: usize| {
            at += n;
            &v[at - n..at]
        };
        let h = |b: &[u8]| -> Hash { b.try_into().unwrap() };
        let genesis = vlichen_ledger::vlichen_validity::btc::OutPoint { txid: h(take(32)), vout: u32::from_le_bytes(take(4).try_into().unwrap()) };
        let sequencer = h(take(32));
        let seal = vlichen_ledger::vlichen_validity::btc::OutPoint { txid: h(take(32)), vout: u32::from_le_bytes(take(4).try_into().unwrap()) };
        let height = u64::from_le_bytes(take(8).try_into().unwrap());
        let (root, block, hist) = (h(take(32)), h(take(32)), h(take(32)));
        let block_height = u32::from_le_bytes(take(4).try_into().unwrap());
        Some(AnchorChecked { anchor: vlichen_ledger::AnchorState { genesis, sequencer, seal, height, root, block }, hist, block_height })
    }
}

/// A deposit for a contract not yet open: kept from before it is paid,
/// so that it can always be refunded.

/// An unspent note of ours.
#[derive(Clone, Copy, Debug)]
pub struct Owned {
    pub note: Note,
    pub index: u32,
}

/// The rollup account: master's `Wallet` methods, on a lichen wallet.
pub trait Ledger {
    /// Our address on the L2 (the wallet's own key's).
    fn me(&self) -> Hash;
    /// The wallet's key on the L2.
    fn l2_key(&self) -> SigningKey;
    /// Claim the mint at `index`, locked to the key of `l2_address` and the
    /// funding's adaptor secret `t`: spent to us.
    fn claim_mint_at(&mut self, index: u32, l2_address: &Hash, t: &SecretKey) -> Result<()>;
    fn state(&self) -> Result<State>;
    fn observe(&self, state: State) -> Result<State>;
    fn seen(&self) -> Seen;
    fn compare_roots(&self, roots: &[vlichen_ledger::SignedRoot]) -> Result<usize>;
    fn evidence(&self, e: &Equivocation) -> Result<PathBuf>;
    fn notes(&self, state: &State) -> Vec<Owned>;
    fn notes_locked_too(&self, state: &State) -> Vec<Owned>;
    fn ak_key(&self, nf0: &Hash) -> SigningKey;
    fn balance(&self) -> Result<u64>;
    fn send(&mut self, to: Hash, amount: u64, memo: u32) -> Result<Vec<u32>>;
    fn wait_mint(&self, index: u32, owner: &Hash, value: u64) -> Result<()>;
    fn withdrawal_claim(&mut self, chain: &Chain, cid: u32, units: u32) -> Result<exit::ExitPlan>;
}

impl Ledger for Wallet<Rollup> {
    fn me(&self) -> Hash {
        self.owners()[0]
    }
    fn l2_key(&self) -> SigningKey {
        let kp = self.kp_of(&self.me()).expect("the wallet's own key");
        SigningKey::from_bytes(&kp.secret_bytes()).expect("a scalar")
    }
    fn claim_mint_at(&mut self, index: u32, l2_address: &Hash, t: &SecretKey) -> Result<()> {
        let owner = SigningKey::from_bytes(&self.kp_of(l2_address)?.secret_bytes()).context("the contract's L2 key")?;
        let key = mint_secret(&owner, &t.secret_bytes())?;
        let state = self.state()?;
        let n = *state.note(index).context("the deposit minted to us is gone")?;
        ensure!(n.owner == xonly(&key), "the deposit minted is not ours to claim");
        if !state.is_spent(&n) {
            let spends = vec![Spend { note: n, index }];
            let outputs = vec![note(self.me(), n.value, 0)];
            let nf = vlichen_ledger::nullifier(&n);
            let ak = xonly(&self.ak_key(&nf));
            let digest = Transfer::digest(&spends, &outputs, &ak);
            self.record_signed(Signed::new(vec![nf], &outputs))?;
            self.l2.seq().submit(&Tx::Transfer(Transfer { sigs: vec![sign(&key, &digest)], spends, outputs, ak }))?;
        }
        Ok(())
    }
    /// The sequencer's state, its roots checked and recorded: an error if
    /// it shows a root at a height seen before other than the one seen
    /// (the evidence written beside the wallet, `<wallet>.equivocation.json`).
    /// Its exit set kept up to it as well, at most every
    /// [`exit::EXIT_SET_EVERY`] ([`Wallet::keep_exit_set`]).
    fn state(&self) -> Result<State> {
        let s = self.observe(self.l2.seq().state()?)?;
        self.keep_exit_set_soon(&s);
        Ok(s)
    }
    /// A state of the sequencer fetched apart from the wallet (with a
    /// client pinned to its deployment, which checks its roots), recorded
    /// as [`Wallet::state`] records it: a caller holding the wallet in a
    /// lock fetches it first.
    fn observe(&self, state: State) -> Result<State> {
        let new = self.l2.seen.lock().unwrap().record(&state.sealed);
        match new {
            Ok(true) => self.save()?,
            Ok(false) => {}
            Err(e) => bail!("{e}: it signed two histories. The evidence: {}", self.evidence(&e)?.display()),
        }
        Ok(state)
    }
    /// The signed roots seen, by height.
    fn seen(&self) -> Seen {
        self.l2.seen.lock().unwrap().clone()
    }
    /// Check `roots` from elsewhere (another wallet's, another server's)
    /// against those seen: how many heights agree, or the first conflict,
    /// its evidence written as by [`Wallet::state`]. Each root's signature
    /// is checked first.
    fn compare_roots(&self, roots: &[vlichen_ledger::SignedRoot]) -> Result<usize> {
        for r in roots {
            r.verify(&self.l2.seq().deployment.sequencer).with_context(|| format!("height {}: not the sequencer's signature", r.height))?;
        }
        let mut theirs = Seen::default();
        theirs.record(roots).map_err(|e| anyhow::anyhow!("{e}: the roots given conflict among themselves"))?;
        let seen = self.seen();
        if let Err(e) = seen.clone().record(roots) {
            bail!("{e}: it signed two histories. The evidence: {}", self.evidence(&e)?.display());
        }
        Ok(theirs.roots.keys().filter(|h| seen.roots.contains_key(h)).count())
    }
    fn evidence(&self, e: &Equivocation) -> Result<PathBuf> {
        let path = self.path().with_extension("equivocation.json");
        std::fs::write(&path, serde_json::to_vec_pretty(e)?)?;
        Ok(path)
    }
    /// Our unspent notes, unlocked (a locked one waits for its sender's
    /// acknowledgment: [`State::locked`]).
    fn notes(&self, state: &State) -> Vec<Owned> {
        self.notes_locked_too(state).into_iter().filter(|o| !state.locked(o.index)).collect()
    }
    /// Our unspent notes, locked or not.
    fn notes_locked_too(&self, state: &State) -> Vec<Owned> {
        let me = self.me();
        state.transparent().filter(|(_, n)| n.owner == me && !state.is_spent(n)).map(|(i, n)| Owned { note: *n, index: i }).collect()
    }
    /// The one-time acknowledgment key of our spend whose first nullifier
    /// is `nf0` (`vlichen_validity::ack`).
    fn ak_key(&self, nf0: &Hash) -> SigningKey {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(b"lichen/ak");
        h.update(self.l2_key().to_bytes());
        h.update(nf0);
        SigningKey::from_bytes(&h.finalize()).expect("a scalar, all but certainly")
    }
    /// What we hold on the L2: transparent and shielded notes.
    fn balance(&self) -> Result<u64> {
        let (t, s) = self.balances()?;
        Ok(t + s)
    }
    /// Pay `amount` to `to` with `memo`; the new notes' indices (the
    /// payment's first, then our change if any).
    fn send(&mut self, to: Hash, amount: u64, memo: u32) -> Result<Vec<u32>> {
        anyhow::ensure!(amount > 0, "nothing to send");
        let state = self.state()?;
        let mut mine = self.notes(&state);
        mine.sort_by_key(|o| std::cmp::Reverse(o.note.value));
        let (mut spends, mut total) = (vec![], 0u64);
        for o in mine {
            if total >= amount {
                break;
            }
            total += o.note.value;
            spends.push(Spend { note: o.note, index: o.index });
        }
        if total < amount {
            bail!("the balance is {total}, short of {amount}");
        }
        let mut outputs = vec![note(to, amount, memo)];
        if total > amount {
            outputs.push(note(self.me(), total - amount, 0));
        }
        let nfs: Vec<Hash> = spends.iter().map(|s| vlichen_ledger::nullifier(&s.note)).collect();
        let ak = xonly(&self.ak_key(&nfs[0]));
        let digest = Transfer::digest(&spends, &outputs, &ak);
        let sigs = spends.iter().map(|_| sign(&self.l2_key(), &digest)).collect();
        // kept before it goes: an exit without the sequencer must know,
        // and the wallet acknowledges it
        self.record_signed(Signed::new(nfs, &outputs))?;
        self.l2.seq().submit(&Tx::Transfer(Transfer { spends, outputs, sigs, ak }))
    }

    /// Wait until note `index` is `value` to `owner` (memo 0), unspent and
    /// sealed: a contract's mint, before we sign its funding.
    fn wait_mint(&self, index: u32, owner: &Hash, value: u64) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            let state = self.state()?;
            if let Some(n) = state.note(index) {
                ensure!((n.owner, n.value, n.memo) == (*owner, value, 0), "note {index} is not the deposit minted to us, locked to the funding");
                ensure!(!state.is_spent(n), "the deposit minted to us is spent");
                if state.sealed.iter().any(|s| s.size > index) {
                    return Ok(());
                }
                let _ = self.l2.seq().seal();
            }
            ensure!(start.elapsed() < std::time::Duration::from_secs(120), "the deposit minted to us was never sealed");
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    /// A cooperative withdrawal's claim of `units`: her notes worth exactly `units` of the contract's unit, by
    /// position, signed for the contract. Her notes if some are worth it
    /// exactly (all of them, or one); else a note of that value split off
    /// by a payment to herself, waited for until acknowledged (its output
    /// unlocked: anchored, the margin deep).
    fn withdrawal_claim(&mut self, chain: &Chain, cid: u32, units: u32) -> Result<exit::ExitPlan> {
        let record = self.contracts.iter().find(|h| h.record.cid() == cid).with_context(|| format!("no contract {cid}"))?.record.clone();
        let want = u64::from(units) * record.terms.unit.to_sat();
        // our own spends acknowledged first: their change unlocked
        self.acknowledge_all(chain, ack::ACK_PATIENCE)?;
        let mine = self.notes(&self.state()?);
        let total: u64 = mine.iter().map(|o| o.note.value).sum();
        ensure!(total >= want, "the transparent balance is {total} sats, short of {want}");
        let positions: Vec<u32> = if total == want && mine.len() <= vlichen_withdraw::hp::MAX_NOTES {
            mine.iter().map(|o| o.index).collect()
        } else if let Some(o) = mine.iter().find(|o| o.note.value == want) {
            vec![o.index]
        } else {
            self.say(format!("splitting off a note of {want} sats to claim"));
            let i = self.send(self.me(), want, 0)?[0];
            self.say("waiting for the split to be anchored and acknowledged: until then its note is locked".into());
            self.acknowledge_all(chain, ack::ACK_PATIENCE)?;
            ensure!(!self.state()?.locked(i), "the note split off is still locked");
            vec![i]
        };
        let acks = self.acks_for(&self.state()?, &positions)?;
        self.claim_of(&record, units, positions, acks)
    }
}

/// The anchors checked, kept as hex.
pub(crate) mod anchors_hex {
    use std::sync::{Arc, Mutex};

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::AnchorChecked;

    pub fn serialize<S: Serializer>(v: &Arc<Mutex<Vec<AnchorChecked>>>, s: S) -> Result<S::Ok, S::Error> {
        v.lock().unwrap().iter().map(AnchorChecked::to_hex).collect::<Vec<_>>().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Arc<Mutex<Vec<AnchorChecked>>>, D::Error> {
        let v: Vec<String> = Vec::deserialize(d)?;
        Ok(Arc::new(Mutex::new(v.iter().map_while(|a| AnchorChecked::from_hex(a)).collect())))
    }
}
