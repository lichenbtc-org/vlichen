//! vlichen's dispute front (the exit where only the hub proves): the user
//! claims, and the hub asserts a proof that her claim is false or lets her
//! timeout pay her. Then BABE's tail ([`lichen_babe::graph`]); `success`
//! pays the hub.
//!
//! ```text
//! request (the user: b, E, the claim)
//!  ├─ request_timeout (after w_H)      → the user: b
//!  └─ assert … (BABE's tail; success pays the hub)
//! ```
//!
//! The contract output's `request` leaf carries her signatures on `b` and
//! `E`, and the claim posted below them (the hub's statement reads it,
//! `vlichen_withdraw::hp`).

use anyhow::{ensure, Context, Result};
use lichen_babe::graph::{drop_n, link_leaf, Front, FrontLink, Payout};
use lichen_btc::bitcoin::{Amount, TxOut};
use lichen_contract::{wots_shape, ContractParams, Core, PartyPubKeys, Role, Terms};
use vlichen_withdraw::hp::{posted_items, POSTED_ITEM, POSTED_ITEMS};
use lichen_wots::{WotsExt, WotsParams, WotsPublic, WotsSig};

/// The front's windows: `w_H` for the hub's assertion; `t_a` her
/// request's latest, a height (as `T_close` is).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Windows {
    pub w_h: u16,
    pub t_a: u32,
}

/// `E`'s 64 digits (32 bytes).
const E_DIGITS: usize = 64;

/// What the front is built from.
#[derive(Clone, Debug)]
pub struct Spec {
    pub terms: Terms,
    pub params: ContractParams,
    /// The user's keys, then the hub's.
    pub pubs: [PartyPubKeys; 2],
    /// The user's one-time keys for `b` (the payout's bit outputs read
    /// it) and for `E`.
    pub b_key: WotsPublic,
    pub e_key: WotsPublic,
    pub windows: Windows,
}

/// The front: `request`.
pub struct HubProves {
    pub spec: Spec,
}

impl HubProves {
    pub fn new(spec: Spec) -> Result<HubProves> {
        ensure!(spec.b_key.params == WotsParams::for_bytes(4), "the key for b signs a 4-byte word");
        ensure!(spec.e_key.params == WotsParams::for_bytes(32), "the key for E signs 32 bytes");
        Ok(HubProves { spec })
    }

    /// `request`'s witness: the claim as posted (in [`POSTED_ITEMS`] items
    /// of up to [`POSTED_ITEM`] bytes, its first items), then her
    /// signatures on `E` and on `b`.
    pub fn request_wire(b: &WotsSig, e: &WotsSig, posted: &[u8]) -> Result<Vec<Vec<u8>>> {
        let items = posted_items(posted).with_context(|| format!("a claim of {} bytes posted", posted.len()))?;
        Ok(items.into_iter().map(<[u8]>::to_vec).chain(e.consumption_order().into_iter().rev()).chain(b.consumption_order().into_iter().rev()).collect())
    }
    /// Read `request`'s arguments (consumption order, below both
    /// signatures): her signatures on `b` and `E`, and the claim as posted.
    pub fn read_request(args: &[Vec<u8>]) -> Result<(WotsSig, WotsSig, Vec<u8>)> {
        let (nb, ne) = (2 * WotsParams::for_bytes(4).total_digits() as usize, 2 * WotsParams::for_bytes(32).total_digits() as usize);
        ensure!(args.len() == nb + ne + POSTED_ITEMS, "a request of {} arguments", args.len());
        let b = WotsSig::from_consumption_order(WotsParams::for_bytes(4), &args[..nb])?;
        let e = WotsSig::from_consumption_order(WotsParams::for_bytes(32), &args[nb..nb + ne])?;
        Ok((b, e, args[nb + ne..].iter().rev().flatten().copied().collect()))
    }
    /// Her signature on `b` as the payout's leaves take it (bottom first:
    /// each digit's hash, then the digit).
    pub fn b_wire(b: &WotsSig) -> Vec<Vec<u8>> {
        b.consumption_order().into_iter().rev().collect()
    }
}

impl Front for HubProves {
    fn core(&self) -> Core<'_> {
        Core { terms: &self.spec.terms, params: &self.spec.params, pubs: &self.spec.pubs }
    }
    fn links(&self) -> Result<Vec<FrontLink>> {
        let s = &self.spec;
        let ctx = self.core().ctx();
        let posted = vec![0u8; POSTED_ITEMS * POSTED_ITEM];
        Ok(vec![FrontLink {
            name: "request",
            poster: Role::User,
            // her signatures on b and E, the claim posted below them,
            // unchecked (the hub's statement reads it)
            leaf: link_leaf(&ctx, "request", true, |b| {
                let b = drop_n(b.wots_verify(&s.b_key), 8);
                drop_n(drop_n(b.wots_verify(&s.e_key), E_DIGITS), POSTED_ITEMS)
            }),
            // with the largest claim
            shape: posted_items(&posted).expect("fits").into_iter().map(<[u8]>::to_vec).chain(wots_shape([&s.e_key, &s.b_key])).collect(),
            terminal: "request_timeout",
            window: s.windows.w_h,
            pays: Payout::User,
        }])
    }
    fn pay(&self, which: Payout, available: Amount) -> Result<Vec<TxOut>> {
        match which {
            Payout::User => self.core().pay_alice(&self.spec.b_key, available),
            Payout::Hub => Ok(self.core().pay_hub(available)),
            Payout::Other(o) => anyhow::bail!("no payout {o}"),
        }
    }
    fn success(&self) -> Payout {
        Payout::Hub
    }
}
