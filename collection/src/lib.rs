//! NEARMISS launchpad collection.
//!
//! One contract per creator collection, created by the NEARMISS launchpad.
//! Supply, price, schedule and draw mode are set by the creator. Every mint
//! pays the platform fee (fixed at creation, 2.5%) to the platform treasury
//! and the rest, after token storage, to the creator. Secondary sales pay the
//! creator's royalty (at most 7.5%) through NEP-199.
//!
//! Standards: NEP-171, NEP-177, NEP-178, NEP-181, NEP-199, NEP-297.
use std::collections::HashMap;

use near_contract_standards::non_fungible_token::approval::{ext_nft_approval_receiver, NonFungibleTokenApproval};
use near_contract_standards::non_fungible_token::core::{NonFungibleTokenCore, NonFungibleTokenResolver};
use near_contract_standards::non_fungible_token::enumeration::NonFungibleTokenEnumeration;
use near_contract_standards::non_fungible_token::events::NftMint;
use near_contract_standards::non_fungible_token::metadata::{
    NFTContractMetadata, NonFungibleTokenMetadataProvider, TokenMetadata, NFT_METADATA_SPEC,
};
use near_contract_standards::non_fungible_token::{NonFungibleToken, Token, TokenId};
use near_sdk::collections::{LazyOption, LookupMap};
use near_sdk::json_types::U128;
use near_sdk::{
    assert_one_yocto, env, near, require, AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise,
    PromiseOrValue,
};

pub const MAX_SUPPLY: u32 = 10_000;
pub const MAX_ROYALTY_BPS: u16 = 750; // 7.5%
pub const MAX_PLATFORM_FEE_BPS: u16 = 250; // 2.5%
pub const MAX_PER_TX_CAP: u8 = 10;
/// Lowest price per token: it has to cover the ~0.01 NEAR of storage a token uses.
pub const MIN_PRICE: NearToken = NearToken::from_millinear(20);

/// Minimum for the approved account's nft_on_approve; it also gets all unused gas.
const GAS_FOR_ON_APPROVE: Gas = Gas::from_tgas(15);

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Owner,
    TokenMetadata,
    Enumeration,
    Approval,
    ContractMetadata,
    Swaps,
}

#[near(serializers = [json, borsh])]
#[derive(Clone)]
pub struct Config {
    pub name: String,
    pub symbol: String,
    pub total: u32,
    pub price: U128,
    pub max_per_tx: u8,
    /// Mint opens at this time (ms since epoch); None = as soon as `open` is set.
    pub start_ms: Option<u64>,
    /// false = random draw from the whole pool (like NEARMISS), true = 1, 2, 3 …
    pub sequential: bool,
    /// Image file extension inside the media folder: png, jpg, gif, webp.
    pub media_ext: String,
    pub media_cid: String,
    pub refs_cid: String,
    pub royalty_bps: u16,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    tokens: NonFungibleToken,
    metadata: LazyOption<NFTContractMetadata>,
    cfg: Config,
    owner: AccountId,
    /// Where mint proceeds and royalties go.
    treasury: AccountId,
    platform_treasury: AccountId,
    platform_fee_bps: u16,
    open: bool,
    minted: u32,
    swaps: LookupMap<u32, u32>,
    frozen: bool,
}

#[near(serializers = [json])]
pub struct Info {
    pub name: String,
    pub symbol: String,
    pub price_yocto: U128,
    pub minted: u32,
    pub total: u32,
    pub remaining: u32,
    pub open: bool,
    pub started: bool,
    pub start_ms: Option<u64>,
    pub max_per_tx: u8,
    pub sequential: bool,
    pub owner: AccountId,
    pub treasury: AccountId,
    pub royalty_bps: u16,
    pub platform_fee_bps: u16,
    pub frozen: bool,
    pub media_cid: String,
    pub refs_cid: String,
    pub media_ext: String,
}

