//! NEARMISS launchpad.
//!
//! Anyone can open a collection once the launchpad is open. Two steps:
//!
//! 1. `create_collection` (the creator) reserves `<slug>.<this account>` and
//!    holds the deposit.
//! 2. `deploy` (anyone; the NEARMISS service does it within seconds) passes the
//!    collection wasm. The launchpad keeps only its SHA-256, so the bytes must
//!    match exactly. It creates the account, deploys and initialises it in one
//!    batch. If any step fails the creator gets the whole deposit back.
//!
//! Until step 2 runs, the creator can `cancel_request` for a full refund.
//! The deposit pays the collection's own storage, which stays locked in the
//! creator's collection account.
//!
//! The registry keeps what the site shows: name, supply, price, description
//! and the creator's links (X, Telegram, Discord, website).
use near_sdk::collections::{LookupMap, UnorderedMap};
use near_sdk::json_types::U128;
use near_sdk::serde_json::{self, json};
use near_sdk::{env, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise, PromiseError};

const CODE_HASH_KEY: &[u8] = b"CODE_HASH";
const CODE_LEN_KEY: &[u8] = b"CODE_LEN";
/// Headroom above the code size for the collection's initial state.
const STATE_MARGIN: NearToken = NearToken::from_millinear(100);
/// Kept by the launchpad to store the registry entry.
const REGISTRY_FEE: NearToken = NearToken::from_millinear(30);
const GAS_INIT: Gas = Gas::from_tgas(60);
const GAS_CALLBACK: Gas = Gas::from_tgas(20);
const MAX_PLATFORM_FEE_BPS: u16 = 250;

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Collections,
    ByCreator,
    Pending,
}

#[near(serializers = [json, borsh])]
#[derive(Clone, Default)]
pub struct Links {
    pub x: Option<String>,
    pub telegram: Option<String>,
    pub discord: Option<String>,
    pub website: Option<String>,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Profile {
    pub description: String,
    /// Cover image path relative to the base URI, e.g. "<cid>/cover.png".
    pub cover: Option<String>,
    pub links: Links,
}

/// Mirrors the collection contract's Config (sent through as JSON).
#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct CollectionConfig {
    pub name: String,
    pub symbol: String,
    pub total: u32,
    pub price: U128,
    pub max_per_tx: u8,
    pub start_ms: Option<u64>,
    pub sequential: bool,
    pub media_ext: String,
    pub media_cid: String,
    pub refs_cid: String,
    pub royalty_bps: u16,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Collection {
    pub slug: String,
    pub contract_id: AccountId,
    pub creator: AccountId,
    pub name: String,
    pub symbol: String,
    pub total: u32,
    pub price: U128,
    pub royalty_bps: u16,
    pub created_ms: u64,
    pub profile: Profile,
    pub hidden: bool,
}

/// A reserved collection waiting for `deploy`.
#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Request {
    pub entry: Collection,
    pub init: String,
    pub deposit: U128,
    pub in_flight: bool,
}

#[near(serializers = [json])]
pub struct Info {
    pub owner: AccountId,
    pub platform_treasury: AccountId,
    pub platform_fee_bps: u16,
    pub open: bool,
    pub base_uri: String,
    pub code_size: u64,
    pub code_hash: String,
    pub required_deposit: U128,
    pub collections: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Launchpad {
    owner: AccountId,
    platform_treasury: AccountId,
    platform_fee_bps: u16,
    open: bool,
    base_uri: String,
    collections: UnorderedMap<String, Collection>,
    by_creator: LookupMap<AccountId, Vec<String>>,
    pending: UnorderedMap<String, Request>,
}

fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=32).contains(&b.len())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && !s.contains("--")
}

fn check_link(l: &Option<String>) {
    if let Some(u) = l {
        require!(u.len() <= 200 && u.starts_with("https://"), "Links must start with https:// and be under 200 characters");
    }
}

fn check_profile(p: &Profile) {
    require!(p.description.len() <= 1000, "Description must be under 1,000 characters");
    check_link(&p.links.x);
    check_link(&p.links.telegram);
    check_link(&p.links.discord);
    check_link(&p.links.website);
    if let Some(c) = &p.cover {
        require!(c.len() <= 200, "Cover path too long");
    }
}

