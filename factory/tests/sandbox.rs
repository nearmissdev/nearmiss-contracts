//! Launchpad end to end in a local sandbox: store the collection code, create a
//! collection, mint from it, check the fee split; then a failing create refunds.
use near_workspaces::types::NearToken;
use serde_json::json;

fn near(n: f64) -> NearToken { NearToken::from_yoctonear((n * 1e24) as u128) }
fn f(t: NearToken) -> f64 { t.as_yoctonear() as f64 / 1e24 }

#[tokio::test]
async fn create_mint_and_refund() -> anyhow::Result<()> {
    let sb = near_workspaces::sandbox().await?;
    let root = sb.root_account()?;
    let lp = root.create_subaccount("launchpad").initial_balance(near(10.0)).transact().await?.into_result()?;
    let lpc = lp.deploy(&std::fs::read("target/near/nearmiss_launchpad.wasm")?).await?.into_result()?;
    let admin = sb.dev_create_account().await?;
    let platform = sb.dev_create_account().await?;
    let creator = sb.dev_create_account().await?;
    let buyer = sb.dev_create_account().await?;
    lp.call(lp.id(), "new").args_json(json!({"owner": admin.id(), "platform_treasury": platform.id(),
        "platform_fee_bps": 250, "base_uri": "https://nearmiss.fun/ipfs"})).transact().await?.into_result()?;
    let code = std::fs::read("../collection/target/near/nearmiss_collection.wasm")?;
    admin.call(lp.id(), "set_code").args(code).max_gas().transact().await?.into_result()?;

    let cfg = json!({"name": "Night Shift", "symbol": "NIGHT", "total": 20, "price": near(0.5).as_yoctonear().to_string(),
        "max_per_tx": 5, "start_ms": null, "sequential": false, "media_ext": "png", "media_cid": "bafyimg",
        "refs_cid": "bafyref", "royalty_bps": 750});
    let profile = json!({"description": "Photographs from the late shift.", "cover": null,
        "links": {"x": "https://x.com/nightshift", "telegram": "https://t.me/nightshift", "discord": null, "website": null}});

    // closed launchpad refuses creators
    let need: String = lpc.view("required_deposit").await?.json()?;
    let need = NearToken::from_yoctonear(need.parse()?);
    println!("required deposit {:.4} NEAR", f(need));
    let r = creator.call(lp.id(), "create_collection").args_json(json!({"slug": "night-shift", "config": cfg, "profile": profile, "open": true}))
        .deposit(need).max_gas().transact().await?;
    assert!(r.is_failure(), "created while closed");
    admin.call(lp.id(), "set_open").args_json(json!({"open": true})).transact().await?.into_result()?;

    let r = creator.call(lp.id(), "create_collection").args_json(json!({"slug": "night-shift", "config": cfg, "profile": profile, "open": true}))
        .deposit(need).max_gas().transact().await?;
    assert!(r.is_success(), "{:?}", r.failures());
    let col: serde_json::Value = lpc.view("get_collection").args_json(json!({"slug": "night-shift"})).await?.json()?;
    let cid = col["contract_id"].as_str().unwrap().to_string();
    println!("created {cid}; links {}", col["profile"]["links"]);
    let cid: near_workspaces::AccountId = cid.parse()?;
    let info: serde_json::Value = sb.view(&cid, "nm_info").await?.json()?;
    assert_eq!(info["total"], 20);
    assert_eq!(info["owner"], creator.id().to_string());

    // mint 2 at 0.5: platform gets 2.5% of 1.0, creator the rest minus storage
    sb.fast_forward(3).await?;
    let (p0, c0) = (platform.view_account().await?.balance, creator.view_account().await?.balance);
    let r = buyer.call(&cid, "nft_mint_random").args_json(json!({"count": 2})).deposit(near(1.0)).max_gas().transact().await?;
    assert!(r.is_success(), "{:?}", r.failures());
    sb.fast_forward(3).await?;
    let (p1, c1) = (platform.view_account().await?.balance, creator.view_account().await?.balance);
    println!("mint 2 × 0.5 → platform +{:.4}, creator +{:.4}", f(p1) - f(p0), f(c1) - f(c0));
    assert!((f(p1) - f(p0) - 0.025).abs() < 1e-6);
    assert!(f(c1) - f(c0) > 0.95 && f(c1) - f(c0) < 0.975);

    // a bad config fails inside the batch: the creator must get the deposit back
    let bad = json!({"name": "Bad", "symbol": "BAD", "total": 20, "price": near(0.5).as_yoctonear().to_string(),
        "max_per_tx": 5, "start_ms": null, "sequential": true, "media_ext": "png", "media_cid": "a", "refs_cid": "b", "royalty_bps": 900});
    sb.fast_forward(3).await?;
    let b0 = creator.view_account().await?.balance;
    let r = creator.call(lp.id(), "create_collection").args_json(json!({"slug": "bad-one", "config": bad, "profile": profile, "open": true}))
        .deposit(need).max_gas().transact().await?;
    let created: bool = r.json().unwrap_or(false);
    assert!(!created);
    sb.fast_forward(5).await?;
    let b1 = creator.view_account().await?.balance;
    println!("failed create: creator lost {:.4} NEAR (gas only)", f(b0) - f(b1));
    assert!(f(b0) - f(b1) < 0.05, "deposit not refunded");
    let gone: Option<serde_json::Value> = lpc.view("get_collection").args_json(json!({"slug": "bad-one"})).await?.json()?;
    assert!(gone.is_none());
    let avail: bool = lpc.view("slug_available").args_json(json!({"slug": "bad-one"})).await?.json()?;
    assert!(avail, "slug stuck in pending");

    // creator edits links; a stranger cannot
    let p2 = json!({"description": "New.", "cover": null, "links": {"x": null, "telegram": null, "discord": "https://discord.gg/abc", "website": null}});
    creator.call(lp.id(), "update_profile").args_json(json!({"slug": "night-shift", "profile": p2})).transact().await?.into_result()?;
    assert!(buyer.call(lp.id(), "update_profile").args_json(json!({"slug": "night-shift", "profile": p2})).transact().await?.is_failure());
    let list: Vec<serde_json::Value> = lpc.view("list_collections").args_json(json!({})).await?.json()?;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["profile"]["links"]["discord"], "https://discord.gg/abc");
    Ok(())
}