#[near(serializers = [json])]
#[derive(Debug, PartialEq)]
pub struct Payout {
    pub payout: HashMap<AccountId, U128>,
}

fn check(cfg: &Config) {
    require!(!cfg.name.trim().is_empty() && cfg.name.len() <= 64, "Name must be 1 to 64 characters");
    require!(!cfg.symbol.is_empty() && cfg.symbol.len() <= 12, "Symbol must be 1 to 12 characters");
    require!((1..=MAX_SUPPLY).contains(&cfg.total), "Supply must be 1 to 10,000");
    require!(cfg.price.0 >= MIN_PRICE.as_yoctonear(), "Price must be at least 0.02 NEAR");
    require!((1..=MAX_PER_TX_CAP).contains(&cfg.max_per_tx), "Max per transaction must be 1 to 10");
    require!(cfg.royalty_bps <= MAX_ROYALTY_BPS, "Royalty above 7.5%");
    require!(["png", "jpg", "jpeg", "gif", "webp"].contains(&cfg.media_ext.as_str()), "Unsupported image type");
    require!(!cfg.media_cid.is_empty() && !cfg.refs_cid.is_empty(), "Media not set");
}

#[near]
impl Contract {
    #[init]
    pub fn new(
        owner: AccountId,
        treasury: AccountId,
        platform_treasury: AccountId,
        platform_fee_bps: u16,
        base_uri: String,
        config: Config,
        open: bool,
    ) -> Self {
        require!(platform_fee_bps <= MAX_PLATFORM_FEE_BPS, "Platform fee above 2.5%");
        check(&config);
        let meta = NFTContractMetadata {
            spec: NFT_METADATA_SPEC.to_string(),
            name: config.name.clone(),
            symbol: config.symbol.clone(),
            icon: None,
            base_uri: Some(base_uri),
            reference: None,
            reference_hash: None,
        };
        meta.assert_valid();
        Self {
            tokens: NonFungibleToken::new(
                Key::Owner,
                owner.clone(),
                Some(Key::TokenMetadata),
                Some(Key::Enumeration),
                Some(Key::Approval),
            ),
            metadata: LazyOption::new(Key::ContractMetadata, Some(&meta)),
            cfg: config,
            owner,
            treasury,
            platform_treasury,
            platform_fee_bps,
            open,
            minted: 0,
            swaps: LookupMap::new(Key::Swaps),
            frozen: false,
        }
    }

    // ------------------------------------------------------------------ mint

    fn started(&self) -> bool {
        self.cfg.start_ms.map(|s| env::block_timestamp_ms() >= s).unwrap_or(true)
    }

    /// Mint `count` tokens (1..=max_per_tx). Same name as NEARMISS so one site
    /// flow serves both. Partial fills refund; failed calls refund in full.
    #[payable]
    pub fn nft_mint_random(&mut self, count: Option<u8>) -> Vec<TokenId> {
        let requested = count.unwrap_or(1);
        require!(self.open, "Minting is closed");
        require!(self.started(), "Minting has not started yet");
        require!((1..=self.cfg.max_per_tx).contains(&requested), format!("Request 1 to {} tokens", self.cfg.max_per_tx));
        let remaining = self.cfg.total - self.minted;
        require!(remaining > 0, "Sold out");
        let price = NearToken::from_yoctonear(self.cfg.price.0);
        let deposit = env::attached_deposit();
        require!(
            deposit >= price.saturating_mul(requested as u128),
            format!("Attach at least {} yoctoNEAR", price.saturating_mul(requested as u128).as_yoctonear())
        );
        let count: u8 = if remaining < requested as u32 { remaining as u8 } else { requested };
        let cost = price.saturating_mul(count as u128);

        let buyer = env::predecessor_account_id();
        let storage_before = env::storage_usage();
        let seed = env::random_seed();
        let mut ids: Vec<TokenId> = Vec::with_capacity(count as usize);
        for k in 0..count {
            let n = if self.cfg.sequential { self.next() } else { self.draw(&seed, k) } + 1;
            let token_id = n.to_string();
            let metadata = self.token_metadata(n);
            self.tokens.internal_mint_with_refund(token_id.clone(), buyer.clone(), Some(metadata), None);
            ids.push(token_id);
        }
        let used = env::storage_usage() - storage_before;
        let storage_cost = env::storage_byte_cost().saturating_mul(used as u128);
        require!(cost >= storage_cost, "Price does not cover storage");

        NftMint { owner_id: &buyer, token_ids: &ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(), memo: None }
            .emit();

        // platform fee on the gross, creator gets the rest after storage
        let fee = NearToken::from_yoctonear(cost.as_yoctonear() / 10_000 * self.platform_fee_bps as u128);
        let creator = cost.saturating_sub(storage_cost).saturating_sub(fee);
        if !fee.is_zero() {
            Promise::new(self.platform_treasury.clone()).transfer(fee).detach();
        }
        if !creator.is_zero() {
            Promise::new(self.treasury.clone()).transfer(creator).detach();
        }
        let refund = deposit.saturating_sub(cost);
        if !refund.is_zero() {
            Promise::new(buyer).transfer(refund).detach();
        }
        ids
    }

