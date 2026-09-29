//! NEARMISS — 3,333 incident files, drawn at random on mint.
//!
//! Every file (1..=3333) sits in one pool. `nft_mint_random` picks from the
//! files still in the pool using the receipt's random seed and removes the
//! pick (swap-and-pop over a sparse map), so each file is issued exactly once
//! and nobody chooses which file a wallet receives.
//!
//! Standards: NEP-171 core, NEP-177 metadata, NEP-178 approvals, NEP-181
//! enumeration, NEP-199 payouts, NEP-297 events.
use std::collections::HashMap;

use near_contract_standards::non_fungible_token::approval::NonFungibleTokenApproval;
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
    assert_one_yocto, env, near, require, AccountId, BorshStorageKey, NearToken, PanicOnDefault, Promise,
    PromiseOrValue,
};

pub const TOTAL: u32 = 3333;
pub const MAX_PER_TX: u8 = 5;
const MAX_ROYALTY_BPS: u16 = 1000; // 10%

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Owner,
    Metadata,
    TokenMetadata,
    Enumeration,
    Approval,
    ContractMetadata,
    Swaps,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Contract {
    tokens: NonFungibleToken,
    metadata: LazyOption<NFTContractMetadata>,
    owner: AccountId,
    treasury: AccountId,
    price: NearToken,
    open: bool,
    minted: u32,
    /// Sparse Fisher–Yates state: virtual pool slot -> file index (0-based).
    /// A slot with no entry still holds its own index.
    swaps: LookupMap<u32, u32>,
    /// IPFS folders. `media` = images, `refs` = metadata JSON. Relative to base_uri.
    media_cid: String,
    refs_cid: String,
    frozen: bool,
    royalty_bps: u16,
}

#[near(serializers = [json])]
pub struct Info {
    pub price_yocto: U128,
    pub minted: u32,
    pub total: u32,
    pub remaining: u32,
    pub open: bool,
    pub max_per_tx: u8,
    pub treasury: AccountId,
    pub royalty_bps: u16,
    pub frozen: bool,
    pub media_cid: String,
    pub refs_cid: String,
}

#[near(serializers = [json])]
#[derive(Debug, PartialEq)]
pub struct Payout {
    pub payout: HashMap<AccountId, U128>,
}

#[near]
impl Contract {
    #[init]
    pub fn new(
        owner: AccountId,
        treasury: AccountId,
        price: U128,
        base_uri: String,
        reference: Option<String>,
        reference_hash: Option<near_sdk::json_types::Base64VecU8>,
        royalty_bps: u16,
    ) -> Self {
        require!(royalty_bps <= MAX_ROYALTY_BPS, "Royalty above 10%");
        let meta = NFTContractMetadata {
            spec: NFT_METADATA_SPEC.to_string(),
            name: "NEARMISS".to_string(),
            symbol: "NMIB".to_string(),
            icon: None,
            base_uri: Some(base_uri),
            reference,
            reference_hash,
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
            owner,
            treasury,
            price: NearToken::from_yoctonear(price.0),
            open: false,
            minted: 0,
            swaps: LookupMap::new(Key::Swaps),
            media_cid: String::new(),
            refs_cid: String::new(),
            frozen: false,
            royalty_bps,
        }
    }

    // ------------------------------------------------------------------ mint