#[near]
impl Launchpad {
    #[init]
    pub fn new(owner: AccountId, platform_treasury: AccountId, platform_fee_bps: u16, base_uri: String) -> Self {
        require!(platform_fee_bps <= MAX_PLATFORM_FEE_BPS, "Platform fee above 2.5%");
        Self {
            owner,
            platform_treasury,
            platform_fee_bps,
            open: false,
            base_uri,
            collections: UnorderedMap::new(Key::Collections),
            by_creator: LookupMap::new(Key::ByCreator),
            pending: UnorderedMap::new(Key::Pending),
        }
    }

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Owner only");
    }

    fn code_len(&self) -> u64 {
        env::storage_read(CODE_LEN_KEY).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
    }

    fn code_hash(&self) -> Vec<u8> {
        env::storage_read(CODE_HASH_KEY).unwrap_or_default()
    }

    /// Deposit a creator attaches to `create_collection`.
    pub fn required_deposit(&self) -> U128 {
        let code = env::storage_byte_cost().saturating_mul(self.code_len() as u128);
        U128(code.saturating_add(STATE_MARGIN).saturating_add(REGISTRY_FEE).as_yoctonear())
    }

    // ------------------------------------------------------------------ create

    #[payable]
    pub fn create_collection(&mut self, slug: String, config: CollectionConfig, profile: Profile, open: bool) {
        let creator = env::predecessor_account_id();
        require!(self.open || creator == self.owner, "The launchpad is not open yet");
        require!(valid_slug(&slug), "Name in the address: 2 to 32 characters, a-z, 0-9 and single dashes");
        require!(self.collections.get(&slug).is_none() && self.pending.get(&slug).is_none(), "That address is taken");
        check_profile(&profile);
        require!(self.code_len() > 0, "Collection code not set");
        let deposit = env::attached_deposit();
        let need = self.required_deposit().0;
        require!(deposit.as_yoctonear() >= need, format!("Attach at least {} yoctoNEAR", need));

        let contract_id: AccountId = format!("{}.{}", slug, env::current_account_id()).parse().unwrap();
        let init = json!({
            "owner": creator,
            "treasury": creator,
            "platform_treasury": self.platform_treasury,
            "platform_fee_bps": self.platform_fee_bps,
            "base_uri": self.base_uri,
            "config": config,
            "open": open,
        });
        let entry = Collection {
            slug: slug.clone(),
            contract_id,
            creator: creator.clone(),
            name: config.name.clone(),
            symbol: config.symbol.clone(),
            total: config.total,
            price: config.price,
            royalty_bps: config.royalty_bps,
            created_ms: env::block_timestamp_ms(),
            profile,
            hidden: false,
        };
        self.pending.insert(&slug, &Request { entry, init: init.to_string(), deposit: U128(deposit.as_yoctonear()), in_flight: false });
        env::log_str(&format!("EVENT_JSON:{}", json!({"standard": "nearmiss_launchpad", "version": "1.0.0",
            "event": "collection_requested", "data": [{"slug": slug, "creator": creator}]})));
    }

    /// Step 2: deploy a reserved collection. Anyone may call it; the wasm must
    /// hash to the stored code hash. Arguments are Borsh: (slug, wasm bytes).
    pub fn deploy(&mut self, #[serializer(borsh)] slug: String, #[serializer(borsh)] code: Vec<u8>) -> Promise {
        let mut req = self.pending.get(&slug).unwrap_or_else(|| env::panic_str("No request for that address"));
        require!(!req.in_flight, "Already deploying");
        require!(env::sha256(&code) == self.code_hash(), "Code does not match the launchpad's collection code");
        require!(env::prepaid_gas().as_tgas() >= 150, "Attach at least 150 TGas");
        req.in_flight = true;
        self.pending.insert(&slug, &req);
        let deposit = NearToken::from_yoctonear(req.deposit.0);
        Promise::new(req.entry.contract_id.clone())
            .create_account()
            .transfer(deposit.saturating_sub(REGISTRY_FEE))
            .deploy_contract(code)
            .function_call("new".to_string(), req.init.into_bytes(), NearToken::from_yoctonear(0), GAS_INIT)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_created(req.entry, req.deposit))
    }

    /// Before `deploy` runs, the creator can take the request back with a full refund.
    pub fn cancel_request(&mut self, slug: String) -> Promise {
        let req = self.pending.get(&slug).unwrap_or_else(|| env::panic_str("No request for that address"));
        require!(env::predecessor_account_id() == req.entry.creator, "Creator only");
        require!(!req.in_flight, "Already deploying");
        self.pending.remove(&slug);
        Promise::new(req.entry.creator).transfer(NearToken::from_yoctonear(req.deposit.0))
    }

    #[private]
    pub fn on_created(&mut self, entry: Collection, deposit: U128, #[callback_result] r: Result<(), PromiseError>) -> bool {
        self.pending.remove(&entry.slug);
        if r.is_err() {
            Promise::new(entry.creator.clone()).transfer(NearToken::from_yoctonear(deposit.0)).detach();
            env::log_str(&format!("EVENT_JSON:{}", json!({"standard": "nearmiss_launchpad", "version": "1.0.0",
                "event": "create_failed", "data": [{"slug": entry.slug, "creator": entry.creator, "refunded": deposit}]})));
            return false;
        }
        let mut mine = self.by_creator.get(&entry.creator).unwrap_or_default();
        mine.push(entry.slug.clone());
        self.by_creator.insert(&entry.creator, &mine);
        env::log_str(&format!("EVENT_JSON:{}", json!({"standard": "nearmiss_launchpad", "version": "1.0.0",
            "event": "collection_created", "data": [{"slug": entry.slug, "contract_id": entry.contract_id, "creator": entry.creator}]})));
        self.collections.insert(&entry.slug, &entry);
        true
    }

    /// Creators keep their description, cover and links up to date.
    pub fn update_profile(&mut self, slug: String, profile: Profile) {
        let mut c = self.collections.get(&slug).unwrap_or_else(|| env::panic_str("No such collection"));
        require!(env::predecessor_account_id() == c.creator, "Creator only");
        check_profile(&profile);
        c.profile = profile;
        self.collections.insert(&slug, &c);
    }

    // ------------------------------------------------------------------ views

    pub fn info(&self) -> Info {
        Info {
            owner: self.owner.clone(),
            platform_treasury: self.platform_treasury.clone(),
            platform_fee_bps: self.platform_fee_bps,
            open: self.open,
            base_uri: self.base_uri.clone(),
            code_size: self.code_len(),
            code_hash: near_sdk::bs58::encode(self.code_hash()).into_string(),
            required_deposit: self.required_deposit(),
            collections: self.collections.len(),
        }
    }

    pub fn get_collection(&self, slug: String) -> Option<Collection> {
        self.collections.get(&slug)
    }

    /// Newest last. Hidden collections are left out unless `include_hidden`.
    pub fn list_collections(&self, from_index: Option<u64>, limit: Option<u64>, include_hidden: Option<bool>) -> Vec<Collection> {
        let all = include_hidden.unwrap_or(false);
        self.collections
            .values()
            .filter(|c| all || !c.hidden)
            .skip(from_index.unwrap_or(0) as usize)
            .take(limit.unwrap_or(50).min(200) as usize)
            .collect()
    }

    pub fn collections_of(&self, creator: AccountId) -> Vec<Collection> {
        self.by_creator.get(&creator).unwrap_or_default().iter().filter_map(|s| self.collections.get(s)).collect()
    }

    /// Reserved collections waiting for `deploy`.
    pub fn list_pending(&self, from_index: Option<u64>, limit: Option<u64>) -> Vec<Request> {
        self.pending.values().skip(from_index.unwrap_or(0) as usize).take(limit.unwrap_or(50).min(200) as usize).collect()
    }

    pub fn get_pending(&self, slug: String) -> Option<Request> {
        self.pending.get(&slug)
    }

    pub fn slug_available(&self, slug: String) -> bool {
        valid_slug(&slug) && self.collections.get(&slug).is_none() && self.pending.get(&slug).is_none()
    }

    // ------------------------------------------------------------------ owner

    /// Set the collection code. Call with the raw wasm bytes as the argument;
    /// only its SHA-256 and length are stored.
    pub fn set_code(&mut self) {
        self.assert_owner();
        let code = env::input().unwrap_or_else(|| env::panic_str("Pass the wasm bytes"));
        require!(code.len() > 1000 && &code[..4] == b"\0asm", "Not a wasm file");
        env::storage_write(CODE_HASH_KEY, &env::sha256(&code));
        env::storage_write(CODE_LEN_KEY, &(code.len() as u64).to_le_bytes());
    }

    pub fn set_open(&mut self, open: bool) {
        self.assert_owner();
        self.open = open;
    }

    /// Moderation: hide a collection from the NEARMISS site. Its contract is untouched.
    pub fn set_hidden(&mut self, slug: String, hidden: bool) {
        self.assert_owner();
        let mut c = self.collections.get(&slug).unwrap_or_else(|| env::panic_str("No such collection"));
        c.hidden = hidden;
        self.collections.insert(&slug, &c);
    }

    pub fn set_platform_treasury(&mut self, platform_treasury: AccountId) {
        self.assert_owner();
        self.platform_treasury = platform_treasury;
    }

    pub fn set_owner(&mut self, owner: AccountId) {
        self.assert_owner();
        self.owner = owner;
    }
}

#[cfg(test)]
mod tests {
    use super::valid_slug;

    #[test]
    fn slugs() {
        for ok in ["ab", "cool-cats", "a1", "night-shift-99"] {
            assert!(valid_slug(ok), "{ok}");
        }
        for bad in ["a", "-ab", "ab-", "a--b", "Cool", "a_b", "a.b", &"x".repeat(33)] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }
}