    fn next(&mut self) -> u32 {
        let n = self.minted;
        self.minted += 1;
        n
    }

    /// Random pick from the remaining pool (swap-and-pop over a sparse map).
    fn draw(&mut self, seed: &[u8], k: u8) -> u32 {
        let n = self.cfg.total - self.minted;
        let mut input = seed.to_vec();
        input.extend_from_slice(&self.minted.to_le_bytes());
        input.push(k);
        let h = env::sha256(&input);
        let r = (u64::from_le_bytes(h[..8].try_into().unwrap()) % n as u64) as u32;
        let last = n - 1;
        let picked = self.swaps.get(&r).unwrap_or(r);
        let tail = self.swaps.get(&last).unwrap_or(last);
        if r != last {
            self.swaps.insert(&r, &tail);
        }
        self.swaps.remove(&last);
        self.minted += 1;
        picked
    }

    fn token_metadata(&self, n: u32) -> TokenMetadata {
        TokenMetadata {
            title: Some(format!("{} #{}", self.cfg.name, n)),
            description: None,
            media: Some(format!("{}/{}.{}", self.cfg.media_cid, n, self.cfg.media_ext)),
            media_hash: None,
            copies: Some(1),
            issued_at: Some(env::block_timestamp_ms().to_string()),
            expires_at: None,
            starts_at: None,
            updated_at: None,
            extra: None,
            reference: Some(format!("{}/{}.json", self.cfg.refs_cid, n)),
            reference_hash: None,
        }
    }

    // ------------------------------------------------------------------ views

    pub fn nm_info(&self) -> Info {
        Info {
            name: self.cfg.name.clone(),
            symbol: self.cfg.symbol.clone(),
            price_yocto: self.cfg.price,
            minted: self.minted,
            total: self.cfg.total,
            remaining: self.cfg.total - self.minted,
            open: self.open,
            started: self.started(),
            start_ms: self.cfg.start_ms,
            max_per_tx: self.cfg.max_per_tx,
            sequential: self.cfg.sequential,
            owner: self.owner.clone(),
            treasury: self.treasury.clone(),
            royalty_bps: self.cfg.royalty_bps,
            platform_fee_bps: self.platform_fee_bps,
            frozen: self.frozen,
            media_cid: self.cfg.media_cid.clone(),
            refs_cid: self.cfg.refs_cid.clone(),
            media_ext: self.cfg.media_ext.clone(),
        }
    }

