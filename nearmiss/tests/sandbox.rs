//! Runs the compiled wasm in a local NEAR sandbox: real runtime, real storage
//! costs, real transfers. Build first: cargo near build non-reproducible-wasm
use near_workspaces::types::NearToken;
use serde_json::json;

const PRICE: NearToken = NearToken::from_millinear(120);

#[tokio::test]
async fn mint_pays_treasury_and_refunds() -> anyhow::Result<()> {
    let wasm = std::fs::read("target/near/nearmiss.wasm")?;
    let sb = near_workspaces::sandbox().await?;
    let contract = sb.dev_deploy(&wasm).await?;
    let owner = sb.dev_create_account().await?;
    let treasury = sb.dev_create_account().await?;
    let buyer = sb.dev_create_account().await?;

    contract.call("new").args_json(json!({
        "owner": owner.id(), "treasury": treasury.id(), "price": PRICE.as_yoctonear().to_string(),
        "base_uri": "https://ipfs.io/ipfs", "royalty_bps": 500
    })).transact().await?.into_result()?;
    owner.call(contract.id(), "set_media").args_json(json!({"media_cid": "bafyimg", "refs_cid": "bafyref"}))
        .transact().await?.into_result()?;
    owner.call(contract.id(), "set_open").args_json(json!({"open": true})).transact().await?.into_result()?;

    let t0 = treasury.view_account().await?.balance;
    let b0 = buyer.view_account().await?.balance;
    let c0 = contract.view_account().await?.balance;
    // pay for 3 plus 0.05 extra: the extra must come back
    let res = buyer.call(contract.id(), "nft_mint_random").args_json(json!({"count": 3}))
        .deposit(NearToken::from_yoctonear(PRICE.as_yoctonear() * 3 + NearToken::from_millinear(50).as_yoctonear()))
        .max_gas().transact().await?;
    assert!(res.is_success(), "{:?}", res.failures());
    let logs = res.logs().join("\n");
    let ids: Vec<String> = res.json()?;
    assert_eq!(ids.len(), 3);
    assert!(logs.contains("EVENT_JSON") && logs.contains("nft_mint"), "{logs}");

    let t1 = treasury.view_account().await?.balance;
    let b1 = buyer.view_account().await?.balance;
    let c1 = contract.view_account().await?.balance;
    let proceeds = t1.as_yoctonear() - t0.as_yoctonear();
    let spent = b0.as_yoctonear() - b1.as_yoctonear();
    let kept = c1.as_yoctonear() as i128 - c0.as_yoctonear() as i128;
    println!("treasury +{} NEAR, buyer -{} NEAR (incl. gas), contract +{} yN", proceeds as f64 / 1e24, spent as f64 / 1e24, kept);
    assert!(proceeds > PRICE.as_yoctonear() * 3 * 9 / 10, "treasury got too little: {proceeds}");
    assert!(spent < PRICE.as_yoctonear() * 3 + NearToken::from_millinear(10).as_yoctonear(), "overpayment not refunded");

    let toks: serde_json::Value = contract.view("nft_tokens_for_owner").args_json(json!({"account_id": buyer.id()})).await?.json()?;
    assert_eq!(toks.as_array().unwrap().len(), 3);
    let info: serde_json::Value = contract.view("nm_info").await?.json()?;
    assert_eq!(info["minted"], 3);

    // underpaying fails and the deposit comes back (only gas is spent)
    let b2 = buyer.view_account().await?.balance;
    let bad = buyer.call(contract.id(), "nft_mint_random").args_json(json!({"count": 1}))
        .deposit(NearToken::from_millinear(100)).max_gas().transact().await?;
    assert!(bad.is_failure());
    let b3 = buyer.view_account().await?.balance;
    sb.fast_forward(5).await?;
    let b4 = buyer.view_account().await?.balance;
    let lost_now = b2.as_yoctonear() as f64 / 1e24 - b3.as_yoctonear() as f64 / 1e24;
    let lost_later = b2.as_yoctonear() as f64 / 1e24 - b4.as_yoctonear() as f64 / 1e24;
    println!("failed mint: buyer down {lost_now:.5} NEAR right after, {lost_later:.5} NEAR after 5 blocks");
    assert!(lost_later < 0.005, "failed mint kept the deposit");
    Ok(())
}
