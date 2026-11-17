//! Acknowledging our spends (agent notes: docs/HUB_PROVES_PLAN.md, "Data
//! availability: sender acknowledgment"): a spend of ours counts against
//! us, and its outputs unlock, only once we acknowledge it, which we do
//! only once an anchor checked on Bitcoin holds it and we hold its data
//! (the sequencer's state, checked against its signed roots). The
//! acknowledgment is kept with the spend's record before it goes, so a
//! claim of its locked outputs can carry it.

use anyhow::{Context, Result};
use lichen_chain::Chain;
use vlichen_ledger::vlichen_validity::ack;
use vlichen_ledger::{xonly, Ack, Tx};

use crate::{Rollup, Wallet};
use crate::prelude::*;

/// How long a wallet waits for its spends to be acknowledged before it
/// gives up (s): an acknowledgment waits for an anchor at or above the
/// spend, recorded once it is the margin deep (20 blocks on Mutinynet,
/// some minutes; 6 on mainnet, an hour or more).
pub const ACK_PATIENCE: u64 = 6 * 3600;

/// The rollup account: master's `Wallet` methods, on a lichen wallet.
pub trait Acks {
    fn unacknowledged(&self) -> usize;
    fn acknowledge(&mut self, chain: &Chain) -> Result<usize>;
    fn acknowledge_all(&mut self, chain: &Chain, secs: u64) -> Result<()>;
}

impl Acks for Wallet<Rollup> {
    /// How many spends of ours may still need acknowledging: until
    /// [`Wallet::acknowledge`] finds each acknowledged (or refused, or
    /// void), the wallet's user should stay online.
    fn unacknowledged(&self) -> usize {
        self.l2.signed.iter().filter(|s| !s.done).count()
    }

    /// Acknowledge each spend of ours the sequencer has sealed and an
    /// anchor checked on `chain` holds; how many are still waiting (not
    /// sealed, not anchored yet, or refused for now).
    fn acknowledge(&mut self, chain: &Chain) -> Result<usize> {
        let state = self.state()?;
        if self.l2.seq().deployment.seal.is_none() {
            // an unanchored deployment locks nothing
            for s in &mut self.l2.signed {
                s.done = true;
            }
            self.save()?;
            return Ok(0);
        }
        let anchors = self.checked_anchors(&state, chain)?;
        let hists = crate::finality::hists(&state);
        let mut waiting = 0;
        for k in 0..self.l2.signed.len() {
            let s = &self.l2.signed[k];
            if s.done {
                continue;
            }
            let Some(nf0) = s.nullifiers.first().copied() else { continue };
            let txid = state.leaf(&nf0);
            if txid == [0; 32] {
                // never sequenced: the sequencer applies a transaction as
                // it takes it, so one not in the state was refused
                self.l2.signed[k].done = true;
                continue;
            }
            let pending = state.leaf(&ack::tx_key(&txid));
            if pending == [0; 32] || state.spent(&ack::acked_key(&txid)) || state.spent(&ack::void_key(&txid)) {
                // not a pending spend (an exit's), acknowledged, or void
                self.l2.signed[k].done = true;
                continue;
            }
            let key = self.ak_key(&nf0);
            let ak = xonly(&key);
            let Some(h) = state.sealed.iter().map(|r| r.height).find(|h| ack::pending(&ak, *h) == pending) else {
                waiting += 1;
                continue;
            };
            let a = match &s.ack {
                Some(a) if anchors.iter().any(|c| c.height == a.a) => a.clone(),
                _ => match anchors.iter().find(|c| c.height >= h) {
                    Some(c) => Ack::new(&key, txid, h, c.height, *hists.get(c.height as usize).context("an anchor's height not sealed")?),
                    None => {
                        waiting += 1;
                        continue;
                    }
                },
            };
            // kept before it goes: a claim of the outputs carries it
            self.l2.signed[k].ack = Some(a.clone());
            self.save()?;
            if self.l2.acks_unsent {
                self.l2.signed[k].done = true;
                continue;
            }
            match self.l2.seq().submit(&Tx::Ack(a)) {
                Ok(_) => self.l2.signed[k].done = true,
                Err(e) => {
                    self.say(format!("acknowledging a spend: {e:#}"));
                    waiting += 1;
                }
            }
        }
        self.save()?;
        Ok(waiting)
    }

    /// [`Wallet::acknowledge`] until nothing waits, sealing as we go (a
    /// test's sequencer, or one that seals on demand), for at most `secs`.
    fn acknowledge_all(&mut self, chain: &Chain, secs: u64) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            if self.acknowledge(chain)? == 0 {
                return Ok(());
            }
            anyhow::ensure!(start.elapsed() < std::time::Duration::from_secs(secs), "spends still unacknowledged after {secs} s: not sealed or not anchored");
            let _ = self.l2.seq().seal();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}
