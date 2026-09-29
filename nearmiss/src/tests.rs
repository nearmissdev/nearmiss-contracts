use super::*;
use near_sdk::test_utils::{accounts, VMContextBuilder};
use near_sdk::testing_env;
use std::collections::HashSet;

const PRICE: u128 = 120_000_000_000_000_000_000_000; // 0.12 NEAR

fn ctx(predecessor: AccountId, deposit: u128, seed: u8) -> VMContextBuilder {
    let mut b = VMContextBuilder::new();
    b.current_account_id("nearmissboard.near".parse().unwrap())
        .predecessor_account_id(predecessor)
        .attached_deposit(NearToken::from_yoctonear(deposit))
        .account_balance(NearToken::from_near(10))
        .random_seed([seed; 32]);
    b
}

fn setup() -> Contract {
    testing_env!(ctx(accounts(0), 0, 1).build());
    let mut c = Contract::new(accounts(0), accounts(1), U128(PRICE), "https://ipfs.io/ipfs".into(), None, None, 500);
    c.set_media("bafyimages".into(), "bafyrefs".into(), None);
    c.set_open(true);
    c
}

#[test]
fn mints_distinct_files_and_reports_info() {
    let mut c = setup();
    testing_env!(ctx(accounts(2), PRICE * 5, 7).build());
    let ids = c.nft_mint_random(Some(5));
    assert_eq!(ids.len(), 5);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 5);
    let info = c.nm_info();
    assert_eq!(info.minted, 5);
    assert_eq!(info.remaining, TOTAL - 5);
    let t = c.nft_token(ids[0].clone()).unwrap();
    assert_eq!(t.owner_id, accounts(2));
    let m = t.metadata.unwrap();
    let n: u32 = ids[0].parse().unwrap();
    assert_eq!(m.title.unwrap(), format!("NEARMISS #{:04}", n));
    assert_eq!(m.media.unwrap(), format!("bafyimages/{:04}.png", n));
    assert_eq!(m.reference.unwrap(), format!("bafyrefs/{:04}.json", n));
}

#[test]
fn draws_every_file_exactly_once_then_closes() {
    let mut c = setup();
    let mut seen = HashSet::new();
    let mut seed: u8 = 0;
    while c.nm_info().remaining > 0 {
        seed = seed.wrapping_add(1);
        let n = c.nm_info().remaining.min(5) as u8;
        let mut b = ctx(accounts(3), PRICE * n as u128, seed);
        b.block_height(seed as u64 + seen.len() as u64);
        testing_env!(b.build());
        for id in c.nft_mint_random(Some(n)) {
            let v: u32 = id.parse().unwrap();
            assert!((1..=TOTAL).contains(&v));
            assert!(seen.insert(v), "file {} issued twice", v);
        }
    }
    assert_eq!(seen.len(), TOTAL as usize);
    assert_eq!(c.nft_total_supply().0, TOTAL as u128);
}

#[test]
fn full_request_while_many_files_remain() {
    // regression: remaining (u32) must never be truncated to u8
    for left in [3072u32, 256, 512, 3333] {
        let mut c = setup();
        c.minted = TOTAL - left;
        testing_env!(ctx(accounts(2), PRICE * 5, 11).build());
        assert_eq!(c.nft_mint_random(Some(5)).len(), 5, "left = {}", left);
    }
}

#[test]
fn partial_fill_when_archive_runs_low() {
    let mut c = setup();
    c.minted = TOTAL - 3;
    testing_env!(ctx(accounts(2), PRICE * 5, 5).build());
    let ids = c.nft_mint_random(Some(5));
    assert_eq!(ids.len(), 3);
    assert_eq!(c.nm_info().remaining, 0);
}

#[test]
#[should_panic(expected = "Every file has been issued")]
fn refuses_after_sell_out() {
    let mut c = setup();
    c.minted = TOTAL;
    testing_env!(ctx(accounts(2), PRICE, 3).build());
    c.nft_mint_random(Some(1));
}

#[test]
#[should_panic(expected = "Attach at least")]
fn refuses_underpayment() {
    let mut c = setup();
    testing_env!(ctx(accounts(2), PRICE * 2 - 1, 3).build());
    c.nft_mint_random(Some(2));
}

#[test]
#[should_panic(expected = "Request 1 to 5 files")]
fn refuses_more_than_five() {
    let mut c = setup();
    testing_env!(ctx(accounts(2), PRICE * 6, 3).build());
    c.nft_mint_random(Some(6));
}

#[test]
#[should_panic(expected = "The archive is closed")]
fn refuses_when_closed() {
    let mut c = setup();
    c.set_open(false);
    testing_env!(ctx(accounts(2), PRICE, 3).build());
    c.nft_mint_random(None);
}

#[test]
#[should_panic(expected = "Owner only")]
fn owner_only_price() {
    let mut c = setup();
    testing_env!(ctx(accounts(4), 0, 3).build());
    c.set_price(U128(1));
}

#[test]
#[should_panic(expected = "Media is frozen")]
fn freeze_is_permanent() {
    let mut c = setup();
    c.freeze_media();
    c.set_media("x".into(), "y".into(), None);
}

#[test]
fn payout_splits_royalty() {
    let mut c = setup();
    testing_env!(ctx(accounts(2), PRICE, 9).build());
    let id = c.nft_mint_random(None).remove(0);
    let p = c.nft_payout(id, U128(10_000_000), Some(10)).payout;
    assert_eq!(p.get(&accounts(1)).unwrap().0, 500_000); // 5% to treasury
    assert_eq!(p.get(&accounts(2)).unwrap().0, 9_500_000);
}

#[test]
fn distribution_is_not_sequential() {
    let mut c = setup();
    testing_env!(ctx(accounts(2), PRICE * 5, 42).build());
    let ids: Vec<u32> = c.nft_mint_random(Some(5)).iter().map(|s| s.parse().unwrap()).collect();
    assert_ne!(ids, vec![1, 2, 3, 4, 5]);
}
