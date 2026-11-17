//! Shielded notes end to end, with native proofs: Alice shields her
//! deposit, pays Bob, Bob pays her back, she unshields part of hers (a
//! claim takes transparent notes).
//! The sequencer's state shows no shielded value or owner.

use vlichen_ledger::{regtest_hub, regtest_sequencer, Leaf, Mint, Tx, HUB_KEY};
use vlichen_sequencer::{Options, Sequencer};
use vlichen_wallet::prelude::*;

#[test]
fn shield_pay_and_unshield() {
    let dir = std::env::temp_dir().join(format!("vlichen-shielded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let seq = Sequencer::start_with("127.0.0.1:0", Some(dir.join("ledger.json")), regtest_sequencer(), HUB_KEY, Options { native_proofs: true, ..Default::default() }).unwrap();
    let mut alice = vlichen_wallet::create(&dir.join("alice.json"), &seq.url).unwrap();
    let mut bob = vlichen_wallet::create(&dir.join("bob.json"), &seq.url).unwrap();
    alice.l2.seq().submit(&Tx::Mint(Mint::new(&regtest_hub(), &alice.l2.seq().deployment, vec![vlichen_ledger::note(alice.me(), 100, 0)]))).unwrap();
    alice.l2.seq().seal().unwrap();

    // Alice shields 70 (30 stays transparent)
    alice.shield(70).unwrap();
    assert_eq!(alice.balances().unwrap(), (30, 70));
    // she pays Bob 45 from her shielded notes; Bob pays 5 back
    let to_bob = bob.shielded_address();
    alice.send_shielded(&to_bob, 45).unwrap();
    assert_eq!(alice.balances().unwrap(), (30, 25));
    assert_eq!(bob.balances().unwrap(), (0, 45));
    bob.send_shielded(&alice.shielded_address(), 5).unwrap();
    assert_eq!((alice.balances().unwrap(), bob.balances().unwrap()), ((30, 30), (0, 40)));
    assert!(bob.send_shielded(&alice.shielded_address(), 41).is_err(), "Bob has 40");
    // the state shows commitments and ciphertexts, no shielded value or
    // owner: the transparent notes are the mint, Alice's change and nothing else
    let state = alice.l2.seq().state().unwrap();
    assert_eq!(state.notes.iter().filter(|l| matches!(l, Leaf::Shielded { .. })).count(), 5);
    assert_eq!(state.transparent().map(|(_, n)| n.value).collect::<Vec<_>>(), vec![100, 30]);

    // Alice unshields 28: a transparent note of hers
    let r = alice.unshield(28).unwrap();
    assert_eq!(alice.balances().unwrap(), (58, 2));
    alice.l2.seq().seal().unwrap();
    let state = alice.l2.seq().state().unwrap();
    assert_eq!((state.note(r).unwrap().owner, state.note(r).unwrap().value), (alice.me(), 28));

    // a sequencer that accepts no native proofs refuses them
    drop(seq);
    let seq = Sequencer::start("127.0.0.1:0", Some(dir.join("ledger.json")), regtest_sequencer(), HUB_KEY).unwrap();
    let mut bob = vlichen_wallet::open(&dir.join("bob.json"), &seq.url).unwrap();
    assert_eq!(bob.balances().unwrap(), (0, 40), "the restarted sequencer kept the shielded notes");
    let a = alice.shielded_address();
    assert!(bob.send_shielded(&a, 1).unwrap_err().to_string().contains("no native proofs"));

    // every height's recorded block, checked by the chain statement, runs
    // on from the last: from genesis to the last sealed root
    let blocks = vlichen_sequencer::blocks_dir(&dir.join("ledger.json"));
    let id = [0xc4; 32];
    let mut prev: Option<vlichen_validity::ChainJournal> = None;
    let last = bob.l2.seq().state().unwrap();
    for h in 1.. {
        let Ok(b) = std::fs::read(blocks.join(format!("{h}.input"))) else { break };
        let input = vlichen_validity::BlockInput::from_bytes(&b).unwrap();
        let p = prev.as_ref().map(|j| j.to_bytes());
        let j = vlichen_validity::chain(&id, &input, &mut |_| true, &mut |jb| p.as_deref() == Some(jb)).unwrap();
        assert_eq!(j.height, h);
        prev = Some(j);
    }
    let sealed = last.sealed.last().unwrap();
    let j = prev.expect("blocks recorded");
    assert_eq!((j.height, j.root), (sealed.height, sealed.root()), "the chain ends at the last sealed root");
    let _ = std::fs::remove_dir_all(&dir);
}
