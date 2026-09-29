use super::*;
use near_sdk::test_utils::{accounts, VMContextBuilder};
use near_sdk::testing_env;
use std::collections::HashSet;

const PRICE: u128 = 100_000_000_000_000_000_000_000; // 0.1 NEAR

fn ctx(who: AccountId, deposit: u128, seed: u8, now_ms: u64) -> VMContextBuilder {
    let mut b = VMContextBuilder::new();
    b.current_account_id("cool.launchpad.nearmissboard.near".parse().unwrap())
        .predecessor_account_id(who)
        .attached_deposit(NearToken::from_yoctonear(deposit))
        .account_balance(NearToken::from_near(10))
        .block_timestamp(now_ms * 1_000_000)
        .random_seed([seed; 32]);
    b
}

fn cfg(total: u32, sequential: bool) -> Config {
    Config {
        name: "Cool Cats".into(), symbol: "COOL".into(), total, price: U128(PRICE), max_per_tx: 5,
        start_ms: None, sequential, media_ext: "png".into(), media_cid: "bafyimg".into(), refs_cid: "bafyref".into(),
        royalty_bps: 750,
    }
}

fn make(c: Config) -> Contract {
    testing_env!(ctx(accounts(0), 0, 1, 1_000).build());
    Contract::new(accounts(0), accounts(0), accounts(1), 250, "https://nearmiss.fun/ipfs".into(), c, true)
}

#[test]
fn random_mode_issues_every_token_once() {
    let mut c = make(cfg(47, false));
    let mut seen = HashSet::new();
    let mut s = 0u8;
    while c.nm_info().remaining > 0 {
        s = s.wrapping_add(1);
        let n = c.nm_info().remaining.min(5) as u8;
        testing_env!(ctx(accounts(2), PRICE * n as u128, s, 2_000).build());
        for id in c.nft_mint_random(Some(n)) {
            assert!(seen.insert(id.parse::<u32>().unwrap()));
        }
    }
    assert_eq!(seen, (1..=47).collect());
}

#[test]
fn sequential_mode_counts_up() {
    let mut c = make(cfg(10, true));
    testing_env!(ctx(accounts(2), PRICE * 3, 9, 2_000).build());
    assert_eq!(c.nft_mint_random(Some(3)), vec!["1", "2", "3"]);
    testing_env!(ctx(accounts(3), PRICE * 2, 9, 2_000).build());
    assert_eq!(c.nft_mint_random(Some(2)), vec!["4", "5"]);
    let t = c.nft_token("4".into()).unwrap();
    assert_eq!(t.metadata.unwrap().title.unwrap(), "Cool Cats #4");
}

#[test]
#[should_panic(expected = "Minting has not started yet")]
fn respects_start_time() {
    let mut k = cfg(10, true);
    k.start_ms = Some(5_000);
    let mut c = make(k);
    testing_env!(ctx(accounts(2), PRICE, 1, 4_999).build());
    c.nft_mint_random(None);
}

#[test]
fn opens_at_start_time() {
    let mut k = cfg(10, true);
    k.start_ms = Some(5_000);
    let mut c = make(k);
    testing_env!(ctx(accounts(2), PRICE, 1, 5_000).build());
    assert_eq!(c.nft_mint_random(None).len(), 1);
}

#[test]
#[should_panic(expected = "Royalty above 7.5%")]
fn royalty_cap() {
    let mut k = cfg(10, true);
    k.royalty_bps = 751;
    make(k);
}

#[test]
#[should_panic(expected = "Platform fee above 2.5%")]
fn platform_fee_cap() {
    testing_env!(ctx(accounts(0), 0, 1, 1_000).build());
    Contract::new(accounts(0), accounts(0), accounts(1), 251, "x".into(), cfg(10, true), true);
}

#[test]
#[should_panic(expected = "Price must be at least 0.02 NEAR")]
fn minimum_price() {
    let mut k = cfg(10, true);
    k.price = U128(19_999_999_999_999_999_999_999);
    make(k);
}

#[test]
#[should_panic(expected = "Supply must be 1 to 10,000")]
fn supply_cap() {
    make(cfg(10_001, true));
}

#[test]
#[should_panic(expected = "Price is fixed once minting has begun")]
fn price_locked_after_first_mint() {
    let mut c = make(cfg(10, true));
    testing_env!(ctx(accounts(2), PRICE, 1, 2_000).build());
    c.nft_mint_random(None);
    testing_env!(ctx(accounts(0), 0, 1, 2_000).build());
    c.set_price(U128(PRICE * 2));
}

#[test]
#[should_panic(expected = "Creator only")]
fn creator_only() {
    let mut c = make(cfg(10, true));
    testing_env!(ctx(accounts(4), 0, 1, 2_000).build());
    c.set_open(false);
}

#[test]
fn royalty_goes_to_creator() {
    let mut c = make(cfg(10, true));
    testing_env!(ctx(accounts(2), PRICE, 1, 2_000).build());
    let id = c.nft_mint_random(None).remove(0);
    let p = c.nft_payout(id, U128(10_000_000), Some(10)).payout;
    assert_eq!(p.get(&accounts(0)).unwrap().0, 750_000);
    assert_eq!(p.get(&accounts(2)).unwrap().0, 9_250_000);
}