    // ------------------------------------------------------------------ creator

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Creator only");
    }

    pub fn set_open(&mut self, open: bool) {
        self.assert_owner();
        self.open = open;
    }

    /// Price can change only before the first mint.
    pub fn set_price(&mut self, price: U128) {
        self.assert_owner();
        require!(self.minted == 0, "Price is fixed once minting has begun");
        require!(price.0 >= MIN_PRICE.as_yoctonear(), "Price must be at least 0.02 NEAR");
        self.cfg.price = price;
    }

    pub fn set_start(&mut self, start_ms: Option<u64>) {
        self.assert_owner();
        require!(self.minted == 0, "Start time is fixed once minting has begun");
        self.cfg.start_ms = start_ms;
    }

    pub fn set_treasury(&mut self, treasury: AccountId) {
        self.assert_owner();
        self.treasury = treasury;
    }

    pub fn set_royalty(&mut self, royalty_bps: u16) {
        self.assert_owner();
        require!(royalty_bps <= MAX_ROYALTY_BPS, "Royalty above 7.5%");
        self.cfg.royalty_bps = royalty_bps;
    }

    /// Replace the media folders before minting starts; never after freeze.
    pub fn set_media(&mut self, media_cid: String, refs_cid: String, media_ext: String) {
        self.assert_owner();
        require!(!self.frozen, "Media is frozen");
        require!(self.minted == 0, "Media is fixed once minting has begun");
        self.cfg.media_cid = media_cid;
        self.cfg.refs_cid = refs_cid;
        self.cfg.media_ext = media_ext;
        check(&self.cfg);
    }

    /// One-way: after this, media and metadata locations can never change.
    pub fn freeze_media(&mut self) {
        self.assert_owner();
        self.frozen = true;
    }

    pub fn set_owner(&mut self, owner: AccountId) {
        self.assert_owner();
        self.owner = owner;
    }

    // ------------------------------------------------------------------ NEP-199

    pub fn nft_payout(&self, token_id: String, balance: U128, max_len_payout: Option<u32>) -> Payout {
        let owner = self.tokens.owner_by_id.get(&token_id).unwrap_or_else(|| env::panic_str("Token not found"));
        self.split(owner, balance.0, max_len_payout)
    }

    #[payable]
    pub fn nft_transfer_payout(
        &mut self,
        receiver_id: AccountId,
        token_id: String,
        approval_id: Option<u64>,
        memo: Option<String>,
        balance: U128,
        max_len_payout: Option<u32>,
    ) -> Payout {
        assert_one_yocto();
        let sender = env::predecessor_account_id();
        let (previous_owner, approvals) =
            self.tokens.internal_transfer(&sender, &receiver_id, &token_id, approval_id, memo);
        if let Some(approvals) = approvals {
            refund_approvals(&previous_owner, &approvals);
        }
        self.split(previous_owner, balance.0, max_len_payout)
    }

    fn split(&self, owner: AccountId, balance: u128, max_len_payout: Option<u32>) -> Payout {
        if let Some(max) = max_len_payout {
            require!(max >= 2, "max_len_payout below 2");
        }
        let royalty = balance / 10_000 * self.cfg.royalty_bps as u128;
        let mut payout = HashMap::new();
        if owner == self.treasury {
            payout.insert(owner, U128(balance));
        } else {
            payout.insert(self.treasury.clone(), U128(royalty));
            payout.insert(owner, U128(balance - royalty));
        }
        Payout { payout }
    }
}

/// Return the storage deposit paid for approvals to the previous owner
/// (mirrors near-contract-standards' private helper).
fn refund_approvals(owner: &AccountId, approvals: &HashMap<AccountId, u64>) {
    let bytes: u64 = approvals.keys().map(|a| a.as_str().len() as u64 + 4 + 8).sum();
    if bytes > 0 {
        Promise::new(owner.clone()).transfer(env::storage_byte_cost().saturating_mul(bytes as u128)).detach();
    }
}

// ---------------------------------------------------------------------- NEP-171

