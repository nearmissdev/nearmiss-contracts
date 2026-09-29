//! NEARMISS market — fixed-price listings and escrowed offers for approved
//! NEP-171 collections (NEARMISS first, launchpad collections later).
//!
//! Listing: the holder calls `nft_approve(token, market, msg)` on the
//! collection with `{"price":"<yocto>"}`. The NFT stays in the holder's wallet
//! until it sells. Accepting an offer uses the same path with
//! `{"accept_offer":"<buyer>"}`.
//!
//! Every sale goes through `nft_transfer_payout` (NEP-199): the market fee is
//! taken first, the rest is split by the collection's payout (royalty + seller).
//! If the transfer fails, the buyer is refunded in full.
use near_sdk::collections::{LookupMap, UnorderedMap, UnorderedSet};
use near_sdk::json_types::U128;
use near_sdk::serde_json::{self, json};
use near_sdk::{
    assert_one_yocto, env, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue,
};
use std::collections::HashMap;

/// Storage the market holds back per listing (paid from the seller's storage balance).
pub const LISTING_STORAGE: NearToken = NearToken::from_millinear(10);
/// Storage deposit added on top of every offer, refunded when the offer ends.
pub const OFFER_STORAGE: NearToken = NearToken::from_millinear(10);
const MAX_FEE_BPS: u16 = 1000;
const GAS_TRANSFER: Gas = Gas::from_tgas(50);
const GAS_RESOLVE: Gas = Gas::from_tgas(40);

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Collections,
    Listings,
    ByCollection,
    ByCollectionInner { hash: Vec<u8> },
    Offers,
    OffersByToken,
    OffersByTokenInner { hash: Vec<u8> },
    Storage,
    ListingsBySeller,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Listing {
    pub nft_contract_id: AccountId,
    pub token_id: String,
    pub owner_id: AccountId,
    pub approval_id: u64,
    pub price: U128,
    pub listed_at: u64,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Offer {
    pub nft_contract_id: AccountId,
    pub token_id: String,
    pub buyer_id: AccountId,
    pub amount: U128,
    pub made_at: u64,
}

#[near(serializers = [json])]
pub struct Payout {
    pub payout: HashMap<AccountId, U128>,
}

#[near(serializers = [json])]
pub struct Info {
    pub owner: AccountId,
    pub treasury: AccountId,
    pub fee_bps: u16,
    pub open: bool,
    pub collections: Vec<AccountId>,
    pub listing_storage: U128,
    pub offer_storage: U128,
}

#[near(serializers = [json])]
#[serde(untagged)]
enum ApproveMsg {
    List { price: U128 },
    AcceptOffer { accept_offer: AccountId },
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Market {
    owner: AccountId,
    treasury: AccountId,
    fee_bps: u16,
    open: bool,
    collections: UnorderedSet<AccountId>,
    listings: UnorderedMap<String, Listing>,
    by_collection: LookupMap<AccountId, UnorderedSet<String>>,
    offers: UnorderedMap<String, Offer>,
    offers_by_token: LookupMap<String, UnorderedSet<AccountId>>,
    storage: LookupMap<AccountId, u128>,
    listings_by_seller: LookupMap<AccountId, u32>,
}

fn key(contract: &AccountId, token: &str) -> String {
    format!("{}||{}", contract, token)
}

fn offer_key(contract: &AccountId, token: &str, buyer: &AccountId) -> String {
    format!("{}||{}||{}", contract, token, buyer)
}

fn emit(event: &str, data: near_sdk::serde_json::Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        json!({"standard": "nearmiss_market", "version": "1.0.0", "event": event, "data": [data]})
    ));
}

#[near]
impl Market {
    #[init]
    pub fn new(owner: AccountId, treasury: AccountId, fee_bps: u16) -> Self {
        require!(fee_bps <= MAX_FEE_BPS, "Fee above 10%");
        Self {
            owner,
            treasury,
            fee_bps,
            open: false,
            collections: UnorderedSet::new(Key::Collections),
            listings: UnorderedMap::new(Key::Listings),
            by_collection: LookupMap::new(Key::ByCollection),
            offers: UnorderedMap::new(Key::Offers),
            offers_by_token: LookupMap::new(Key::OffersByToken),
            storage: LookupMap::new(Key::Storage),
            listings_by_seller: LookupMap::new(Key::ListingsBySeller),
        }
    }

