//! Reflect **whitelabel** routing: the account plumbing + SDK delegation
//! shared by the Jupiter and Titan shims.
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

use reflect_sdk::common::Clock;
use reflect_sdk::proxy::{ids, quote_account_keys_from_state, CoreError, ProxyState, WhitelabelAccounts};
use reflect_sdk::strategy::MintInfo;

use crate::{spl_mint_info, RouteQuote};

/// Which branded-pair op an input mint maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhitelabelRoute {
    /// stablecoin -> branded: share mint at the cranked price.
    Wrap,
    /// branded -> stablecoin: share burn at the cranked price.
    Unwrap,
}

/// One branded market (a `ProxyState` PDA), bound to THE proxy deployment.
/// All protocol logic is `reflect-sdk`'s; the built [`WhitelabelAccounts`] is
/// cached behind an `Arc` once per [`load`](Self::load), so quote/build reuse
/// it and `Clone` stays cheap.
///
/// Load is two-phase: [`seeds`](Self::seeds) -> fetch ->
/// [`discovery_keys`](Self::discovery_keys) -> fetch the full set ->
/// [`load`](Self::load), mirroring `LendingVenue`/`BondVenue`.
#[derive(Clone)]
pub struct WhitelabelVenue {
    market: Pubkey,
    loaded: Option<Arc<WhitelabelAccounts>>,
    mints: Option<[MintInfo; 2]>, // [stablecoin (idx 0), branded (idx 1)]
}

impl WhitelabelVenue {
    /// Construct cold from the `ProxyState` account the aggregator lists.
    pub fn from_market(market: Pubkey) -> Self {
        Self { market, loaded: None, mints: None }
    }

    /// THE proxy deployment.
    pub fn program_id(&self) -> Pubkey {
        ids::PROGRAM
    }

    /// The market/pool key (the `ProxyState` PDA).
    pub fn market(&self) -> Pubkey {
        self.market
    }

    /// One seed: the `ProxyState` itself carries both mints.
    pub fn seeds(&self) -> Vec<Pubkey> {
        vec![self.market]
    }

    /// Given the `ProxyState` bytes, the full fetch set: the 4 quote keys
    /// plus the stablecoin mint account (decimals; extras pass through the
    /// SDK loader harmlessly).
    ///
    /// # Errors
    /// If the bytes are not a valid `ProxyState`.
    pub fn discovery_keys(&self, proxy_state: &[u8]) -> Result<Vec<Pubkey>> {
        let ps = ProxyState::from_bytes(proxy_state).map_err(|e| anyhow!("proxy_state: {e:?}"))?;
        let branded = Pubkey::new_from_array(ps.branded_mint());
        let keys = quote_account_keys_from_state(branded, proxy_state).map_err(|e| anyhow!("keys: {e:?}"))?;
        let mut all = keys.all_keys(); // [proxy_state, oracle, vault, branded_mint]

        all.push(keys.stablecoin_mint);

        Ok(all)
    }

    /// Ingest the fetched `(key, owner, data)` set: verify ownership, build
    /// the SDK loader once, cache it, resolve both mints' decimals.
    ///
    /// # Errors
    /// If the market account is missing, not owned by the proxy program, or
    /// either mint account is missing/short - a proxy market always has both.
    pub fn load(&mut self, accounts: impl IntoIterator<Item = (Pubkey, Pubkey, Vec<u8>)>) -> Result<()> {
        let fetched: Vec<(Pubkey, Pubkey, Vec<u8>)> = accounts.into_iter().collect();
        let (_, owner, state) = fetched.iter().find(|(k, _, _)| *k == self.market).context("market account missing")?;

        if *owner != ids::PROGRAM {
            return Err(anyhow!("market owner {owner} is not the proxy program"));
        }

        let branded = Pubkey::new_from_array(ProxyState::from_bytes(state).map_err(|e| anyhow!("proxy_state: {e:?}"))?.branded_mint());
        let accts = WhitelabelAccounts::from_account_map(branded, fetched.iter().map(|(k, _, d)| (*k, d.clone()))).map_err(|e| anyhow!("from_account_map: {e:?}"))?;
        let stablecoin = accts.stablecoin_mint();

        self.mints = Some([spl_mint_info(stablecoin, &fetched)?, spl_mint_info(branded, &fetched)?]);
        self.loaded = Some(Arc::new(accts));

        Ok(())
    }

    /// The mints this venue trades (empty until loaded): `[stablecoin,
    /// branded]` - deposit-then-receipt order, mirroring lending's
    /// `[deposit, receipt]`.
    pub fn mints(&self) -> Vec<MintInfo> {
        self.mints.as_ref().map_or(vec![], |m| m.to_vec())
    }

