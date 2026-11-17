//! The user's side of vlichen's dispute front: she requests, posting her
//! claim; the hub asserts a proof that it is false (lichen plays the
//! rest), or after `w_H` her timeout pays her.
//!
//! Also her one-time keys for `b` and `E`, from her contract's seed, so
//! that nothing of them need be kept.

use anyhow::Result;
use lichen_chain::Chain;
use lichen_proto::hp::Disputed;
use lichen_wallet::engine::{ContractSecrets, Engine};
use lichen_wallet::FrontEnd;
use lichen_wots::{WotsParams, WotsSecret};
use vlichen_proto::{AliceKeys, HubProves};

use crate::exit::ExitPlan;

/// Her one-time key for `b`.
pub fn hp_b(s: &ContractSecrets) -> WotsSecret {
    WotsSecret::from_entropy(WotsParams::for_bytes(4), s.wots("hp/b"))
}

/// Her one-time key for `E`.
pub fn hp_e(s: &ContractSecrets) -> WotsSecret {
    WotsSecret::from_entropy(WotsParams::for_bytes(32), s.wots("hp/e"))
}

/// Her keys in the setup.
pub fn alice_keys(s: &ContractSecrets) -> AliceKeys {
    AliceKeys { b_key: hp_b(s).public(), e_key: hp_e(s).public() }
}

/// `request`'s witness: her signatures on `b` and `E`, the claim posted.
pub fn request_wire(s: &ContractSecrets, b: u32, e: &[u8; 32], posted: &[u8]) -> Result<Vec<Vec<u8>>> {
    HubProves::request_wire(&hp_b(s).sign(&b.to_be_bytes())?, &hp_e(s).sign(e)?, posted)
}

/// Her signature on `b`, as the payout's leaves take it.
pub fn b_wire(s: &ContractSecrets, b: u32) -> Result<Vec<Vec<u8>>> {
    Ok(HubProves::b_wire(&hp_b(s).sign(&b.to_be_bytes())?))
}

/// The front: `request`, then the hub's assertion is due.
pub fn run(e: &mut Engine<'_>, chain: &Chain, d: &Disputed, plan: &ExitPlan) -> Result<FrontEnd> {
    let record = &d.record;
    let b = plan.b;
    let wire = request_wire(e.signer.contract(), b, &plan.e()?, &plan.posted()?)?;
    let tx = d.finish("request", wire)?;
    e.say(format!("requesting {b} units ({} sats), {} ({} vB)", u64::from(b) * record.terms.unit.to_sat(), d.txid("request"), tx.vsize()));
    let at = e.link(chain, "request", tx)?;
    if e.opts.pause_after == Some(1) {
        return Ok(FrontEnd::Paused);
    }
    Ok(FrontEnd::Asserting { at, b_corr: None })
}