    // ------------------------------------------------------------ storage (listing deposits)

    /// Pay for listing slots. Each active listing holds back 0.01 NEAR.
    #[payable]
    pub fn storage_deposit(&mut self, account_id: Option<AccountId>) -> U128 {
        let who = account_id.unwrap_or_else(env::predecessor_account_id);
        let bal = self.storage.get(&who).unwrap_or(0) + env::attached_deposit().as_yoctonear();
        self.storage.insert(&who, &bal);
        U128(bal)
    }

    /// Withdraw storage balance not held by active listings.
    #[payable]
    pub fn storage_withdraw(&mut self) -> U128 {
        assert_one_yocto();
        let who = env::predecessor_account_id();
        let bal = self.storage.get(&who).unwrap_or(0);
        let held = self.held(&who);
        let free = bal.saturating_sub(held);
        if free > 0 {
            self.storage.insert(&who, &held);
            Promise::new(who).transfer(NearToken::from_yoctonear(free)).detach();
        }
        U128(free)
    }

    pub fn storage_balance_of(&self, account_id: AccountId) -> U128 {
        U128(self.storage.get(&account_id).unwrap_or(0))
    }

    pub fn storage_minimum_balance(&self) -> U128 {
        U128(LISTING_STORAGE.as_yoctonear())
    }

    fn held(&self, who: &AccountId) -> u128 {
        self.listings_by_seller.get(who).unwrap_or(0) as u128 * LISTING_STORAGE.as_yoctonear()
    }

    // ------------------------------------------------------------ listing via nft_approve

    /// Called by an approved collection after `nft_approve(token, market, msg)`.
    pub fn nft_on_approve(&mut self, token_id: String, owner_id: AccountId, approval_id: u64, msg: String) -> PromiseOrValue<()> {
        let nft = env::predecessor_account_id();
        require!(self.collections.contains(&nft), "Collection not traded here");
        require!(self.open, "The market is not open yet");
        require!(env::signer_account_id() == owner_id, "Only the holder can list");
        let parsed: ApproveMsg = serde_json::from_str(&msg).unwrap_or_else(|_| env::panic_str("Bad msg"));
        match parsed {
            ApproveMsg::List { price } => {
                require!(price.0 > 0, "Price must be above zero");
                let k = key(&nft, &token_id);
                let fresh = self.listings.get(&k).map(|l| l.owner_id != owner_id).unwrap_or(true);
                if let Some(old) = self.listings.get(&k) {
                    self.drop_listing(&old);
                }
                if fresh {
                    let need = self.held(&owner_id) + LISTING_STORAGE.as_yoctonear();
                    require!(
                        self.storage.get(&owner_id).unwrap_or(0) >= need,
                        "Add storage first: 0.01 NEAR per listing (storage_deposit)"
                    );
                }
                let l = Listing {
                    nft_contract_id: nft.clone(),
                    token_id: token_id.clone(),
                    owner_id: owner_id.clone(),
                    approval_id,
                    price,
                    listed_at: env::block_timestamp_ms(),
                };
                self.put_listing(&l);
                emit("list", json!({"nft_contract_id": nft, "token_id": token_id, "owner_id": owner_id, "price": price}));
                PromiseOrValue::Value(())
            }
            ApproveMsg::AcceptOffer { accept_offer } => {
                let ok = offer_key(&nft, &token_id, &accept_offer);
                let offer = self.offers.get(&ok).unwrap_or_else(|| env::panic_str("No such offer"));
                self.remove_offer(&offer);
                if let Some(l) = self.listings.get(&key(&nft, &token_id)) {
                    self.drop_listing(&l);
                }
                emit("offer_accept", json!({"nft_contract_id": nft, "token_id": token_id, "owner_id": owner_id,
                    "buyer_id": offer.buyer_id, "amount": offer.amount}));
                PromiseOrValue::Promise(self.settle(nft, token_id, approval_id, owner_id, offer.buyer_id, offer.amount.0, OFFER_STORAGE.as_yoctonear()))
            }
        }
    }