#[near]
impl NonFungibleTokenCore for Contract {
    #[payable]
    fn nft_transfer(&mut self, receiver_id: AccountId, token_id: TokenId, approval_id: Option<u64>, memo: Option<String>) {
        self.tokens.nft_transfer(receiver_id, token_id, approval_id, memo)
    }

    #[payable]
    fn nft_transfer_call(
        &mut self,
        receiver_id: AccountId,
        token_id: TokenId,
        approval_id: Option<u64>,
        memo: Option<String>,
        msg: String,
    ) -> PromiseOrValue<bool> {
        self.tokens.nft_transfer_call(receiver_id, token_id, approval_id, memo, msg)
    }

    fn nft_token(&self, token_id: TokenId) -> Option<Token> {
        self.tokens.nft_token(token_id)
    }
}

#[near]
impl NonFungibleTokenResolver for Contract {
    #[private]
    fn nft_resolve_transfer(
        &mut self,
        previous_owner_id: AccountId,
        receiver_id: AccountId,
        token_id: TokenId,
        approved_account_ids: Option<HashMap<AccountId, u64>>,
    ) -> bool {
        self.tokens.nft_resolve_transfer(previous_owner_id, receiver_id, token_id, approved_account_ids)
    }
}

// ---------------------------------------------------------------------- NEP-178

#[near]
impl NonFungibleTokenApproval for Contract {
    #[payable]
    fn nft_approve(&mut self, token_id: TokenId, account_id: AccountId, msg: Option<String>) -> Option<Promise> {
        // The standard call does the bookkeeping and refunds any excess deposit. Its own
        // nft_on_approve call reserves only a fixed 10 TGas margin, which a refund to an
        // implicit account (it may create the account) uses up. So the callback is made
        // here, with a minimum and all remaining gas, sized by the runtime.
        self.tokens.nft_approve(token_id.clone(), account_id.clone(), None);
        let msg = msg?;
        let owner_id = self.tokens.owner_by_id.get(&token_id).unwrap();
        let approval_id = self.tokens.approvals_by_id.as_ref().and_then(|a| a.get(&token_id)).and_then(|m| m.get(&account_id).copied()).unwrap();
        Some(
            ext_nft_approval_receiver::ext(account_id)
                .with_static_gas(GAS_FOR_ON_APPROVE)
                .with_unused_gas_weight(1)
                .nft_on_approve(token_id, owner_id, approval_id, msg),
        )
    }

    #[payable]
    fn nft_revoke(&mut self, token_id: TokenId, account_id: AccountId) {
        self.tokens.nft_revoke(token_id, account_id);
    }

    #[payable]
    fn nft_revoke_all(&mut self, token_id: TokenId) {
        self.tokens.nft_revoke_all(token_id);
    }

    fn nft_is_approved(&self, token_id: TokenId, approved_account_id: AccountId, approval_id: Option<u64>) -> bool {
        self.tokens.nft_is_approved(token_id, approved_account_id, approval_id)
    }
}

// ---------------------------------------------------------------------- NEP-181

#[near]
impl NonFungibleTokenEnumeration for Contract {
    fn nft_total_supply(&self) -> U128 {
        self.tokens.nft_total_supply()
    }

    fn nft_tokens(&self, from_index: Option<U128>, limit: Option<u64>) -> Vec<Token> {
        self.tokens.nft_tokens(from_index, limit)
    }

    fn nft_supply_for_owner(&self, account_id: AccountId) -> U128 {
        self.tokens.nft_supply_for_owner(account_id)
    }

    fn nft_tokens_for_owner(&self, account_id: AccountId, from_index: Option<U128>, limit: Option<u64>) -> Vec<Token> {
        self.tokens.nft_tokens_for_owner(account_id, from_index, limit)
    }
}

// ---------------------------------------------------------------------- NEP-177

#[near]
impl NonFungibleTokenMetadataProvider for Contract {
    fn nft_metadata(&self) -> NFTContractMetadata {
        self.metadata.get().unwrap()
    }
}

#[cfg(test)]
mod tests;
