//! End to end in a local NEAR sandbox: NEARMISS collection + market.
//! Build both wasm files first (cargo near build in ../contract and here).
use near_workspaces::types::NearToken;
use serde_json::json;

fn near(n: f64) -> NearToken { NearToken::from_yoctonear((n * 1e24) as u128) }
fn f(t: NearToken) -> f64 { t.as_yoctonear() as f64 / 1e24 }

#[tokio::test]
async fn list_buy_offer_accept() -> anyhow::Result<()> {
    let sb = near_workspaces::sandbox().await?;
    let nft = sb.dev_deploy(&std::fs::read("../nearmiss/target/near/nearmiss.wasm")?).await?;
    let market = sb.dev_deploy(&std::fs::read("target/near/nearmiss_market.wasm")?).await?;
    let admin = sb.dev_create_account().await?;
    let nft_treasury = sb.dev_create_account().await?;
    let mkt_treasury = sb.dev_create_account().await?;
    let seller = sb.dev_create_account().await?;
    let buyer = sb.dev_create_account().await?;
    let bidder = sb.dev_create_account().await?;

    nft.call("new").args_json(json!({"owner": admin.id(), "treasury": nft_treasury.id(),
        "price": near(0.12).as_yoctonear().to_string(), "base_uri": "https://nearmiss.fun/ipfs", "royalty_bps": 500}))
        .transact().await?.into_result()?;
    admin.call(nft.id(), "set_media").args_json(json!({"media_cid": "a", "refs_cid": "b"})).transact().await?.into_result()?;
    admin.call(nft.id(), "set_open").args_json(json!({"open": true})).transact().await?.into_result()?;
    market.call("new").args_json(json!({"owner": admin.id(), "treasury": mkt_treasury.id(), "fee_bps": 250}))
        .transact().await?.into_result()?;
    admin.call(market.id(), "add_collection").args_json(json!({"nft_contract_id": nft.id()})).transact().await?.into_result()?;

    // closed market refuses listings
    let token: String = seller.call(nft.id(), "nft_mint_random").args_json(json!({"count": 1}))
        .deposit(near(0.12)).max_gas().transact().await?.json::<Vec<String>>()?.remove(0);
    seller.call(market.id(), "storage_deposit").args_json(json!({})).deposit(near(0.01)).transact().await?.into_result()?;
    let r = seller.call(nft.id(), "nft_approve").args_json(json!({"token_id": token, "account_id": market.id(),
        "msg": json!({"price": near(1.0).as_yoctonear().to_string()}).to_string()})).deposit(near(0.01)).max_gas().transact().await?;
    let listed: Option<serde_json::Value> = market.view("get_listing").args_json(json!({"nft_contract_id": nft.id(), "token_id": token})).await?.json()?;
    assert!(listed.is_none(), "listing accepted while closed: {:?}", r.logs());

    admin.call(market.id(), "set_open").args_json(json!({"open": true})).transact().await?.into_result()?;
    seller.call(nft.id(), "nft_approve").args_json(json!({"token_id": token, "account_id": market.id(),
        "msg": json!({"price": near(1.0).as_yoctonear().to_string()}).to_string()})).deposit(near(0.01)).max_gas()
        .transact().await?.into_result()?;
    let listed: serde_json::Value = market.view("get_listing").args_json(json!({"nft_contract_id": nft.id(), "token_id": token})).await?.json()?;
    assert_eq!(listed["price"], near(1.0).as_yoctonear().to_string());

    // buy for 1 NEAR, attaching 1.2 (0.2 must come back)
    let m0 = market.view_account().await?.balance;
    let n0 = nft.view_account().await?.balance;
    // let gas refunds from earlier calls land before taking a baseline
    sb.fast_forward(3).await?;
    let (s0, nt0, mt0, b0) = (seller.view_account().await?.balance, nft_treasury.view_account().await?.balance,
        mkt_treasury.view_account().await?.balance, buyer.view_account().await?.balance);
    let res = buyer.call(market.id(), "buy").args_json(json!({"nft_contract_id": nft.id(), "token_id": token}))
        .deposit(near(1.2)).max_gas().transact().await?;
    assert!(res.is_success(), "{:?}", res.failures());
    let owner: serde_json::Value = nft.view("nft_token").args_json(json!({"token_id": token})).await?.json()?;
    assert_eq!(owner["owner_id"], buyer.id().to_string());
    sb.fast_forward(3).await?;
    let (s1, nt1, mt1, b1) = (seller.view_account().await?.balance, nft_treasury.view_account().await?.balance,
        mkt_treasury.view_account().await?.balance, buyer.view_account().await?.balance);
    let m1 = market.view_account().await?.balance;
    let n1 = nft.view_account().await?.balance;
    assert!((f(m1) - f(m0)).abs() < 0.001, "market contract balance moved");
    assert!((f(n1) - f(n0)).abs() < 0.002, "NFT contract balance moved");
    println!("sale 1 NEAR → seller +{:.4}, royalty +{:.4}, market fee +{:.4}, buyer -{:.4}",
        f(s1) - f(s0), f(nt1) - f(nt0), f(mt1) - f(mt0), f(b0) - f(b1));
    assert!((f(mt1) - f(mt0) - 0.025).abs() < 1e-6, "market fee");
    assert!((f(nt1) - f(nt0) - 0.04875).abs() < 1e-4, "royalty 5% of net");
    assert!((f(s1) - f(s0) - 0.92625).abs() < 0.005, "seller proceeds");
    assert!(f(b0) - f(b1) < 1.01, "overpayment refunded");

    // offer 0.5 from bidder, accepted by the new holder
    bidder.call(market.id(), "make_offer").args_json(json!({"nft_contract_id": nft.id(), "token_id": token}))
        .deposit(near(0.51)).transact().await?.into_result()?;
    let offers: Vec<serde_json::Value> = market.view("get_offers").args_json(json!({"nft_contract_id": nft.id(), "token_id": token})).await?.json()?;
    assert_eq!(offers.len(), 1);
    let res = buyer.call(nft.id(), "nft_approve").args_json(json!({"token_id": token, "account_id": market.id(),
        "msg": json!({"accept_offer": bidder.id()}).to_string()})).deposit(near(0.01)).max_gas().transact().await?;
    assert!(res.is_success(), "{:?}", res.failures());
    let owner: serde_json::Value = nft.view("nft_token").args_json(json!({"token_id": token})).await?.json()?;
    assert_eq!(owner["owner_id"], bidder.id().to_string());

    // withdraw path: a second offer is refunded in full
    let t2: String = seller.call(nft.id(), "nft_mint_random").args_json(json!({"count": 1}))
        .deposit(near(0.12)).max_gas().transact().await?.json::<Vec<String>>()?.remove(0);
    sb.fast_forward(3).await?;
    let w0 = bidder.view_account().await?.balance;
    bidder.call(market.id(), "make_offer").args_json(json!({"nft_contract_id": nft.id(), "token_id": t2}))
        .deposit(near(0.31)).transact().await?.into_result()?;
    bidder.call(market.id(), "withdraw_offer").args_json(json!({"nft_contract_id": nft.id(), "token_id": t2}))
        .deposit(NearToken::from_yoctonear(1)).transact().await?.into_result()?;
    sb.fast_forward(3).await?;
    let w1 = bidder.view_account().await?.balance;
    assert!(f(w0) - f(w1) < 0.005, "withdrawn offer not refunded: lost {}", f(w0) - f(w1));

    // stale listing: seller lists, then transfers the NFT away; a buy must refund
    seller.call(nft.id(), "nft_approve").args_json(json!({"token_id": t2, "account_id": market.id(),
        "msg": json!({"price": near(0.5).as_yoctonear().to_string()}).to_string()})).deposit(near(0.01)).max_gas()
        .transact().await?.into_result()?;
    seller.call(nft.id(), "nft_transfer").args_json(json!({"receiver_id": admin.id(), "token_id": t2}))
        .deposit(NearToken::from_yoctonear(1)).max_gas().transact().await?.into_result()?;
    sb.fast_forward(3).await?;
    let x0 = buyer.view_account().await?.balance;
    let res = buyer.call(market.id(), "buy").args_json(json!({"nft_contract_id": nft.id(), "token_id": t2}))
        .deposit(near(0.5)).max_gas().transact().await?;
    sb.fast_forward(3).await?;
    let x1 = buyer.view_account().await?.balance;
    println!("stale buy: logs {:?}; buyer lost {:.4}", res.logs().iter().filter(|l| l.contains("sale")).collect::<Vec<_>>(), f(x0) - f(x1));
    assert!(f(x0) - f(x1) < 0.01, "stale buy kept the money");
    Ok(())
}