    /// Change the price of your listing.
    #[payable]
    pub fn update_price(&mut self, nft_contract_id: AccountId, token_id: String, price: U128) {
        assert_one_yocto();
        require!(price.0 > 0, "Price must be above zero");
        let k = key(&nft_contract_id, &token_id);
        let mut l = self.listings.get(&k).unwrap_or_else(|| env::panic_str("Not listed"));
        require!(l.owner_id == env::predecessor_account_id(), "Not your listing");
        l.price = price;
        self.listings.insert(&k, &l);
        emit("update_price", json!({"nft_contract_id": nft_contract_id, "token_id": token_id, "price": price}));
    }

    /// Take your listing down. Revoke the approval on the collection as well.
    #[payable]
    pub fn remove_listing(&mut self, nft_contract_id: AccountId, token_id: String) {
        assert_one_yocto();
        let l = self.listings.get(&key(&nft_contract_id, &token_id)).unwrap_or_else(|| env::panic_str("Not listed"));
        require!(l.owner_id == env::predecessor_account_id() || env::predecessor_account_id() == self.owner, "Not your listing");
        self.drop_listing(&l);
        emit("delist", json!({"nft_contract_id": nft_contract_id, "token_id": token_id}));
    }

    // ------------------------------------------------------------ buy

    /// Buy a listed file. Attach at least the price; anything above is refunded.
    #[payable]
    pub fn buy(&mut self, nft_contract_id: AccountId, token_id: String) -> Promise {
        require!(self.open, "The market is not open yet");
        let buyer = env::predecessor_account_id();
        let l = self.listings.get(&key(&nft_contract_id, &token_id)).unwrap_or_else(|| env::panic_str("Not listed"));
        require!(l.owner_id != buyer, "You already hold this file");
        let deposit = env::attached_deposit().as_yoctonear();
        require!(deposit >= l.price.0, "Attach at least the listing price");
        self.drop_listing(&l);
        if deposit > l.price.0 {
            Promise::new(buyer.clone()).transfer(NearToken::from_yoctonear(deposit - l.price.0)).detach();
        }
        emit("buy", json!({"nft_contract_id": nft_contract_id, "token_id": token_id, "buyer_id": buyer, "price": l.price}));
        self.settle(nft_contract_id, token_id, l.approval_id, l.owner_id, buyer, l.price.0, 0)
    }

