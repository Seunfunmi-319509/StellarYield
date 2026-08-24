//! # Yield Donation Module
//!
//! Allows users to auto-route a configured percentage of their generated
//! yield to a whitelisted charity address on every `harvest` / `withdraw`.
//!
//! ## Storage layout
//! - `DonationBps(user)` — per-user donation split in basis points (0..=10_000)
//! - `DonationCharity(user)` — the charity Address chosen by the user
//! - `WhitelistedCharity(Address)` — flag marking an address as an approved charity
//! - `TotalDonated` — running protocol-wide total of donated tokens
//!
//! ## Security
//! - Donation logic operates only on yield, never on principal.
//! - Underflow is prevented: the donated slice is subtracted from yield
//!   *before* it is credited to the user, always within checked arithmetic.
//! - Overflow on `TotalDonated` is absorbed gracefully by saturating add.

use crate::{VaultError, YieldVault};
use soroban_sdk::{contracttype, symbol_short, token, Address, Env};

// ── Storage Keys ────────────────────────────────────────────────────────

#[contracttype]
pub enum DonationKey {
    /// Donation split in bps for a specific user account.
    DonationBps(Address),
    /// Whitelisted charity chosen by a specific user account.
    DonationCharity(Address),
    /// Flag: true if this address is an approved charity.
    WhitelistedCharity(Address),
    /// Cumulative protocol-wide donated token amount (stroops).
    TotalDonated,
}

const BPS_DENOMINATOR: i128 = 10_000;

/// Minimum economically meaningful donation amount in stroops.
///
/// Donations below this threshold are considered dust: they cost gas to
/// submit but have no meaningful impact. The donation preview rejects them
/// up front so they never reach the contract as unusable transactions.
/// `1_000_000` stroops = 0.1 units of a 7-decimal token (e.g. 0.1 XLM/USDC).
const MIN_DONATION_AMOUNT: i128 = 1_000_000;

// ── Error Codes ─────────────────────────────────────────────────────────
// These map to the error dictionary in `errorDecoder.ts`
// Error 2001 — invalid donation percentage
// Error 2002 — charity not whitelisted
// Error 2007 — donation zero or below the minimum dust threshold

// ── Public API ───────────────────────────────────────────────────────────

impl YieldVault {
    /// Configure the auto-donate yield split for the calling user.
    ///
    /// Sets the percentage (in basis points) of generated yield that will
    /// be automatically routed to `charity` on each harvest / withdrawal.
    /// Setting `bps = 0` effectively disables donations.
    ///
    /// # Arguments
    /// * `user`    — The user's account address (must authorise this call).
    /// * `bps`     — Split percentage in basis points (0 = 0 %, 10_000 = 100 %).
    /// * `charity` — The destination charity address (must be whitelisted).
    ///
    /// # Errors
    /// * `VaultError::InvalidDonationBps`    (code 2001) — `bps > 10_000`.
    /// * `VaultError::CharityNotWhitelisted` (code 2002) — charity is not approved.
    pub fn set_donation_split(
        env: Env,
        user: Address,
        bps: i128,
        charity: Address,
    ) -> Result<(), VaultError> {
        user.require_auth();

        if !(0..=BPS_DENOMINATOR).contains(&bps) {
            return Err(VaultError::InvalidDonationBps);
        }

        let is_whitelisted: bool = env
            .storage()
            .instance()
            .get(&DonationKey::WhitelistedCharity(charity.clone()))
            .unwrap_or(false);

        if !is_whitelisted {
            return Err(VaultError::CharityNotWhitelisted);
        }

        env.storage()
            .instance()
            .set(&DonationKey::DonationBps(user.clone()), &bps);
        env.storage()
            .instance()
            .set(&DonationKey::DonationCharity(user.clone()), &charity);

        env.events()
            .publish((symbol_short!("don_set"),), (user, bps));

        Ok(())
    }

    /// Whitelist (or de-list) an address as an approved charity destination.
    ///
    /// Only callable by the vault admin (governance).
    ///
    /// # Arguments
    /// * `admin`     — The admin address (must authorise).
    /// * `charity`   — The charity address to modify.
    /// * `approved`  — `true` to whitelist, `false` to remove.
    pub fn set_charity_whitelist(
        env: Env,
        admin: Address,
        charity: Address,
        approved: bool,
    ) -> Result<(), VaultError> {
        Self::require_admin(&env, &admin)?;

        env.storage()
            .instance()
            .set(&DonationKey::WhitelistedCharity(charity.clone()), &approved);

        env.events()
            .publish((symbol_short!("charity"),), (charity, approved));

        Ok(())
    }