    fn loaded(&self) -> Result<&WhitelabelAccounts> {
        self.loaded.as_deref().context("venue not loaded")
    }

    /// Classify the input mint.
    pub fn route(&self, input_mint: Pubkey) -> Result<WhitelabelRoute> {
        let m = self.mints.as_ref().context("venue not loaded")?;

        if input_mint == m[0].pubkey {
            Ok(WhitelabelRoute::Wrap)
        } else if input_mint == m[1].pubkey {
            Ok(WhitelabelRoute::Unwrap)
        } else {
            Err(anyhow!("mint {input_mint} is not in this market's pair"))
        }
    }

    /// Quote `amount` of `input_mint` into its counterpart, ExactIn.
    ///
    /// `None` is "no route": the ONLY predicted revert this product has is
    /// a stale oracle (> 200 slots; there are no caps/floors/pause flags).
    /// `amount == 0` returns `Some(zero)` before touching state - the
    /// documented zero-input contract (`bounds()` binary-searches on it).
    ///
    /// `fee` is ALWAYS 0: the crank fee is folded into pricing, never a
    /// scalar - a wrong-unit fee would mislead a router more than zero does.
    ///
    /// # Errors
    /// If the venue isn't loaded, the mint isn't in the pair, or the SDK
    /// quote call fails for a reason other than a stale oracle.
    pub fn quote(&self, input_mint: Pubkey, amount: u64, clock: Clock) -> Result<Option<RouteQuote>> {
        if amount == 0 {
            return Ok(Some(RouteQuote { amount_out: 0, fee: 0 }));
        }

        match self.loaded()?.quote_by_mints(input_mint, amount, clock) {
            Ok(q) => Ok(Some(RouteQuote { amount_out: q.amount_out, fee: 0 })),
            Err(CoreError::StaleOracle) => Ok(None), // on-chain would revert: no route
            Err(e) => Err(anyhow!("quote: {e:?}")),
        }
    }