    /// Transfer via NEP-199 and pay out in the callback.
    fn settle(&self, nft: AccountId, token_id: String, approval_id: u64, seller: AccountId, buyer: AccountId, price: u128, extra_refund: u128) -> Promise {
        let fee = price / 10_000 * self.fee_bps as u128;
        let args = json!({
            "receiver_id": buyer, "token_id": token_id, "approval_id": approval_id,
            "memo": "NEARMISS market", "balance": U128(price - fee), "max_len_payout": 10,
        });
        Promise::new(nft.clone())
            .function_call("nft_transfer_payout".to_string(), args.to_string().into_bytes(), NearToken::from_yoctonear(1), GAS_TRANSFER)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_RESOLVE).resolve_sale(
                nft, token_id, seller, buyer, U128(price), U128(extra_refund),
            ))
    }

    #[private]
    pub fn resolve_sale(
        &mut self,
        nft_contract_id: AccountId,
        token_id: String,
        seller: AccountId,
        buyer: AccountId,
        price: U128,
        extra_refund: U128,
        #[callback_result] result: Result<Payout, PromiseError>,
    ) -> bool {
        let price = price.0;
        let fee = price / 10_000 * self.fee_bps as u128;
        let net = price - fee;
        match result {
            Ok(p) => {
                let total: u128 = p.payout.values().map(|v| v.0).sum();
                if total > net || p.payout.len() > 10 {
                    // a collection returning a broken payout gets paid as a plain sale
                    Promise::new(seller.clone()).transfer(NearToken::from_yoctonear(net)).detach();
                } else {
                    for (acct, amt) in p.payout {
                        if amt.0 > 0 {
                            Promise::new(acct).transfer(NearToken::from_yoctonear(amt.0)).detach();
                        }
                    }
                    if net > total {
                        Promise::new(seller.clone()).transfer(NearToken::from_yoctonear(net - total)).detach();
                    }
                }
                if fee > 0 {
                    Promise::new(self.treasury.clone()).transfer(NearToken::from_yoctonear(fee)).detach();
                }
                if extra_refund.0 > 0 {
                    Promise::new(buyer.clone()).transfer(NearToken::from_yoctonear(extra_refund.0)).detach();
                }
                emit("sale", json!({"nft_contract_id": nft_contract_id, "token_id": token_id, "seller_id": seller,
                    "buyer_id": buyer, "price": U128(price), "fee": U128(fee)}));
                true
            }
            Err(_) => {
                Promise::new(buyer.clone())
                    .transfer(NearToken::from_yoctonear(price + extra_refund.0))
                    .detach();
                emit("sale_failed", json!({"nft_contract_id": nft_contract_id, "token_id": token_id, "buyer_id": buyer, "refunded": U128(price + extra_refund.0)}));
                false
            }
        }
    }

    // ------------------------------------------------------------ offers

    /// Offer on any file in an approved collection. Attach the amount plus 0.01 NEAR
    /// storage; both come back if you withdraw or the offer is not taken.
    #[payable]
    pub fn make_offer(&mut self, nft_contract_id: AccountId, token_id: String) -> U128 {
        require!(self.open, "The market is not open yet");
        require!(self.collections.contains(&nft_contract_id), "Collection not traded here");
        let buyer = env::predecessor_account_id();
        let deposit = env::attached_deposit().as_yoctonear();
        require!(deposit > OFFER_STORAGE.as_yoctonear(), "Attach the offer plus 0.01 NEAR storage");
        let amount = deposit - OFFER_STORAGE.as_yoctonear();
        let ok = offer_key(&nft_contract_id, &token_id, &buyer);
        if let Some(old) = self.offers.get(&ok) {
            self.remove_offer(&old);
            Promise::new(buyer.clone())
                .transfer(NearToken::from_yoctonear(old.amount.0 + OFFER_STORAGE.as_yoctonear()))
                .detach();
        }
        let o = Offer {
            nft_contract_id: nft_contract_id.clone(),
            token_id: token_id.clone(),
            buyer_id: buyer.clone(),
            amount: U128(amount),
            made_at: env::block_timestamp_ms(),
        };
        self.offers.insert(&ok, &o);
        let tk = key(&nft_contract_id, &token_id);
        let mut set = self.offers_by_token.get(&tk).unwrap_or_else(|| {
            UnorderedSet::new(Key::OffersByTokenInner { hash: env::sha256(tk.as_bytes()) })
        });
        set.insert(&buyer);
        self.offers_by_token.insert(&tk, &set);
        emit("offer", json!({"nft_contract_id": nft_contract_id, "token_id": token_id, "buyer_id": buyer, "amount": U128(amount)}));
        U128(amount)
    }

    #[payable]
    pub fn withdraw_offer(&mut self, nft_contract_id: AccountId, token_id: String) {
        assert_one_yocto();
        let buyer = env::predecessor_account_id();
        let o = self.offers.get(&offer_key(&nft_contract_id, &token_id, &buyer)).unwrap_or_else(|| env::panic_str("No offer"));
        self.remove_offer(&o);
        Promise::new(buyer.clone())
            .transfer(NearToken::from_yoctonear(o.amount.0 + OFFER_STORAGE.as_yoctonear()))
            .detach();
        emit("offer_withdraw", json!({"nft_contract_id": nft_contract_id, "token_id": token_id, "buyer_id": buyer}));
    }

    // ------------------------------------------------------------ internals

    fn put_listing(&mut self, l: &Listing) {
        let k = key(&l.nft_contract_id, &l.token_id);
        self.listings.insert(&k, l);
        let mut set = self.by_collection.get(&l.nft_contract_id).unwrap_or_else(|| {
            UnorderedSet::new(Key::ByCollectionInner { hash: env::sha256(l.nft_contract_id.as_bytes()) })
        });
        set.insert(&k);
        self.by_collection.insert(&l.nft_contract_id, &set);
        let n = self.listings_by_seller.get(&l.owner_id).unwrap_or(0);
        self.listings_by_seller.insert(&l.owner_id, &(n + 1));
    }

    fn drop_listing(&mut self, l: &Listing) {
        let k = key(&l.nft_contract_id, &l.token_id);
        self.listings.remove(&k);
        if let Some(mut set) = self.by_collection.get(&l.nft_contract_id) {
            set.remove(&k);
            self.by_collection.insert(&l.nft_contract_id, &set);
        }
        let n = self.listings_by_seller.get(&l.owner_id).unwrap_or(0);
        self.listings_by_seller.insert(&l.owner_id, &n.saturating_sub(1));
    }

    fn remove_offer(&mut self, o: &Offer) {
        self.offers.remove(&offer_key(&o.nft_contract_id, &o.token_id, &o.buyer_id));
        let tk = key(&o.nft_contract_id, &o.token_id);
        if let Some(mut set) = self.offers_by_token.get(&tk) {
            set.remove(&o.buyer_id);
            self.offers_by_token.insert(&tk, &set);
        }
    }

    // ------------------------------------------------------------ views

    pub fn info(&self) -> Info {
        Info {
            owner: self.owner.clone(),
            treasury: self.treasury.clone(),
            fee_bps: self.fee_bps,
            open: self.open,
            collections: self.collections.to_vec(),
            listing_storage: U128(LISTING_STORAGE.as_yoctonear()),
            offer_storage: U128(OFFER_STORAGE.as_yoctonear()),
        }
    }

    pub fn get_listing(&self, nft_contract_id: AccountId, token_id: String) -> Option<Listing> {
        self.listings.get(&key(&nft_contract_id, &token_id))
    }

    pub fn get_listings(&self, nft_contract_id: AccountId, from_index: Option<u64>, limit: Option<u64>) -> Vec<Listing> {
        let Some(set) = self.by_collection.get(&nft_contract_id) else { return vec![] };
        set.iter()
            .skip(from_index.unwrap_or(0) as usize)
            .take(limit.unwrap_or(100).min(500) as usize)
            .filter_map(|k| self.listings.get(&k))
            .collect()
    }

    pub fn listings_count(&self, nft_contract_id: AccountId) -> u64 {
        self.by_collection.get(&nft_contract_id).map(|s| s.len()).unwrap_or(0)
    }

    pub fn get_offers(&self, nft_contract_id: AccountId, token_id: String) -> Vec<Offer> {
        let Some(set) = self.offers_by_token.get(&key(&nft_contract_id, &token_id)) else { return vec![] };
        set.iter().filter_map(|b| self.offers.get(&offer_key(&nft_contract_id, &token_id, &b))).collect()
    }

    // ------------------------------------------------------------ owner

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Owner only");
    }

    pub fn set_open(&mut self, open: bool) {
        self.assert_owner();
        self.open = open;
    }

    pub fn add_collection(&mut self, nft_contract_id: AccountId) {
        self.assert_owner();
        self.collections.insert(&nft_contract_id);
    }

    pub fn remove_collection(&mut self, nft_contract_id: AccountId) {
        self.assert_owner();
        self.collections.remove(&nft_contract_id);
    }

    pub fn set_fee(&mut self, fee_bps: u16) {
        self.assert_owner();
        require!(fee_bps <= MAX_FEE_BPS, "Fee above 10%");
        self.fee_bps = fee_bps;
    }

    pub fn set_treasury(&mut self, treasury: AccountId) {
        self.assert_owner();
        self.treasury = treasury;
    }

    pub fn set_owner(&mut self, owner: AccountId) {
        self.assert_owner();
        self.owner = owner;
    }
}