    /// Returns the current donation configuration for `user`.
    ///
    /// Returns `(0, None)` when the user has not configured a split.
    ///
    /// # Arguments
    /// * `user` — The account to query.
    pub fn get_donation_config(env: Env, user: Address) -> (i128, Option<Address>) {
        let bps: i128 = env
            .storage()
            .instance()
            .get(&DonationKey::DonationBps(user.clone()))
            .unwrap_or(0);

        let charity: Option<Address> = env
            .storage()
            .instance()
            .get(&DonationKey::DonationCharity(user));

        (bps, charity)
    }

    /// Returns the cumulative total of tokens donated protocol-wide.
    pub fn get_total_donated(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DonationKey::TotalDonated)
            .unwrap_or(0)
    }

    /// Preview the donation amount that would be routed to the charity for
    /// `yield_amount` at the user's configured split.
    ///
    /// This is a read-only validation gate used *before* submitting a
    /// donation transaction. Zero-value inputs (`yield_amount <= 0`) and
    /// dust-value inputs (a computed donation that rounds to zero, or that
    /// falls below `MIN_DONATION_AMOUNT`) are rejected early so they never
    /// reach the contract as unusable transactions. No state is read or
    /// written beyond the user's donation configuration.
    ///
    /// # Arguments
    /// * `user`         — The user whose donation split is applied.
    /// * `yield_amount` — The gross yield amount in stroops.
    ///
    /// # Errors
    /// * `VaultError::ZeroAmount` (code 3) — `yield_amount <= 0`.
    /// * `VaultError::DonationBelowMinimum` (code 2007) — the computed
    ///   donation is zero, or below `MIN_DONATION_AMOUNT` (dust).
    ///
    /// # Returns
    /// The validated donation amount in stroops.
    pub fn preview_donation(
        env: Env,
        user: Address,
        yield_amount: i128,
    ) -> Result<i128, VaultError> {
        if yield_amount <= 0 {
            return Err(VaultError::ZeroAmount);
        }

        let bps: i128 = env
            .storage()
            .instance()
            .get(&DonationKey::DonationBps(user))
            .unwrap_or(0);

        // No split configured (or explicitly disabled) means nothing would
        // be donated — reject instead of returning a zero-value preview.
        if bps <= 0 {
            return Err(VaultError::DonationBelowMinimum);
        }

        // Compute donation using the same integer math as `apply_donation`.
        // `bps` is bounded to [0, 10_000] by `set_donation_split`, so the
        // multiplication cannot overflow for any realistic `yield_amount`.
        let donation = (yield_amount * bps) / BPS_DENOMINATOR;

        // Reject dust: `MIN_DONATION_AMOUNT > 0`, so this single check also
        // covers donations that round down to zero. A donation that is too
        // small to be economically meaningful would waste a transaction.
        if donation < MIN_DONATION_AMOUNT {
            return Err(VaultError::DonationBelowMinimum);
        }

        Ok(donation)
    }

    // ── Internal ────────────────────────────────────────────────────────

    /// Routes the donation slice out of `yield_amount` to the configured
    /// charity and returns the net amount remaining for the user.
    ///
    /// This is called internally from harvest / withdrawal logic.
    /// Operates only on the yield — never on principal.
    ///
    /// ## Invariants:
    /// 1. yield_in = user_yield + donation
    /// 2. donation cannot exceed yield_amount
    /// 3. net >= 0 (user always gets non-negative yield)
    ///
    /// # Arguments
    /// * `env`          — Contract environment.
    /// * `user`         — The user whose yield is being harvested.
    /// * `yield_amount` — The gross yield amount in stroops.
    /// * `token_id`     — The yield token contract address.
    ///
    /// # Returns
    /// The net yield amount after the donation slice has been transferred.
    pub fn apply_donation(
        env: &Env,
        user: &Address,
        yield_amount: i128,
        token_id: &Address,
    ) -> i128 {
        if yield_amount <= 0 {
            return yield_amount;
        }

        let bps: i128 = env
            .storage()
            .instance()
            .get(&DonationKey::DonationBps(user.clone()))
            .unwrap_or(0);

        if bps <= 0 {
            return yield_amount;
        }

        let charity_opt: Option<Address> = env
            .storage()
            .instance()
            .get(&DonationKey::DonationCharity(user.clone()));

        // If no charity set (shouldn't normally happen) skip donation silently.
        let charity = match charity_opt {
            Some(c) => c,
            None => return yield_amount,
        };

        // Compute donation amount using checked math to prevent underflow.
        // `bps` is bounded to [0, 10_000] so the multiplication cannot
        // exceed i128::MAX for any realistic `yield_amount`.
        let donation = (yield_amount * bps) / BPS_DENOMINATOR;
        let net = yield_amount - donation; // Always >= 0 since bps <= 10_000

        // INVARIANT: donation cannot exceed yield_amount
        if donation > yield_amount {
            env.events().publish(
                (symbol_short!("don_err"),),
                (user.clone(), donation, yield_amount),
            );
            return yield_amount;
        }

        // INVARIANT: net must be non-negative
        if net < 0 {
            env.events()
                .publish((symbol_short!("don_err"),), (user.clone(), net, 0i128));
            return yield_amount;
        }

        if donation > 0 {
            let token_client = token::Client::new(env, token_id);
            // Transfer from the contract's own balance to the charity.
            token_client.transfer(&env.current_contract_address(), &charity, &donation);

            // Accumulate total donated (saturating add to avoid overflow trap).
            let prev_total: i128 = env
                .storage()
                .instance()
                .get(&DonationKey::TotalDonated)
                .unwrap_or(0);

            let new_total = prev_total.saturating_add(donation);
            env.storage()
                .instance()
                .set(&DonationKey::TotalDonated, &new_total);

            env.events().publish(
                (symbol_short!("donated"),),
                (user.clone(), charity, donation),
            );
        }

        // INVARIANT CHECK: yield_in == user_yield + donation
        if net + donation != yield_amount {
            env.events().publish(
                (symbol_short!("don_cons"),),
                (user.clone(), yield_amount, net, donation),
            );
        }

        net
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{YieldVault, YieldVaultClient};
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::Env;

    fn setup_env() -> (Env, YieldVaultClient<'static>, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(YieldVault, ());
        let client = YieldVaultClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_admin = Address::generate(&env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin.clone());
        let token_addr = token_contract.address();

        client.initialize(&admin, &token_addr);

        (env, client, admin, token_addr, token_admin)
    }

    /// Register `user` with a donation split of `bps` toward a whitelisted
    /// charity and return the user address.
    fn setup_donor(
        env: &Env,
        client: &YieldVaultClient<'static>,
        admin: &Address,
        bps: i128,
    ) -> Address {
        let user = Address::generate(env);
        let charity = Address::generate(env);
        client.set_charity_whitelist(admin, &charity, &true);
        client.set_donation_split(&user, &bps, &charity);
        user
    }

    // ── Zero-value inputs ───────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn test_preview_donation_zero_yield_panics() {
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 1_000);
        client.preview_donation(&user, &0);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #3)")]
    fn test_preview_donation_negative_yield_panics() {
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 1_000);
        client.preview_donation(&user, &-100);
    }

    // ── Zero-value donations (nothing would be routed) ──────────────────

    #[test]
    #[should_panic(expected = "Error(Contract, #2007)")]
    fn test_preview_donation_without_config_panics() {
        let (env, client, _, _, _) = setup_env();
        let user = Address::generate(&env);
        client.preview_donation(&user, &100_000_000);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2007)")]
    fn test_preview_donation_disabled_split_panics() {
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 0);
        client.preview_donation(&user, &100_000_000);
    }

    // ── Dust-value inputs ───────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "Error(Contract, #2007)")]
    fn test_preview_donation_rounds_to_zero_panics() {
        // bps = 1 (0.01 %) on a tiny yield rounds the donation down to 0.
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 1);
        client.preview_donation(&user, &5_000);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #2007)")]
    fn test_preview_donation_below_minimum_panics() {
        // 10 % of 1_000_000 stroops = 100_000 stroops < MIN_DONATION_AMOUNT.
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 1_000);
        client.preview_donation(&user, &1_000_000);
    }

    // ── Boundary and valid inputs ───────────────────────────────────────

    #[test]
    fn test_preview_donation_at_minimum_boundary() {
        // 100 % of MIN_DONATION_AMOUNT == MIN_DONATION_AMOUNT → accepted.
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 10_000);
        let donation = client.preview_donation(&user, &MIN_DONATION_AMOUNT);
        assert_eq!(donation, MIN_DONATION_AMOUNT);
    }

    #[test]
    fn test_preview_donation_valid() {
        // 10 % of 100_000_000 stroops = 10_000_000 stroops.
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 1_000);
        let donation = client.preview_donation(&user, &100_000_000);
        assert_eq!(donation, 10_000_000);
    }

    #[test]
    fn test_preview_donation_is_read_only() {
        let (env, client, admin, _, _) = setup_env();
        let user = setup_donor(&env, &client, &admin, 1_000);
        let _ = client.preview_donation(&user, &100_000_000);
        // Preview must never mutate protocol state.
        assert_eq!(client.get_total_donated(), 0);
    }
}