    /// Build the wrap/unwrap instruction. `min_out` is baked from
    /// `slippage_bps` (pass 0 for a host that applies its own, e.g. Jupiter
    /// rewrites the instruction data from route context).
    ///
    /// # Errors
    /// If the venue isn't loaded, the mint isn't in the pair, or the SDK's
    /// wrap/unwrap build fails.
    pub fn build(&self, user: Pubkey, input_mint: Pubkey, amount: u64, slippage_bps: u16, clock: Clock) -> Result<Instruction> {
        let h = self.loaded()?.whitelabel(user);

        let (_q, built) = match self.route(input_mint)? {
            WhitelabelRoute::Wrap => h.wrap(amount, slippage_bps, clock).map_err(|e| anyhow!("wrap: {e:?}"))?,
            WhitelabelRoute::Unwrap => h.unwrap(amount, slippage_bps, clock).map_err(|e| anyhow!("unwrap: {e:?}"))?,
        };

        Ok(built.instruction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_sdk::proxy::pda;
    use reflect_sdk::proxy::Oracle;

    fn pk(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    // ProxyState: branded_mint(32) stablecoin_mint(32) fee(2)@64 principal(8)@66 commission(8)@74.
    fn proxy_state_buf(branded: &Pubkey, stablecoin: &Pubkey, fee: u16, principal: u64, commission: u64) -> Vec<u8> {
        let mut b = vec![0u8; ProxyState::LEN];
        b[0..32].copy_from_slice(branded.as_ref());
        b[32..64].copy_from_slice(stablecoin.as_ref());
        b[64..66].copy_from_slice(&fee.to_le_bytes());
        b[66..74].copy_from_slice(&principal.to_le_bytes());
        b[74..82].copy_from_slice(&commission.to_le_bytes());
        b
    }
    fn oracle_buf(slot: u64, price: u64, precision: u8) -> Vec<u8> {
        let mut b = vec![0u8; Oracle::LEN];
        b[0..8].copy_from_slice(&slot.to_le_bytes());
        b[8..16].copy_from_slice(&price.to_le_bytes());
        b[16] = precision;
        b
    }
    fn mint_buf(supply: u64, decimals: u8) -> Vec<u8> {
        let mut b = vec![0u8; 82];
        b[36..44].copy_from_slice(&supply.to_le_bytes()); // Mint.supply @36
        b[44] = decimals; // Mint.decimals @44
        b
    }
    fn token_buf(amount: u64) -> Vec<u8> {
        let mut b = vec![0u8; 165];
        b[64..72].copy_from_slice(&amount.to_le_bytes()); // TokenAccount.amount @64
        b
    }
    fn clk(slot: u64) -> Clock {
        Clock { slot, unix: 0 }
    }

    // A genesis branded market ($1 par, empty) at the CANONICAL PDA for `branded`.
    fn genesis(branded: Pubkey) -> (Pubkey, Vec<(Pubkey, Pubkey, Vec<u8>)>) {
        let stablecoin = pk(99);
        let proxy_state = pda::derive_proxy_state(&branded).0;
        let vault = pda::proxy_state_vault(&proxy_state, &stablecoin);
        let accounts = vec![
            (proxy_state, ids::PROGRAM, proxy_state_buf(&branded, &stablecoin, 200, 0, 0)),
            (ids::ORACLE, pk(200), oracle_buf(100, 1_000_000_000_000, 12)),
            (vault, pk(201), token_buf(0)),
            (branded, pk(202), mint_buf(0, 6)),
            (stablecoin, pk(202), mint_buf(0, 6)),
        ];
        (proxy_state, accounts)
    }

    #[test]
    fn cold_venue_has_no_mints_and_zero_quote_is_zero() {
        let v = WhitelabelVenue::from_market(Pubkey::new_unique());
        assert!(v.mints().is_empty());
        let z = v.quote(Pubkey::new_unique(), 0, clk(0)).unwrap();
        assert!(matches!(z, Some(RouteQuote { amount_out: 0, .. })));
        assert!(v.quote(Pubkey::new_unique(), 1, clk(0)).is_err());
        assert!(v.route(Pubkey::new_unique()).is_err());
        assert!(v.build(Pubkey::new_unique(), Pubkey::new_unique(), 1, 0, clk(0)).is_err());
    }

    #[test]
    fn seeds_is_the_market_account() {
        let market = Pubkey::new_unique();
        let v = WhitelabelVenue::from_market(market);
        assert_eq!(v.seeds(), vec![market]);
    }

    #[test]
    fn load_rejects_wrong_owner() {
        let branded = pk(7);
        let (proxy_state, mut accounts) = genesis(branded);
        // Corrupt the owner of the market account itself.
        accounts[0].1 = Pubkey::new_unique();
        let mut v = WhitelabelVenue::from_market(proxy_state);
        assert!(v.load(accounts).is_err());
    }

    #[test]
    fn load_rejects_market_not_at_canonical_pda() {
        // Market key does NOT match derive_proxy_state(branded) - the
        // fetched map won't contain the key from_account_map re-derives.
        let branded = pk(7);
        let (_, accounts) = genesis(branded);
        let mut v = WhitelabelVenue::from_market(Pubkey::new_unique());
        assert!(v.load(accounts).is_err());
    }

    #[test]
    fn discovery_keys_are_state_oracle_vault_and_both_mints() {
        let branded = pk(7);
        let (proxy_state, accounts) = genesis(branded);
        let state_bytes = &accounts[0].2;
        let v = WhitelabelVenue::from_market(proxy_state);
        let keys = v.discovery_keys(state_bytes).unwrap();
        assert_eq!(keys.len(), 5);
        assert!(keys.contains(&proxy_state));
        assert!(keys.contains(&ids::ORACLE));
        assert!(keys.contains(&branded));
    }

    #[test]
    fn loaded_venue_quotes_and_builds_wrap() {
        let branded = pk(7);
        let (proxy_state, accounts) = genesis(branded);
        let stablecoin = accounts[4].0;
        let mut v = WhitelabelVenue::from_market(proxy_state);
        v.load(accounts).unwrap();
        assert_eq!(v.mints().len(), 2);
        assert_eq!(v.route(stablecoin).unwrap(), WhitelabelRoute::Wrap);
        assert_eq!(v.route(branded).unwrap(), WhitelabelRoute::Unwrap);

        let q = v.quote(stablecoin, 1_000_000, clk(100)).unwrap().unwrap();
        assert_eq!(q.amount_out, 1_000_000); // genesis 1:1
        assert_eq!(q.fee, 0);

        let ix = v.build(Pubkey::new_unique(), stablecoin, 1_000_000, 0, clk(100)).unwrap();
        assert_eq!(ix.program_id, ids::PROGRAM);
    }

    #[test]
    fn stale_oracle_is_no_route_not_an_error() {
        let branded = pk(7);
        let (proxy_state, accounts) = genesis(branded);
        let stablecoin = accounts[4].0;
        let mut v = WhitelabelVenue::from_market(proxy_state);
        v.load(accounts).unwrap();
        // Oracle slot is 100; > 200 slots later is stale.
        assert!(v.quote(stablecoin, 1_000_000, clk(100_000)).unwrap().is_none());
    }
}