    /// Request `count` files (1..=5). Attach at least `price * count`.
    /// Storage is paid out of the price; the rest goes to the treasury. If
    /// fewer files are left than requested, the remaining files are issued and
    /// the price of the missing ones is refunded, along with any overpayment.
    /// A failed call (archive closed, sold out, underpaid) is refunded in full
    /// by the protocol.
    #[payable]
    pub fn nft_mint_random(&mut self, count: Option<u8>) -> Vec<TokenId> {
        let requested = count.unwrap_or(1);
        require!(self.open, "The archive is closed");
        require!(!self.media_cid.is_empty() && !self.refs_cid.is_empty(), "Media not set");
        require!((1..=MAX_PER_TX).contains(&requested), "Request 1 to 5 files");
        let remaining = TOTAL - self.minted;
        require!(remaining > 0, "Every file has been issued");
        let deposit = env::attached_deposit();
        require!(
            deposit >= self.price.saturating_mul(requested as u128),
            format!("Attach at least {} yoctoNEAR", self.price.saturating_mul(requested as u128).as_yoctonear())
        );
        // In a rush the archive may hold fewer files than requested: issue what
        // is left and refund the rest below.
        let count: u8 = if remaining < requested as u32 { remaining as u8 } else { requested };
        let cost = self.price.saturating_mul(count as u128);

        let buyer = env::predecessor_account_id();
        let storage_before = env::storage_usage();
        let seed = env::random_seed();
        let mut ids: Vec<TokenId> = Vec::with_capacity(count as usize);
        for k in 0..count {
            let file = self.draw(&seed, k) + 1;
            let token_id = file.to_string();
            let metadata = self.token_metadata(file);
            self.tokens.internal_mint_with_refund(token_id.clone(), buyer.clone(), Some(metadata), None);
            ids.push(token_id);
        }
        let used = env::storage_usage() - storage_before;
        let storage_cost = env::storage_byte_cost().saturating_mul(used as u128);
        require!(cost >= storage_cost, "Price does not cover storage");

        NftMint { owner_id: &buyer, token_ids: &ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(), memo: None }
            .emit();

        let proceeds = cost.saturating_sub(storage_cost);
        if !proceeds.is_zero() {
            Promise::new(self.treasury.clone()).transfer(proceeds).detach();
        }
        let refund = deposit.saturating_sub(cost);
        if !refund.is_zero() {
            Promise::new(buyer).transfer(refund).detach();
        }
        ids
    }

    /// Pick one file index (0-based) from the pool and remove it.
    fn draw(&mut self, seed: &[u8], k: u8) -> u32 {
        let n = TOTAL - self.minted;
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

    fn token_metadata(&self, file: u32) -> TokenMetadata {
        TokenMetadata {
            title: Some(format!("NEARMISS #{:04}", file)),
            description: None,
            media: Some(format!("{}/{:04}.png", self.media_cid, file)),
            media_hash: None,
            copies: Some(1),
            issued_at: Some(env::block_timestamp_ms().to_string()),
            expires_at: None,
            starts_at: None,
            updated_at: None,
            extra: None,
            reference: Some(format!("{}/{:04}.json", self.refs_cid, file)),
            reference_hash: None,
        }
    }

    // ------------------------------------------------------------------ views

    pub fn nm_info(&self) -> Info {
        Info {
            price_yocto: U128(self.price.as_yoctonear()),
            minted: self.minted,
            total: TOTAL,
            remaining: TOTAL - self.minted,
            open: self.open,
            max_per_tx: MAX_PER_TX,
            treasury: self.treasury.clone(),
            royalty_bps: self.royalty_bps,
            frozen: self.frozen,
            media_cid: self.media_cid.clone(),
            refs_cid: self.refs_cid.clone(),
        }
    }

    // ------------------------------------------------------------------ owner

    fn assert_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Owner only");
    }

    pub fn set_open(&mut self, open: bool) {
        self.assert_owner();
        if open {
            require!(!self.media_cid.is_empty() && !self.refs_cid.is_empty(), "Media not set");
        }
        self.open = open;
    }

    pub fn set_price(&mut self, price: U128) {
        self.assert_owner();
        self.price = NearToken::from_yoctonear(price.0);
    }

    pub fn set_treasury(&mut self, treasury: AccountId) {
        self.assert_owner();
        self.treasury = treasury;
    }

    pub fn set_royalty(&mut self, royalty_bps: u16) {
        self.assert_owner();
        require!(royalty_bps <= MAX_ROYALTY_BPS, "Royalty above 10%");
        self.royalty_bps = royalty_bps;
    }

    /// Point at the IPFS folders. Allowed until `freeze_media` is called.
    pub fn set_media(&mut self, media_cid: String, refs_cid: String, base_uri: Option<String>) {
        self.assert_owner();
        require!(!self.frozen, "Media is frozen");
        self.media_cid = media_cid;
        self.refs_cid = refs_cid;
        if let Some(b) = base_uri {
            let mut m = self.metadata.get().unwrap();
            m.base_uri = Some(b);
            self.metadata.set(&m);
        }
    }

    /// One-way: after this, media and metadata locations can never change.
    pub fn freeze_media(&mut self) {
        self.assert_owner();
        require!(!self.media_cid.is_empty() && !self.refs_cid.is_empty(), "Media not set");
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
        let royalty = balance / 10_000 * self.royalty_bps as u128;
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
        self.tokens.nft_approve(token_id, account_id, msg)
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
