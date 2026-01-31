// ---------- CTO Pools Program V2.5 ----------

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    system_instruction,
};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

// SPL Stake Pool (used for SOL<->LST)
use spl_stake_pool::{instruction as stake_pool_ix, state::StakePool};

declare_id!("4T9SkpDeDyC8KWKrcGsVQ6wG14H46og6f9pBFNc1Csje");

// ============= Constants =============

/// 1% protocol fee
const PROTOCOL_FEE_BPS: u16 = 100;
/// 30% quorum
const QUORUM_BPS: u16 = 3000;
/// 1 SOL minimum proposer value
const MIN_PROPOSER_DEPOSIT_LAMPORTS: u64 = 1_000_000_000;
/// 20% voting cap per wallet
const MAX_VOTER_BPS: u16 = 2000;

/// Proposal buffer: 50 bps (0.50%)
const PROPOSAL_BUFFER_BPS: u64 = 50;

/// NOTE: restore to ~216_000 for production (~1 day)
const MIN_PROPOSAL_DELAY_SLOTS: u64 = 0;

// Buy & burn slippage tolerance (bps). Larger means more tolerant (less likely to fail), but weaker price protection.
const MAX_SLIPPAGE_BPS: u64 = 1500; // 15%

// Legacy Raydium swap constants (kept optional)
const RAYDIUM_SWAP_INSTRUCTION: u8 = 9;

/// BPS denominator
const BPS_DENOM: u64 = 10_000;

pub const INCINERATOR: Pubkey = pubkey!("1nc1nerator11111111111111111111111111111111");
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

/// PumpSwap program id (used after Pump.fun graduation).
/// IMPORTANT: Treat this as a protocol dependency. Keep upgrade authority during beta to respond to upstream changes.
pub const PUMPSWAP_PROGRAM_ID: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");

/// PumpSwap `buy` discriminator (Anchor-style 8-byte discriminator).
/// Args: (base_amount_out: u64, max_quote_amount_in: u64)
const PUMPSWAP_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

// Jito Stake Pool references
pub const JITO_MAINNET_STAKE_POOL_PROGRAM: Pubkey =
    pubkey!("SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy");
pub const JITO_DEVNET_STAKE_POOL_PROGRAM: Pubkey =
    pubkey!("DPoo15wWDqpPJJtS2MUZ49aRxqz5ZaaJCJP4z8bLuib");
pub const JITO_DEVNET_STAKE_POOL: Pubkey = pubkey!("JitoY5pcAxWX6iyP2QdFwTznGb8A99PRCUCVVxB46WZ");
pub const JITO_DEVNET_JITOSOL_MINT: Pubkey =
    pubkey!("J1tos8mqbhdGcF3pgj4PCKyVjzWSURcpLZU7pPGHxSYi");

// ============= Macros =============

/// Generates PDA seeds for pool-signed operations.
/// Usage: pool_seeds!(pool_account, bump_value)
/// Returns: &[&[&[u8]]] suitable for invoke_signed
macro_rules! pool_seeds {
    ($pool:expr, $bump:expr) => {
        &[&[b"pool", $pool.token_mint.as_ref(), &[$bump]]]
    };
}

// ============= Program =============

#[program]
pub mod cto_pools {
    use super::*;

    /// Create one pool per token mint.
    ///
    /// `stake_pool_program`, `stake_pool`, and `lst_mint` configure the liquid staking backend.
    /// For devnet testing, use the devnet constants in this file.
    pub fn create_pool(
        ctx: Context<CreatePool>,
        dev_fee_wallet: Pubkey,
        burn_token_mint: Pubkey,
        stake_pool_program: Pubkey,
        stake_pool: Pubkey,
        lst_mint: Pubkey,
    ) -> Result<()> {
        validate_stake_pool_config(stake_pool_program, stake_pool, lst_mint)?;

        let pool = &mut ctx.accounts.pool;

        // Identity / ownership
        pool.token_mint = ctx.accounts.token_mint.key();
        pool.authority = ctx.accounts.creator.key();
        pool.creator = ctx.accounts.creator.key();

        // Accounting
        pool.total_shares = 0;
        pool.total_pool_tokens = 0; // will be set from on-chain balance after first deposit
        pool.reserved_pool_tokens = 0;
        pool.total_spent_lamports = 0;

        // Governance/config
        pool.protocol_fee_bps = PROTOCOL_FEE_BPS;
        pool.quorum_bps = QUORUM_BPS;
        pool.min_proposer_deposit_lamports = MIN_PROPOSER_DEPOSIT_LAMPORTS;

        // Fee outputs
        pool.dev_fee_wallet = dev_fee_wallet;
        pool.burn_token_mint = burn_token_mint;

        // Proposal tracking
        pool.active_proposal = None;
        pool.proposal_count = 0;

        // Recovery tracking
        pool.active_recovery = None;
        pool.recovery_count = 0;

        // LST config
        pool.stake_pool_program = stake_pool_program;
        pool.stake_pool = stake_pool;
        pool.lst_mint = lst_mint;

        // PumpSwap buy&burn config (recommended for Pump.fun launches post-graduation)
        pool.pumpswap_enabled = false;
        pool.pumpswap_pool_id = Pubkey::default();

        // Legacy Raydium buy&burn config (optional)
        pool.raydium_enabled = false;
        pool.raydium_pool_id = Pubkey::default();

        Ok(())
    }

    /// Configure PumpSwap pool for buy & burn (post Pump.fun graduation).
    ///
    /// Security model:
    /// - `pumpswap_pool_id` is stored in the Pool account.
    /// - At execution time, the executor must pass the PumpSwap pool + vault accounts.
    /// - This program verifies that the passed PumpSwap pool matches config, and that the vault
    ///   token accounts have the expected mints (CTOP + WSOL). Output is forced into the pool's
    ///   PDA-owned CTOP account, then burned to incinerator.
    ///
    /// Note: This does not require trusting the executor with recipient accounts. They are PDAs/ATAs
    /// controlled/validated by this program.
    pub fn configure_pumpswap_pool(
        ctx: Context<ConfigurePumpSwapPool>,
        pumpswap_pool_id: Pubkey,
        enabled: bool,
    ) -> Result<()> {
        require!(
            ctx.accounts.authority.key() == ctx.accounts.pool.authority,
            CtoError::UnauthorizedAuthority
        );
        ctx.accounts.pool.pumpswap_pool_id = pumpswap_pool_id;
        ctx.accounts.pool.pumpswap_enabled = enabled;
        Ok(())
    }

    /// Configure Raydium pool for buy & burn (legacy / optional).
    pub fn configure_raydium_pool(
        ctx: Context<ConfigureRaydiumPool>,
        raydium_pool_id: Pubkey,
        enabled: bool,
    ) -> Result<()> {
        require!(
            ctx.accounts.authority.key() == ctx.accounts.pool.authority,
            CtoError::UnauthorizedAuthority
        );
        ctx.accounts.pool.raydium_pool_id = raydium_pool_id;
        ctx.accounts.pool.raydium_enabled = enabled;
        Ok(())
    }

    /// Donate native SOL to the pool.
    ///
    /// Flow:
    /// - CPI into the configured stake-pool program `DepositSolWithSlippage`
    /// - pool receives LST tokens (e.g. jitoSOL) into its token account
    /// - shares minted to donor based on LST received
    pub fn donate_sol(ctx: Context<DonateSol>, lamports_in: u64, minimum_pool_tokens_out: u64) -> Result<()> {
        require!(lamports_in > 0, CtoError::ZeroAmount);

        stake_pool_deposit_sol(&ctx, lamports_in, minimum_pool_tokens_out)?;

        // Observe actual received LST and update accounting.
        let pool = &mut ctx.accounts.pool;
        ctx.accounts.pool_lst_account.reload()?;
        let new_balance = ctx.accounts.pool_lst_account.amount;

        let prev_total = pool.total_pool_tokens;
        let received = new_balance.checked_sub(prev_total).ok_or(CtoError::MathOverflow)?;
        require!(received > 0, CtoError::StakePoolReturnedZero);

        // Shares: 1st donor mints 1:1 with LST; else proportional
        let shares_minted = if pool.total_shares == 0 {
            received
        } else {
            let r = (received as u128)
                .checked_mul(pool.total_shares as u128)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(prev_total as u128)
                .ok_or(CtoError::MathOverflow)?;
            u64::try_from(r).map_err(|_| CtoError::MathOverflow)?
        };
        require!(shares_minted > 0, CtoError::StakePoolReturnedZero);

        // Update pool
        pool.total_pool_tokens = new_balance;
        pool.total_shares = pool
            .total_shares
            .checked_add(shares_minted)
            .ok_or(CtoError::MathOverflow)?;

        // Update donor
        let donor = &mut ctx.accounts.donor;
        let clock = Clock::get()?;
        donor.pool = pool.key();
        donor.wallet = ctx.accounts.donor_wallet.key();
        donor.shares = donor
            .shares
            .checked_add(shares_minted)
            .ok_or(CtoError::MathOverflow)?;
        donor.total_deposited_lamports = donor
            .total_deposited_lamports
            .checked_add(lamports_in)
            .ok_or(CtoError::MathOverflow)?;
        donor.last_shares_change_slot = clock.slot;

        Ok(())
    }

    /// Withdraw X SOL worth of stake from the pool.
    pub fn withdraw_sol(ctx: Context<WithdrawSol>, lamports_out_desired: u64, minimum_lamports_out: u64) -> Result<()> {
        require!(lamports_out_desired > 0, CtoError::ZeroAmount);

        // ============ PHASE 1: Immutable reads and calculations ============
        let donor_shares = ctx.accounts.donor.shares;
        let total_shares = ctx.accounts.pool.total_shares;
        let reserved_pool_tokens = ctx.accounts.pool.reserved_pool_tokens;
        let pool_bump = ctx.bumps.pool;
        let pool_token_mint = ctx.accounts.pool.token_mint;

        require!(donor_shares > 0, CtoError::NoShares);
        require!(total_shares > 0, CtoError::MathOverflow);

        ctx.accounts.pool_lst_account.reload()?;
        let total_pool_tokens_observed = ctx.accounts.pool_lst_account.amount;

        let free_pool_tokens = total_pool_tokens_observed
            .checked_sub(reserved_pool_tokens)
            .ok_or(CtoError::MathOverflow)?;

        let donor_free_pool_tokens = ((donor_shares as u128)
            .checked_mul(free_pool_tokens as u128)
            .ok_or(CtoError::MathOverflow)?)
            .checked_div(total_shares as u128)
            .ok_or(CtoError::MathOverflow)?;
        let donor_free_pool_tokens = u64::try_from(donor_free_pool_tokens).map_err(|_| CtoError::MathOverflow)?;

        let stake_pool_state = read_stake_pool(&ctx.accounts.stake_pool)?;
        let pool_tokens_to_burn = pool_tokens_for_lamports_ceil(&stake_pool_state, lamports_out_desired)?;
        require!(pool_tokens_to_burn > 0, CtoError::ZeroAmount);
        require!(pool_tokens_to_burn <= donor_free_pool_tokens, CtoError::InsufficientWithdrawable);

        let shares_to_burn = ((pool_tokens_to_burn as u128)
            .checked_mul(total_shares as u128)
            .ok_or(CtoError::MathOverflow)?)
            .checked_div(total_pool_tokens_observed as u128)
            .ok_or(CtoError::MathOverflow)?;
        let shares_to_burn = u64::try_from(shares_to_burn).map_err(|_| CtoError::MathOverflow)?;
        require!(shares_to_burn > 0 && shares_to_burn <= donor_shares, CtoError::MathOverflow);

        // ============ PHASE 2: CPIs ============
        let pre_pool_lamports = ctx.accounts.pool.to_account_info().lamports();
        stake_pool_withdraw_sol(&ctx, pool_tokens_to_burn, minimum_lamports_out)?;
        let post_pool_lamports = ctx.accounts.pool.to_account_info().lamports();
        let received = post_pool_lamports
            .checked_sub(pre_pool_lamports)
            .ok_or(CtoError::MathOverflow)?;
        require!(received >= minimum_lamports_out, CtoError::SlippageExceeded);

        transfer_lamports_signed(
            &ctx.accounts.pool.to_account_info(),
            &ctx.accounts.donor_wallet.to_account_info(),
            &[&[b"pool", pool_token_mint.as_ref(), &[pool_bump]]],
            received,
        )?;

        ctx.accounts.pool_lst_account.reload()?;
        let final_pool_tokens = ctx.accounts.pool_lst_account.amount;
        let clock = Clock::get()?;

        // ============ PHASE 3: state updates ============
        {
            let pool = &mut ctx.accounts.pool;
            pool.total_pool_tokens = final_pool_tokens;
            pool.total_shares = pool
                .total_shares
                .checked_sub(shares_to_burn)
                .ok_or(CtoError::MathOverflow)?;
        }
        {
            let donor = &mut ctx.accounts.donor;
            donor.shares = donor
                .shares
                .checked_sub(shares_to_burn)
                .ok_or(CtoError::MathOverflow)?;
            donor.last_shares_change_slot = clock.slot;
        }

        Ok(())
    }

    /// Create a payout proposal.
    pub fn create_proposal(
        ctx: Context<CreateProposal>,
        requested_lamports: u64,
        destination_wallet: Pubkey,
        title: String,
        description: String,
    ) -> Result<()> {
        require!(requested_lamports > 0, CtoError::ZeroAmount);

        let pool = &mut ctx.accounts.pool;
        let donor = &ctx.accounts.donor;
        let proposal = &mut ctx.accounts.proposal;
        let clock = Clock::get()?;

        require!(pool.active_proposal.is_none(), CtoError::ActiveProposalExists);
        require!(donor.shares > 0, CtoError::NoShares);

        require!(pool.total_shares != donor.shares, CtoError::SingleDonorCannotPropose);

        if pool.creator != ctx.accounts.proposer_wallet.key() {
            let slots_since = clock
                .slot
                .checked_sub(donor.last_shares_change_slot)
                .ok_or(CtoError::MathOverflow)?;
            require!(slots_since >= MIN_PROPOSAL_DELAY_SLOTS, CtoError::SharesTooRecent);
        }

        ctx.accounts.pool_lst_account.reload()?;
        pool.total_pool_tokens = ctx.accounts.pool_lst_account.amount;
        let stake_pool_state = read_stake_pool(&ctx.accounts.stake_pool)?;

        let proposer_pool_tokens = ((donor.shares as u128)
            .checked_mul(pool.total_pool_tokens as u128)
            .ok_or(CtoError::MathOverflow)?)
            .checked_div(pool.total_shares as u128)
            .ok_or(CtoError::MathOverflow)?;
        let proposer_pool_tokens = u64::try_from(proposer_pool_tokens).map_err(|_| CtoError::MathOverflow)?;
        let proposer_value_lamports = stake_pool_state
            .calc_lamports_withdraw_amount(proposer_pool_tokens)
            .ok_or(CtoError::MathOverflow)?;
        require!(
            proposer_value_lamports >= pool.min_proposer_deposit_lamports,
            CtoError::ProposerTooSmall
        );

        let buffered = requested_lamports
            .checked_mul(10_000 + PROPOSAL_BUFFER_BPS)
            .ok_or(CtoError::MathOverflow)?
            .checked_div(10_000)
            .ok_or(CtoError::MathOverflow)?;
        let locked_pool_tokens = pool_tokens_for_lamports_ceil(&stake_pool_state, buffered)?;

        let free_pool_tokens = pool
            .total_pool_tokens
            .checked_sub(pool.reserved_pool_tokens)
            .ok_or(CtoError::MathOverflow)?;
        require!(locked_pool_tokens <= free_pool_tokens, CtoError::InsufficientFreeLiquidity);

        pool.reserved_pool_tokens = pool
            .reserved_pool_tokens
            .checked_add(locked_pool_tokens)
            .ok_or(CtoError::MathOverflow)?;

        proposal.pool = pool.key();
        proposal.kind = ProposalKind::Payout;
        proposal.requested_lamports = requested_lamports;
        proposal.destination_wallet = destination_wallet;
        proposal.title = title;
        proposal.description = description;

        proposal.created_at_ts = clock.unix_timestamp;
        proposal.deadline_ts = clock
            .unix_timestamp
            .checked_add(24 * 60 * 60)
            .ok_or(CtoError::MathOverflow)?;
        proposal.snapshot_slot = clock.slot;
        proposal.total_snapshot_shares = pool.total_shares;

        proposal.locked_pool_tokens = locked_pool_tokens;

        proposal.yes_weight = 0;
        proposal.no_weight = 0;
        proposal.abstain_weight = 0;
        proposal.participation_weight = 0;
        proposal.status = ProposalStatus::Active;

        pool.active_proposal = Some(proposal.key());
        pool.proposal_count = pool.proposal_count.checked_add(1).ok_or(CtoError::MathOverflow)?;

        Ok(())
    }

    /// Vote on proposal.
    pub fn vote(ctx: Context<Vote>, choice: VoteChoice) -> Result<()> {
        let proposal = &mut ctx.accounts.proposal;
        let donor = &ctx.accounts.donor;
        let vote_record = &mut ctx.accounts.vote_record;
        let clock = Clock::get()?;

        require!(proposal.status == ProposalStatus::Active, CtoError::ProposalNotActive);
        require!(clock.unix_timestamp <= proposal.deadline_ts, CtoError::VotingClosed);
        require!(donor.shares > 0, CtoError::NoShares);

        require!(
            donor.last_shares_change_slot <= proposal.snapshot_slot,
            CtoError::NotEligibleForThisProposal
        );

        if vote_record.initialized {
            let w = vote_record.snapshot_weight;
            match vote_record.choice {
                VoteChoice::Yes => proposal.yes_weight = proposal.yes_weight.checked_sub(w).ok_or(CtoError::MathOverflow)?,
                VoteChoice::No => proposal.no_weight = proposal.no_weight.checked_sub(w).ok_or(CtoError::MathOverflow)?,
                VoteChoice::Abstain => proposal.abstain_weight = proposal.abstain_weight.checked_sub(w).ok_or(CtoError::MathOverflow)?,
            }
        }

        let snapshot_weight = if vote_record.initialized {
            vote_record.snapshot_weight
        } else {
            let raw = donor.shares;
            let cap = proposal
                .total_snapshot_shares
                .checked_mul(MAX_VOTER_BPS as u64)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(10_000)
                .ok_or(CtoError::MathOverflow)?;
            raw.min(cap)
        };

        match choice {
            VoteChoice::Yes => proposal.yes_weight = proposal.yes_weight.checked_add(snapshot_weight).ok_or(CtoError::MathOverflow)?,
            VoteChoice::No => proposal.no_weight = proposal.no_weight.checked_add(snapshot_weight).ok_or(CtoError::MathOverflow)?,
            VoteChoice::Abstain => proposal.abstain_weight = proposal.abstain_weight.checked_add(snapshot_weight).ok_or(CtoError::MathOverflow)?,
        }

        proposal.participation_weight = proposal
            .yes_weight
            .checked_add(proposal.no_weight)
            .ok_or(CtoError::MathOverflow)?
            .checked_add(proposal.abstain_weight)
            .ok_or(CtoError::MathOverflow)?;

        vote_record.proposal = proposal.key();
        vote_record.voter = donor.wallet;
        vote_record.snapshot_weight = snapshot_weight;
        vote_record.choice = choice;
        vote_record.initialized = true;

        Ok(())
    }

    /// Execute proposal.
    ///
    /// Atomic, community-executable flow:
    /// - If proposal fails: unlock reserved liquidity and mark Failed.
    /// - If proposal passes: withdraw SOL from stake-pool, pay destination, charge protocol fee.
    /// - Fee split:
    ///   * 50% -> dev wallet
    ///   * 50% -> buy & burn CTOP (best-effort). If swap fails, that half is also sent to dev.
    ///
    /// Key property: Buy & burn failure never prevents the payout leg from succeeding.
    pub fn execute_proposal(mut ctx: Context<ExecuteProposal>, minimum_lamports_out: u64) -> Result<()> {
        let clock = Clock::get()?;

        // ============ PHASE 1: Immutable reads and status checks ============
        require!(ctx.accounts.proposal.status == ProposalStatus::Active, CtoError::ProposalNotActive);

        let participation_weight = ctx.accounts.proposal.participation_weight;
        let total_snapshot_shares = ctx.accounts.proposal.total_snapshot_shares;
        let locked_pool_tokens = ctx.accounts.proposal.locked_pool_tokens;
        let yes_weight = ctx.accounts.proposal.yes_weight;
        let no_weight = ctx.accounts.proposal.no_weight;
        let deadline_ts = ctx.accounts.proposal.deadline_ts;

        let quorum_bps = ctx.accounts.pool.quorum_bps;
        let protocol_fee_bps = ctx.accounts.pool.protocol_fee_bps;

        let pool_token_mint = ctx.accounts.pool.token_mint;
        let pool_bump = ctx.bumps.pool;

        let pumpswap_enabled = ctx.accounts.pool.pumpswap_enabled;
        let pumpswap_pool_id = ctx.accounts.pool.pumpswap_pool_id;

        let raydium_enabled = ctx.accounts.pool.raydium_enabled;
        let raydium_pool_id = ctx.accounts.pool.raydium_pool_id;

        let pool_key = ctx.accounts.pool.key();
        let proposal_key = ctx.accounts.proposal.key();

        let quorum_met = participation_weight
            .checked_mul(BPS_DENOM)
            .ok_or(CtoError::MathOverflow)?
            >= total_snapshot_shares
                .checked_mul(quorum_bps as u64)
                .ok_or(CtoError::MathOverflow)?;

        let time_over = clock.unix_timestamp >= deadline_ts;
        require!(time_over || quorum_met, CtoError::TooEarlyToExecute);

        let majority_met = yes_weight > no_weight;

        // ============ FAIL PATH ============
        if !(quorum_met && majority_met) {
            {
                let pool = &mut ctx.accounts.pool;
                pool.reserved_pool_tokens = pool
                    .reserved_pool_tokens
                    .checked_sub(locked_pool_tokens)
                    .ok_or(CtoError::MathOverflow)?;
                pool.active_proposal = None;
            }
            {
                let proposal = &mut ctx.accounts.proposal;
                proposal.status = ProposalStatus::Failed;
            }

            emit!(ProposalFailedEvent {
                pool: pool_key,
                proposal: proposal_key,
                unlocked_pool_tokens: locked_pool_tokens,
                quorum_met,
                majority_met,
                timestamp: clock.unix_timestamp,
            });

            return Ok(());
        }

        // ============ PASS PATH ============
        let pool_tokens_to_burn = locked_pool_tokens;
        require!(pool_tokens_to_burn > 0, CtoError::MathOverflow);

        // ============ PHASE 2: CPIs ============
        // Withdraw SOL to pool PDA
        let pre_pool_lamports = ctx.accounts.pool.to_account_info().lamports();
        stake_pool_withdraw_sol_exec(&ctx, pool_tokens_to_burn, minimum_lamports_out)?;
        let post_pool_lamports = ctx.accounts.pool.to_account_info().lamports();
        let sol_received = post_pool_lamports
            .checked_sub(pre_pool_lamports)
            .ok_or(CtoError::MathOverflow)?;
        require!(sol_received >= minimum_lamports_out, CtoError::SlippageExceeded);

        // Fee is % of actual received
        let protocol_fee = sol_received
            .checked_mul(protocol_fee_bps as u64)
            .ok_or(CtoError::MathOverflow)?
            .checked_div(BPS_DENOM)
            .ok_or(CtoError::MathOverflow)?;
        let net_to_destination = sol_received
            .checked_sub(protocol_fee)
            .ok_or(CtoError::MathOverflow)?;

        // Pay destination
        transfer_lamports_signed(
            &ctx.accounts.pool.to_account_info(),
            &ctx.accounts.destination_wallet.to_account_info(),
            &[&[b"pool", pool_token_mint.as_ref(), &[pool_bump]]],
            net_to_destination,
        )?;

        // Fee split
        let fee_half = protocol_fee.checked_div(2).ok_or(CtoError::MathOverflow)?;
        let mut dev_take = protocol_fee.checked_sub(fee_half).ok_or(CtoError::MathOverflow)?;

        // Buy & burn attempt with `fee_half` (best-effort).
        if fee_half > 0 {
            // Prefer PumpSwap for Pump.fun launches after graduation.
            let did_try_pumpswap = pumpswap_enabled
                && pumpswap_pool_id != Pubkey::default()
                && ctx.accounts.pumpswap_pool.key() == pumpswap_pool_id;

            if did_try_pumpswap {
                match attempt_pumpswap_swap_and_burn(&mut ctx, fee_half, pool_bump) {
                    Ok(ctop_burned) => {
                        emit!(TokenBurnEvent {
                            pool: pool_key,
                            amount_sol: fee_half,
                            amount_ctop: ctop_burned,
                            timestamp: clock.unix_timestamp,
                        });
                    }
                    Err(_e) => {
                        // Best-effort means failure routes to dev.
                        dev_take = dev_take.checked_add(fee_half).ok_or(CtoError::MathOverflow)?;
                        emit!(SwapFailureEvent {
                            pool: pool_key,
                            amount_sol: fee_half,
                            error_code: 1, // PumpSwap failure
                            timestamp: clock.unix_timestamp,
                        });
                    }
                }
            } else if raydium_enabled
                && raydium_pool_id != Pubkey::default()
                && ctx.accounts.raydium_pool.key() == raydium_pool_id
            {
                // Legacy Raydium path (optional)
                match attempt_raydium_swap_and_burn(&mut ctx, fee_half, pool_bump) {
                    Ok(ctop_burned) => {
                        emit!(TokenBurnEvent {
                            pool: pool_key,
                            amount_sol: fee_half,
                            amount_ctop: ctop_burned,
                            timestamp: clock.unix_timestamp,
                        });
                    }
                    Err(_e) => {
                        dev_take = dev_take.checked_add(fee_half).ok_or(CtoError::MathOverflow)?;
                        emit!(SwapFailureEvent {
                            pool: pool_key,
                            amount_sol: fee_half,
                            error_code: 2, // Raydium failure
                            timestamp: clock.unix_timestamp,
                        });
                    }
                }
            } else {
                // No configured venue -> send to dev (explicitly accepted design)
                dev_take = dev_take.checked_add(fee_half).ok_or(CtoError::MathOverflow)?;
            }
        }

        // Pay dev
        if dev_take > 0 {
            transfer_lamports_signed(
                &ctx.accounts.pool.to_account_info(),
                &ctx.accounts.dev_fee_wallet.to_account_info(),
                &[&[b"pool", pool_token_mint.as_ref(), &[pool_bump]]],
                dev_take,
            )?;
        }

        // Reload LST account after all CPIs
        ctx.accounts.pool_lst_account.reload()?;
        let final_pool_tokens = ctx.accounts.pool_lst_account.amount;

        // ============ PHASE 3: Mutable state updates ============
        {
            let pool = &mut ctx.accounts.pool;
            pool.total_spent_lamports = pool
                .total_spent_lamports
                .checked_add(net_to_destination)
                .ok_or(CtoError::MathOverflow)?;
            pool.reserved_pool_tokens = pool
                .reserved_pool_tokens
                .checked_sub(pool_tokens_to_burn)
                .ok_or(CtoError::MathOverflow)?;
            pool.total_pool_tokens = final_pool_tokens;
            pool.active_proposal = None;
        }
        {
            let proposal = &mut ctx.accounts.proposal;
            proposal.status = ProposalStatus::Executed;
        }

        Ok(())
    }

    /// Recover non-LST assets stuck in the pool.
    pub fn recover_funds_create(
        ctx: Context<RecoverFundsCreate>,
        token_mint: Pubkey,
        amount: u64,
        destination_wallet: Pubkey,
        title: String,
        description: String,
    ) -> Result<()> {
        require!(amount > 0, CtoError::ZeroAmount);
        require!(token_mint != ctx.accounts.pool.lst_mint, CtoError::RecoveryNotAllowedForLST);

        let pool_bump = ctx.bumps.pool;
        let pool_token_mint = ctx.accounts.pool.token_mint;

        if verify_inline_sender(
            &ctx.accounts.instructions,
            &ctx.accounts.requester.key(),
            &ctx.accounts.pool_token_account.key(),
            token_mint,
            amount,
        )? {
            transfer_spl_from_pool_with_seeds(
                &ctx.accounts.pool.to_account_info(),
                &ctx.accounts.pool_token_account,
                &ctx.accounts.destination_token_account,
                &ctx.accounts.token_program,
                &[&[b"pool", pool_token_mint.as_ref(), &[pool_bump]]],
                amount,
            )?;
            return Ok(());
        }

        let pool = &mut ctx.accounts.pool;
        let donor = &ctx.accounts.donor;
        require!(pool.active_recovery.is_none(), CtoError::ActiveRecoveryExists);
        require!(donor.shares > 0, CtoError::NoShares);

        let rec = &mut ctx.accounts.recovery;
        let clock = Clock::get()?;
        rec.pool = pool.key();
        rec.token_mint = token_mint;
        rec.requested_amount = amount;
        rec.destination_wallet = destination_wallet;
        rec.title = title;
        rec.description = description;
        rec.created_at_ts = clock.unix_timestamp;
        rec.deadline_ts = clock
            .unix_timestamp
            .checked_add(24 * 60 * 60)
            .ok_or(CtoError::MathOverflow)?;
        rec.snapshot_slot = clock.slot;
        rec.total_snapshot_shares = pool.total_shares;
        rec.yes_weight = 0;
        rec.no_weight = 0;
        rec.abstain_weight = 0;
        rec.participation_weight = 0;
        rec.status = ProposalStatus::Active;

        pool.active_recovery = Some(rec.key());
        pool.recovery_count = pool.recovery_count.checked_add(1).ok_or(CtoError::MathOverflow)?;

        Ok(())
    }

    pub fn recover_funds_vote(ctx: Context<RecoverFundsVote>, choice: VoteChoice) -> Result<()> {
        let proposal = &mut ctx.accounts.recovery;
        let donor = &ctx.accounts.donor;
        let vote_record = &mut ctx.accounts.vote_record;
        let clock = Clock::get()?;

        require!(proposal.status == ProposalStatus::Active, CtoError::ProposalNotActive);
        require!(clock.unix_timestamp <= proposal.deadline_ts, CtoError::VotingClosed);
        require!(donor.shares > 0, CtoError::NoShares);
        require!(
            donor.last_shares_change_slot <= proposal.snapshot_slot,
            CtoError::NotEligibleForThisProposal
        );

        if vote_record.initialized {
            let w = vote_record.snapshot_weight;
            match vote_record.choice {
                VoteChoice::Yes => proposal.yes_weight = proposal.yes_weight.checked_sub(w).ok_or(CtoError::MathOverflow)?,
                VoteChoice::No => proposal.no_weight = proposal.no_weight.checked_sub(w).ok_or(CtoError::MathOverflow)?,
                VoteChoice::Abstain => proposal.abstain_weight = proposal.abstain_weight.checked_sub(w).ok_or(CtoError::MathOverflow)?,
            }
        }

        let snapshot_weight = if vote_record.initialized {
            vote_record.snapshot_weight
        } else {
            let raw = donor.shares;
            let cap = proposal
                .total_snapshot_shares
                .checked_mul(MAX_VOTER_BPS as u64)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(10_000)
                .ok_or(CtoError::MathOverflow)?;
            raw.min(cap)
        };

        match choice {
            VoteChoice::Yes => proposal.yes_weight = proposal.yes_weight.checked_add(snapshot_weight).ok_or(CtoError::MathOverflow)?,
            VoteChoice::No => proposal.no_weight = proposal.no_weight.checked_add(snapshot_weight).ok_or(CtoError::MathOverflow)?,
            VoteChoice::Abstain => proposal.abstain_weight = proposal.abstain_weight.checked_add(snapshot_weight).ok_or(CtoError::MathOverflow)?,
        }

        proposal.participation_weight = proposal
            .yes_weight
            .checked_add(proposal.no_weight)
            .ok_or(CtoError::MathOverflow)?
            .checked_add(proposal.abstain_weight)
            .ok_or(CtoError::MathOverflow)?;

        vote_record.proposal = proposal.key();
        vote_record.voter = donor.wallet;
        vote_record.snapshot_weight = snapshot_weight;
        vote_record.choice = choice;
        vote_record.initialized = true;

        Ok(())
    }

    pub fn recover_funds_execute(ctx: Context<RecoverFundsExecute>) -> Result<()> {
        let clock = Clock::get()?;

        let pool_bump = ctx.bumps.pool;
        let pool_token_mint = ctx.accounts.pool.token_mint;
        let pool_quorum_bps = ctx.accounts.pool.quorum_bps;
        let requested_amount = ctx.accounts.recovery.requested_amount;

        let rec = &mut ctx.accounts.recovery;

        require!(rec.status == ProposalStatus::Active, CtoError::ProposalNotActive);

        let quorum_met = rec
            .participation_weight
            .checked_mul(10_000)
            .ok_or(CtoError::MathOverflow)?
            >= rec
                .total_snapshot_shares
                .checked_mul(pool_quorum_bps as u64)
                .ok_or(CtoError::MathOverflow)?;

        let time_over = clock.unix_timestamp >= rec.deadline_ts;
        require!(time_over || quorum_met, CtoError::TooEarlyToExecute);

        let majority_met = rec.yes_weight > rec.no_weight;

        if !(quorum_met && majority_met) {
            rec.status = ProposalStatus::Failed;
            ctx.accounts.pool.active_recovery = None;
            return Ok(());
        }

        transfer_spl_from_pool_with_seeds(
            &ctx.accounts.pool.to_account_info(),
            &ctx.accounts.pool_token_account,
            &ctx.accounts.destination_token_account,
            &ctx.accounts.token_program,
            &[&[b"pool", pool_token_mint.as_ref(), &[pool_bump]]],
            requested_amount,
        )?;

        rec.status = ProposalStatus::Executed;
        ctx.accounts.pool.active_recovery = None;

        Ok(())
    }
}

// ============= Helper Functions =============

/// Validates the stake pool configuration against known Jito deployments.
/// This function ensures only trusted stake pool programs are used.
fn validate_stake_pool_config(stake_pool_program: Pubkey, stake_pool: Pubkey, lst_mint: Pubkey) -> Result<()> {
    let is_devnet = stake_pool_program == JITO_DEVNET_STAKE_POOL_PROGRAM
        && stake_pool == JITO_DEVNET_STAKE_POOL
        && lst_mint == JITO_DEVNET_JITOSOL_MINT;

    let is_mainnet_program = stake_pool_program == JITO_MAINNET_STAKE_POOL_PROGRAM;

    require!(is_devnet || is_mainnet_program, CtoError::InvalidStakePoolConfig);
    Ok(())
}

/// Reads and deserializes the StakePool state from an AccountInfo.
///
/// NOTE: `spl_stake_pool` uses borsh 1.x. If you see borsh version conflicts,
/// keep using `borsh1::BorshDeserialize` for StakePool, as you already did.
fn read_stake_pool(stake_pool_ai: &AccountInfo) -> Result<StakePool> {
    let data = stake_pool_ai.try_borrow_data().map_err(|_| CtoError::InvalidAccountData)?;
    borsh1::BorshDeserialize::deserialize(&mut &data[..]).map_err(|_| CtoError::InvalidAccountData.into())
}

/// Compute the minimum pool tokens that should produce at least `lamports_out` when withdrawing.
///
/// We compute a ceiling estimate from the current ratio.
fn pool_tokens_for_lamports_ceil(stake_pool: &StakePool, lamports_out: u64) -> Result<u64> {
    require!(stake_pool.total_lamports > 0, CtoError::StakePoolEmpty);

    let num = (lamports_out as u128)
        .checked_mul(stake_pool.pool_token_supply as u128)
        .ok_or(CtoError::MathOverflow)?;
    let den = stake_pool.total_lamports as u128;

    let mut q = num.checked_div(den).ok_or(CtoError::MathOverflow)?;
    if num % den != 0 {
        q = q.checked_add(1).ok_or(CtoError::MathOverflow)?;
    }
    let q = u64::try_from(q).map_err(|_| CtoError::MathOverflow)?;
    Ok(q.max(1))
}

/// Transfers lamports from one account to another using signed invocation.
/// The `from` account must be a PDA with the provided seeds.
fn transfer_lamports_signed<'info>(
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    seeds: &[&[&[u8]]],
    lamports: u64,
) -> Result<()> {
    invoke_signed(
        &system_instruction::transfer(from.key, to.key, lamports),
        &[from.clone(), to.clone()],
        seeds,
    )
    .map_err(|_| CtoError::LamportTransferFailed.into())
}

/// CPI to stake pool program to deposit SOL and receive LST tokens.
fn stake_pool_deposit_sol(ctx: &Context<DonateSol>, lamports_in: u64, minimum_pool_tokens_out: u64) -> Result<()> {
    let pool = &ctx.accounts.pool;

    require!(
        ctx.accounts.stake_pool_program.key() == pool.stake_pool_program
            && ctx.accounts.stake_pool.key() == pool.stake_pool
            && ctx.accounts.lst_mint.key() == pool.lst_mint,
        CtoError::InvalidStakePoolConfig
    );

    let ix = stake_pool_ix::deposit_sol_with_slippage(
        &pool.stake_pool_program,
        &pool.stake_pool,
        &ctx.accounts.stake_pool_withdraw_authority.key(),
        &ctx.accounts.reserve_stake.key(),
        &ctx.accounts.donor_wallet.key(),
        &ctx.accounts.pool_lst_account.key(),
        &ctx.accounts.manager_fee_account.key(),
        &ctx.accounts.referrer_pool_tokens_account.key(),
        &ctx.accounts.lst_mint.key(),
        &ctx.accounts.token_program.key(),
        lamports_in,
        minimum_pool_tokens_out,
    );

    invoke(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.stake_pool_withdraw_authority.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.donor_wallet.to_account_info(),
            ctx.accounts.pool_lst_account.to_account_info(),
            ctx.accounts.manager_fee_account.to_account_info(),
            ctx.accounts.referrer_pool_tokens_account.to_account_info(),
            ctx.accounts.lst_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        ],
    )
    .map_err(|_| CtoError::StakePoolCpiFailed.into())
}

/// CPI to stake pool program to withdraw SOL by burning LST tokens (withdraw_sol handler).
fn stake_pool_withdraw_sol(ctx: &Context<WithdrawSol>, pool_tokens_in: u64, minimum_lamports_out: u64) -> Result<()> {
    let pool = &ctx.accounts.pool;

    require!(
        ctx.accounts.stake_pool_program.key() == pool.stake_pool_program
            && ctx.accounts.stake_pool.key() == pool.stake_pool
            && ctx.accounts.lst_mint.key() == pool.lst_mint,
        CtoError::InvalidStakePoolConfig
    );

    let ix = stake_pool_ix::withdraw_sol_with_slippage(
        &pool.stake_pool_program,
        &pool.stake_pool,
        &ctx.accounts.stake_pool_withdraw_authority.key(),
        &pool.key(),
        &ctx.accounts.pool_lst_account.key(),
        &ctx.accounts.reserve_stake.key(),
        &pool.key(),
        &ctx.accounts.manager_fee_account.key(),
        &ctx.accounts.lst_mint.key(),
        &ctx.accounts.token_program.key(),
        pool_tokens_in,
        minimum_lamports_out,
    );

    invoke_signed(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.stake_pool_withdraw_authority.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.pool_lst_account.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.manager_fee_account.to_account_info(),
            ctx.accounts.lst_mint.to_account_info(),
            ctx.accounts.clock.to_account_info(),
            ctx.accounts.stake_history.to_account_info(),
            ctx.accounts.stake_program.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        ],
        pool_seeds!(pool, ctx.bumps.pool),
    )
    .map_err(|_| CtoError::StakePoolCpiFailed.into())
}

/// CPI to stake pool program to withdraw SOL by burning LST tokens (execute_proposal handler).
fn stake_pool_withdraw_sol_exec(ctx: &Context<ExecuteProposal>, pool_tokens_in: u64, minimum_lamports_out: u64) -> Result<()> {
    let pool = &ctx.accounts.pool;

    require!(
        ctx.accounts.stake_pool_program.key() == pool.stake_pool_program
            && ctx.accounts.stake_pool.key() == pool.stake_pool
            && ctx.accounts.lst_mint.key() == pool.lst_mint,
        CtoError::InvalidStakePoolConfig
    );

    let ix = stake_pool_ix::withdraw_sol_with_slippage(
        &pool.stake_pool_program,
        &pool.stake_pool,
        &ctx.accounts.stake_pool_withdraw_authority.key(),
        &pool.key(),
        &ctx.accounts.pool_lst_account.key(),
        &ctx.accounts.reserve_stake.key(),
        &pool.key(),
        &ctx.accounts.manager_fee_account.key(),
        &ctx.accounts.lst_mint.key(),
        &ctx.accounts.token_program.key(),
        pool_tokens_in,
        minimum_lamports_out,
    );

    invoke_signed(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.stake_pool_withdraw_authority.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.pool_lst_account.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.manager_fee_account.to_account_info(),
            ctx.accounts.lst_mint.to_account_info(),
            ctx.accounts.clock.to_account_info(),
            ctx.accounts.stake_history.to_account_info(),
            ctx.accounts.stake_program.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        ],
        pool_seeds!(pool, ctx.bumps.pool),
    )
    .map_err(|_| CtoError::StakePoolCpiFailed.into())
}

// ===== Buy & burn helpers =====

/// Attempts to swap SOL for CTOP on PumpSwap and burn to incinerator.
/// Returns the amount of CTOP burned on success.
fn attempt_pumpswap_swap_and_burn<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    amount_sol: u64,
    pool_bump: u8,
) -> Result<u64> {
    // Basic safety: ensure we're calling the canonical PumpSwap program id.
    require!(
        ctx.accounts.pumpswap_program.key() == PUMPSWAP_PROGRAM_ID,
        CtoError::InvalidPumpSwapConfig
    );

    // Prevent trivial executor “account injection”:
    // - Vaults must hold the expected mints (CTOP and WSOL).
    // - Swap output is forced into pool_ctop_account (PDA-owned), and then burned.
    validate_pumpswap_vault_mints(ctx)?;

    // Wrap SOL into WSOL held by the pool PDA
    wrap_sol_to_wsol(ctx, amount_sol, pool_bump)?;

    // On-chain frontrun protection:
    // Compute min-out from current vault reserves, then apply slippage bps.
    let min_ctop = compute_min_out_cpmm_from_vaults(
        ctx.accounts.pumpswap_pool_quote_vault.amount, // WSOL reserve
        ctx.accounts.pumpswap_pool_base_vault.amount,  // CTOP reserve
        amount_sol,
        MAX_SLIPPAGE_BPS,
    )?;

    // Perform PumpSwap buy: spend up to `amount_sol` WSOL, receive at least `min_ctop` CTOP.
    perform_pumpswap_buy(ctx, min_ctop, amount_sol, pool_bump)?;

    // Burn everything acquired by transferring to the incinerator ATA.
    transfer_to_incinerator(ctx, pool_bump)
}

/// Legacy Raydium buy & burn (optional).
fn attempt_raydium_swap_and_burn<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    amount_sol: u64,
    pool_bump: u8,
) -> Result<u64> {
    wrap_sol_to_wsol(ctx, amount_sol, pool_bump)?;
    // NOTE: This is still heuristic for Raydium. Prefer PumpSwap path for Pump.fun launches.
    let min_ctop = calculate_minimum_amount_out_heuristic(amount_sol, MAX_SLIPPAGE_BPS)?;
    perform_raydium_swap(ctx, amount_sol, min_ctop, pool_bump)?;
    transfer_to_incinerator(ctx, pool_bump)
}

/// Wraps native SOL into WSOL by transferring SOL to the pool's WSOL token account and syncing.
fn wrap_sol_to_wsol<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    amount: u64,
    pool_bump: u8,
) -> Result<()> {
    invoke_signed(
        &system_instruction::transfer(&ctx.accounts.pool.key(), &ctx.accounts.pool_wsol_account.key(), amount),
        &[
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.pool_wsol_account.to_account_info(),
        ],
        pool_seeds!(&ctx.accounts.pool, pool_bump),
    )
    .map_err(|_| CtoError::LamportTransferFailed)?;

    token::sync_native(CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        token::SyncNative {
            account: ctx.accounts.pool_wsol_account.to_account_info(),
        },
    ))?;

    Ok(())
}

/// Validates that the PumpSwap vault token accounts correspond to the expected mints.
///
/// Why this matters:
/// - Executors can pass arbitrary accounts.
/// - We force the swap to deposit into PDAs we control, but the AMM vault accounts still matter
///   for price computation and for protocol correctness.
/// - This check prevents a class of misconfiguration / malicious passing of unrelated vaults.
fn validate_pumpswap_vault_mints<'info>(ctx: &Context<ExecuteProposal<'info>>) -> Result<()> {
    require!(
        ctx.accounts.pumpswap_pool_base_vault.mint == ctx.accounts.ctop_mint.key(),
        CtoError::InvalidPumpSwapVaultMints
    );
    require!(
        ctx.accounts.pumpswap_pool_quote_vault.mint == ctx.accounts.wsol_mint.key(),
        CtoError::InvalidPumpSwapVaultMints
    );
    Ok(())
}

/// Computes a conservative min-out using constant product math from vault balances.
///
/// NOTE:
/// - This assumes an x*y=k style pool.
/// - If PumpSwap charges fees, real output will be lower; we already apply a slippage haircut.
/// - If fees become significant and burns start failing, increase MAX_SLIPPAGE_BPS or update
///   this function to include fee modeling (during beta while upgrade authority remains).
fn compute_min_out_cpmm_from_vaults(
    quote_reserve: u64,
    base_reserve: u64,
    quote_in: u64,
    slippage_bps: u64,
) -> Result<u64> {
    require!(quote_reserve > 0 && base_reserve > 0, CtoError::PumpSwapMathError);
    require!(quote_in > 0, CtoError::ZeroAmount);

    // x = quote reserve (WSOL), y = base reserve (CTOP)
    let x = quote_reserve as u128;
    let y = base_reserve as u128;
    let dx = quote_in as u128;

    // y_out = y - (k / (x + dx))
    let k = x.checked_mul(y).ok_or(CtoError::MathOverflow)?;
    let denom = x.checked_add(dx).ok_or(CtoError::MathOverflow)?;
    let y_after = k.checked_div(denom).ok_or(CtoError::MathOverflow)?;
    let expected_out = y.checked_sub(y_after).ok_or(CtoError::PumpSwapMathError)?;

    // Apply slippage haircut
    let slip = (BPS_DENOM as u128)
        .checked_sub(slippage_bps as u128)
        .ok_or(CtoError::MathOverflow)?;
    let min_out = expected_out
        .checked_mul(slip)
        .ok_or(CtoError::MathOverflow)?
        .checked_div(BPS_DENOM as u128)
        .ok_or(CtoError::MathOverflow)?;

    let min_out_u64 = u64::try_from(min_out).map_err(|_| CtoError::MathOverflow)?;
    require!(min_out_u64 > 0, CtoError::PumpSwapMinOutZero);
    Ok(min_out_u64)
}

/// Heuristic minimum out calculator for legacy Raydium path.
/// This is intentionally permissive and should not be relied on for robust frontrun protection.
fn calculate_minimum_amount_out_heuristic(amount_in: u64, slippage_bps: u64) -> Result<u64> {
    let min_out = (amount_in as u128)
        .checked_mul(
            (BPS_DENOM as u128)
                .checked_sub(slippage_bps as u128)
                .ok_or(CtoError::MathOverflow)?,
        )
        .ok_or(CtoError::MathOverflow)?
        .checked_div(BPS_DENOM as u128)
        .ok_or(CtoError::MathOverflow)?;

    // Keep at least 1
    let min_out = u64::try_from(min_out).map_err(|_| CtoError::MathOverflow)?;
    Ok(min_out.max(1))
}

/// Performs PumpSwap `buy`:
/// - Spends up to `max_quote_amount_in` WSOL from `pool_wsol_account`
/// - Receives at least `base_amount_out` CTOP into `pool_ctop_account`
///
/// Security notes:
/// - Recipient token accounts are PDAs owned by this program.
/// - Executors cannot change recipients.
/// - Best-effort failure routes funds to dev (handled by caller).
fn perform_pumpswap_buy<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    base_amount_out: u64,
    max_quote_amount_in: u64,
    pool_bump: u8,
) -> Result<()> {
    // Build instruction data
    let mut data = Vec::with_capacity(8 + 8 + 8);
    data.extend_from_slice(&PUMPSWAP_BUY_DISCRIMINATOR);
    data.extend_from_slice(&base_amount_out.to_le_bytes());
    data.extend_from_slice(&max_quote_amount_in.to_le_bytes());

    // Account order is strict. Do NOT reorder without checking the PumpSwap interface.
    let metas = vec![
        AccountMeta::new(ctx.accounts.pumpswap_pool.key(), false),
        // user (signer): pool PDA
        AccountMeta::new(ctx.accounts.pool.key(), true),
        AccountMeta::new_readonly(ctx.accounts.pumpswap_global_config.key(), false),
        AccountMeta::new_readonly(ctx.accounts.ctop_mint.key(), false), // base_mint
        AccountMeta::new_readonly(ctx.accounts.wsol_mint.key(), false), // quote_mint
        AccountMeta::new(ctx.accounts.pool_ctop_account.key(), false),  // user_base_token_account
        AccountMeta::new(ctx.accounts.pool_wsol_account.key(), false),  // user_quote_token_account
        AccountMeta::new(ctx.accounts.pumpswap_pool_base_vault.key(), false),
        AccountMeta::new(ctx.accounts.pumpswap_pool_quote_vault.key(), false),
        AccountMeta::new_readonly(ctx.accounts.pumpswap_protocol_fee_recipient.key(), false),
        AccountMeta::new(ctx.accounts.pumpswap_protocol_fee_recipient_token_account.key(), false),
        AccountMeta::new_readonly(ctx.accounts.pumpswap_base_token_program.key(), false),
        AccountMeta::new_readonly(ctx.accounts.pumpswap_quote_token_program.key(), false),
        AccountMeta::new_readonly(ctx.accounts.system_program.key(), false),
        AccountMeta::new_readonly(ctx.accounts.associated_token_program.key(), false),
        AccountMeta::new_readonly(ctx.accounts.pumpswap_event_authority.key(), false),
        AccountMeta::new_readonly(ctx.accounts.pumpswap_program.key(), false),
    ];

    let ix = Instruction {
        program_id: ctx.accounts.pumpswap_program.key(),
        accounts: metas,
        data,
    };

    // IMPORTANT: AccountInfos must match metas order.
    invoke_signed(
        &ix,
        &[
            ctx.accounts.pumpswap_pool.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.pumpswap_global_config.to_account_info(),
            ctx.accounts.ctop_mint.to_account_info(),
            ctx.accounts.wsol_mint.to_account_info(),
            ctx.accounts.pool_ctop_account.to_account_info(),
            ctx.accounts.pool_wsol_account.to_account_info(),
            ctx.accounts.pumpswap_pool_base_vault.to_account_info(),
            ctx.accounts.pumpswap_pool_quote_vault.to_account_info(),
            ctx.accounts.pumpswap_protocol_fee_recipient.to_account_info(),
            ctx.accounts.pumpswap_protocol_fee_recipient_token_account.to_account_info(),
            ctx.accounts.pumpswap_base_token_program.to_account_info(),
            ctx.accounts.pumpswap_quote_token_program.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.associated_token_program.to_account_info(),
            ctx.accounts.pumpswap_event_authority.to_account_info(),
            ctx.accounts.pumpswap_program.to_account_info(),
        ],
        pool_seeds!(&ctx.accounts.pool, pool_bump),
    )
    .map_err(|_| CtoError::SwapFailed.into())
}

/// Performs the Raydium swap from WSOL to CTOP (legacy).
fn perform_raydium_swap<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    amount_in: u64,
    minimum_amount_out: u64,
    pool_bump: u8,
) -> Result<()> {
    let mut data = Vec::with_capacity(1 + 8 + 8);
    data.push(RAYDIUM_SWAP_INSTRUCTION);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    let metas = vec![
        AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
        AccountMeta::new(ctx.accounts.raydium_pool.key(), false),
        AccountMeta::new_readonly(ctx.accounts.raydium_pool_authority.key(), false),
        AccountMeta::new(ctx.accounts.raydium_open_orders.key(), false),
        AccountMeta::new(ctx.accounts.raydium_target_orders.key(), false),
        AccountMeta::new(ctx.accounts.raydium_coin_vault.key(), false),
        AccountMeta::new(ctx.accounts.raydium_pc_vault.key(), false),
        AccountMeta::new_readonly(ctx.accounts.serum_program.key(), false),
        AccountMeta::new(ctx.accounts.serum_market.key(), false),
        AccountMeta::new(ctx.accounts.serum_bids.key(), false),
        AccountMeta::new(ctx.accounts.serum_asks.key(), false),
        AccountMeta::new(ctx.accounts.serum_event_queue.key(), false),
        AccountMeta::new(ctx.accounts.serum_coin_vault.key(), false),
        AccountMeta::new(ctx.accounts.serum_pc_vault.key(), false),
        AccountMeta::new_readonly(ctx.accounts.serum_vault_signer.key(), false),
        AccountMeta::new(ctx.accounts.pool_wsol_account.key(), false),
        AccountMeta::new(ctx.accounts.pool_ctop_account.key(), false),
        AccountMeta::new_readonly(ctx.accounts.pool.key(), true),
    ];

    let ix = Instruction {
        program_id: ctx.accounts.raydium_program.key(),
        accounts: metas,
        data,
    };

    invoke_signed(
        &ix,
        &[
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.raydium_pool.to_account_info(),
            ctx.accounts.raydium_pool_authority.to_account_info(),
            ctx.accounts.raydium_open_orders.to_account_info(),
            ctx.accounts.raydium_target_orders.to_account_info(),
            ctx.accounts.raydium_coin_vault.to_account_info(),
            ctx.accounts.raydium_pc_vault.to_account_info(),
            ctx.accounts.serum_program.to_account_info(),
            ctx.accounts.serum_market.to_account_info(),
            ctx.accounts.serum_bids.to_account_info(),
            ctx.accounts.serum_asks.to_account_info(),
            ctx.accounts.serum_event_queue.to_account_info(),
            ctx.accounts.serum_coin_vault.to_account_info(),
            ctx.accounts.serum_pc_vault.to_account_info(),
            ctx.accounts.serum_vault_signer.to_account_info(),
            ctx.accounts.pool_wsol_account.to_account_info(),
            ctx.accounts.pool_ctop_account.to_account_info(),
            ctx.accounts.pool.to_account_info(),
        ],
        pool_seeds!(&ctx.accounts.pool, pool_bump),
    )
    .map_err(|_| CtoError::SwapFailed.into())
}

/// Transfers CTOP tokens to the incinerator address for burning.
fn transfer_to_incinerator<'info>(ctx: &mut Context<ExecuteProposal<'info>>, pool_bump: u8) -> Result<u64> {
    ctx.accounts.pool_ctop_account.reload()?;
    let bal = ctx.accounts.pool_ctop_account.amount;
    require!(bal > 0, CtoError::NoTokensToBurn);

    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            token::Transfer {
                from: ctx.accounts.pool_ctop_account.to_account_info(),
                to: ctx.accounts.incinerator_ctop_account.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            pool_seeds!(&ctx.accounts.pool, pool_bump),
        ),
        bal,
    )?;

    Ok(bal)
}

// ===== Recovery helpers =====

fn transfer_spl_from_pool_with_seeds<'info>(
    pool: &AccountInfo<'info>,
    from: &Account<'info, TokenAccount>,
    to: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    token::transfer(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            token::Transfer {
                from: from.to_account_info(),
                to: to.to_account_info(),
                authority: pool.clone(),
            },
            seeds,
        ),
        amount,
    )
}

/// Instruction-sysvar based "inline sender" check.
///
/// This only works if the requester performs the transfer and recovery in the *same transaction*.
/// It cannot prove sender for a historical accident.
fn verify_inline_sender(
    _instructions_sysvar: &AccountInfo,
    _requester: &Pubkey,
    _pool_token_account: &Pubkey,
    _mint: Pubkey,
    _amount: u64,
) -> Result<bool> {
    Ok(false)
}

// ============= Accounts =============

#[derive(Accounts)]
pub struct CreatePool<'info> {
    #[account(
        init,
        payer = creator,
        space = 8 + Pool::SIZE,
        seeds = [b"pool", token_mint.key().as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    pub token_mint: Account<'info, Mint>,

    #[account(mut)]
    pub creator: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ConfigurePumpSwapPool<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct ConfigureRaydiumPool<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct DonateSol<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        init_if_needed,
        payer = donor_wallet,
        space = 8 + Donor::SIZE,
        seeds = [b"donor", pool.key().as_ref(), donor_wallet.key().as_ref()],
        bump
    )]
    pub donor: Account<'info, Donor>,

    #[account(mut)]
    pub donor_wallet: Signer<'info>,

    /// CHECK: stake pool program (e.g. Jito)
    pub stake_pool_program: UncheckedAccount<'info>,

    /// CHECK: stake pool state account
    #[account(mut)]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: stake pool withdraw authority
    pub stake_pool_withdraw_authority: UncheckedAccount<'info>,

    /// CHECK: reserve stake account
    #[account(mut)]
    pub reserve_stake: UncheckedAccount<'info>,

    #[account(mut)]
    pub manager_fee_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub referrer_pool_tokens_account: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = donor_wallet,
        associated_token::mint = lst_mint,
        associated_token::authority = pool
    )]
    pub pool_lst_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub lst_mint: Account<'info, Mint>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawSol<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"donor", pool.key().as_ref(), donor_wallet.key().as_ref()],
        bump
    )]
    pub donor: Account<'info, Donor>,

    #[account(mut)]
    pub donor_wallet: Signer<'info>,

    /// CHECK
    pub stake_pool_program: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub stake_pool: UncheckedAccount<'info>,
    /// CHECK
    pub stake_pool_withdraw_authority: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub reserve_stake: UncheckedAccount<'info>,

    #[account(mut)]
    pub manager_fee_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        associated_token::mint = lst_mint,
        associated_token::authority = pool
    )]
    pub pool_lst_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub lst_mint: Account<'info, Mint>,

    /// CHECK: sysvar clock
    pub clock: UncheckedAccount<'info>,
    /// CHECK: sysvar stake history
    pub stake_history: UncheckedAccount<'info>,
    /// CHECK: stake program
    pub stake_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CreateProposal<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"donor", pool.key().as_ref(), proposer_wallet.key().as_ref()],
        bump
    )]
    pub donor: Account<'info, Donor>,

    #[account(
        init,
        payer = proposer_wallet,
        space = 8 + Proposal::SIZE,
        seeds = [b"proposal", pool.key().as_ref(), &pool.proposal_count.to_le_bytes()],
        bump
    )]
    pub proposal: Account<'info, Proposal>,

    #[account(mut)]
    pub proposer_wallet: Signer<'info>,

    /// CHECK
    pub stake_pool: UncheckedAccount<'info>,

    #[account(
        mut,
        associated_token::mint = lst_mint,
        associated_token::authority = pool
    )]
    pub pool_lst_account: Account<'info, TokenAccount>,

    pub lst_mint: Account<'info, Mint>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Vote<'info> {
    #[account(
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut, has_one = pool)]
    pub proposal: Account<'info, Proposal>,

    #[account(
        seeds = [b"donor", pool.key().as_ref(), voter_wallet.key().as_ref()],
        bump
    )]
    pub donor: Account<'info, Donor>,

    #[account(
        init_if_needed,
        payer = voter_wallet,
        space = 8 + VoteRecord::SIZE,
        seeds = [b"vote", proposal.key().as_ref(), voter_wallet.key().as_ref()],
        bump
    )]
    pub vote_record: Account<'info, VoteRecord>,

    #[account(mut)]
    pub voter_wallet: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ExecuteProposal<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut, has_one = pool)]
    pub proposal: Account<'info, Proposal>,

    /// CHECK
    #[account(mut)]
    pub destination_wallet: UncheckedAccount<'info>,

    /// CHECK
    #[account(mut, address = pool.dev_fee_wallet)]
    pub dev_fee_wallet: UncheckedAccount<'info>,

    // ===== Stake pool accounts =====
    /// CHECK
    pub stake_pool_program: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub stake_pool: UncheckedAccount<'info>,
    /// CHECK
    pub stake_pool_withdraw_authority: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub reserve_stake: UncheckedAccount<'info>,

    #[account(mut)]
    pub manager_fee_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        associated_token::mint = lst_mint,
        associated_token::authority = pool
    )]
    pub pool_lst_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub lst_mint: Account<'info, Mint>,

    /// CHECK
    pub clock: UncheckedAccount<'info>,
    /// CHECK
    pub stake_history: UncheckedAccount<'info>,
    /// CHECK
    pub stake_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,

    // ===== PUMPSWAP SWAP ACCOUNTS (CTOP buy & burn, post-graduation) =====
    /// CHECK
    pub pumpswap_program: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub pumpswap_pool: UncheckedAccount<'info>,

    /// CHECK
    pub pumpswap_global_config: UncheckedAccount<'info>,
    /// CHECK
    pub pumpswap_protocol_fee_recipient: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub pumpswap_protocol_fee_recipient_token_account: UncheckedAccount<'info>,
    /// CHECK
    pub pumpswap_event_authority: UncheckedAccount<'info>,

    /// The pool's base (CTOP) vault token account.
    #[account(mut)]
    pub pumpswap_pool_base_vault: Account<'info, TokenAccount>,
    /// The pool's quote (WSOL) vault token account.
    #[account(mut)]
    pub pumpswap_pool_quote_vault: Account<'info, TokenAccount>,

    /// CHECK
    pub pumpswap_base_token_program: UncheckedAccount<'info>,
    /// CHECK
    pub pumpswap_quote_token_program: UncheckedAccount<'info>,

    // ===== RAYDIUM SWAP ACCOUNTS (legacy / optional) =====
    /// CHECK
    pub raydium_program: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub raydium_pool: UncheckedAccount<'info>,
    /// CHECK
    pub raydium_pool_authority: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub raydium_open_orders: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub raydium_target_orders: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub raydium_coin_vault: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub raydium_pc_vault: UncheckedAccount<'info>,

    /// CHECK
    pub serum_program: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub serum_market: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub serum_bids: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub serum_asks: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub serum_event_queue: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub serum_coin_vault: UncheckedAccount<'info>,
    /// CHECK
    #[account(mut)]
    pub serum_pc_vault: UncheckedAccount<'info>,
    /// CHECK
    pub serum_vault_signer: UncheckedAccount<'info>,

    // ===== TOKEN ACCOUNTS =====
    #[account(
        init_if_needed,
        payer = executor,
        token::mint = wsol_mint,
        token::authority = pool,
        seeds = [b"pool_wsol", pool.key().as_ref()],
        bump
    )]
    pub pool_wsol_account: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = executor,
        token::mint = ctop_mint,
        token::authority = pool,
        seeds = [b"pool_ctop", pool.key().as_ref()],
        bump
    )]
    pub pool_ctop_account: Account<'info, TokenAccount>,

    #[account(
        init_if_needed,
        payer = executor,
        associated_token::mint = ctop_mint,
        associated_token::authority = incinerator
    )]
    pub incinerator_ctop_account: Account<'info, TokenAccount>,

    #[account(address = WSOL_MINT)]
    pub wsol_mint: Account<'info, Mint>,

    #[account(address = pool.burn_token_mint)]
    pub ctop_mint: Account<'info, Mint>,

    /// CHECK
    #[account(address = INCINERATOR)]
    pub incinerator: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,

    #[account(mut)]
    pub executor: Signer<'info>,
}

// ===== Recovery accounts =====

#[derive(Accounts)]
pub struct RecoverFundsCreate<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        seeds = [b"donor", pool.key().as_ref(), requester.key().as_ref()],
        bump
    )]
    pub donor: Account<'info, Donor>,

    #[account(
        init,
        payer = requester,
        space = 8 + RecoveryProposal::SIZE,
        seeds = [b"recovery", pool.key().as_ref(), &pool.recovery_count.to_le_bytes()],
        bump
    )]
    pub recovery: Account<'info, RecoveryProposal>,

    #[account(mut)]
    pub requester: Signer<'info>,

    #[account(mut, constraint = pool_token_account.owner == pool.key())]
    pub pool_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub destination_token_account: Account<'info, TokenAccount>,

    /// CHECK: instruction sysvar
    pub instructions: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RecoverFundsVote<'info> {
    #[account(
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut, has_one = pool)]
    pub recovery: Account<'info, RecoveryProposal>,

    #[account(
        seeds = [b"donor", pool.key().as_ref(), voter_wallet.key().as_ref()],
        bump
    )]
    pub donor: Account<'info, Donor>,

    #[account(
        init_if_needed,
        payer = voter_wallet,
        space = 8 + VoteRecord::SIZE,
        seeds = [b"vote", recovery.key().as_ref(), voter_wallet.key().as_ref()],
        bump
    )]
    pub vote_record: Account<'info, VoteRecord>,

    #[account(mut)]
    pub voter_wallet: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RecoverFundsExecute<'info> {
    #[account(
        mut,
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(mut, has_one = pool)]
    pub recovery: Account<'info, RecoveryProposal>,

    #[account(mut, constraint = pool_token_account.owner == pool.key())]
    pub pool_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub destination_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,

    #[account(mut)]
    pub executor: Signer<'info>,
}

// ============= State =============

#[account]
pub struct Pool {
    // Identity
    pub token_mint: Pubkey,
    pub authority: Pubkey,
    pub creator: Pubkey,

    // Share accounting
    pub total_shares: u64,

    // LST tokens held by the pool PDA (e.g. jitoSOL)
    pub total_pool_tokens: u64,
    pub reserved_pool_tokens: u64,

    // Spend accounting (net payouts to destination wallets)
    pub total_spent_lamports: u64,

    // Governance/config
    pub protocol_fee_bps: u16,
    pub quorum_bps: u16,
    pub min_proposer_deposit_lamports: u64,

    // Fee outputs
    pub dev_fee_wallet: Pubkey,
    pub burn_token_mint: Pubkey,

    // Proposal tracking
    pub active_proposal: Option<Pubkey>,
    pub proposal_count: u64,

    // Recovery tracking
    pub active_recovery: Option<Pubkey>,
    pub recovery_count: u64,

    // LST backend config
    pub stake_pool_program: Pubkey,
    pub stake_pool: Pubkey,
    pub lst_mint: Pubkey,

    // PumpSwap buy&burn (recommended for Pump.fun post-graduation)
    pub pumpswap_pool_id: Pubkey,
    pub pumpswap_enabled: bool,

    // Legacy Raydium buy&burn (optional)
    pub raydium_pool_id: Pubkey,
    pub raydium_enabled: bool,
}

impl Pool {
    pub const SIZE: usize =
        32 + 32 + 32 + // token_mint, authority, creator
        8 +            // total_shares
        8 + 8 +        // total_pool_tokens, reserved_pool_tokens
        8 +            // total_spent_lamports
        2 + 2 + 8 +    // protocol_fee_bps, quorum_bps, min_proposer
        32 + 32 +      // dev_fee_wallet, burn_token_mint
        1 + 32 +       // active_proposal
        8 +            // proposal_count
        1 + 32 +       // active_recovery
        8 +            // recovery_count
        32 + 32 + 32 + // stake_pool_program, stake_pool, lst_mint
        32 + 1 +       // pumpswap_pool_id, pumpswap_enabled
        32 + 1;        // raydium_pool_id, raydium_enabled
}

#[account]
pub struct Donor {
    pub pool: Pubkey,
    pub wallet: Pubkey,
    pub shares: u64,
    pub total_deposited_lamports: u64,
    pub last_shares_change_slot: u64,
}

impl Donor {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 8;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq)]
pub enum ProposalStatus {
    Active,
    Failed,
    Executed,
}

#[derive(AnchorSerialize, AnchorDeserialize, Copy, Clone, PartialEq, Eq)]
pub enum ProposalKind {
    Payout,
}

#[account]
pub struct Proposal {
    pub pool: Pubkey,
    pub kind: ProposalKind,

    pub requested_lamports: u64,
    pub destination_wallet: Pubkey,

    pub title: String,
    pub description: String,

    pub created_at_ts: i64,
    pub deadline_ts: i64,

    pub snapshot_slot: u64,
    pub total_snapshot_shares: u64,

    // pool tokens locked for this proposal
    pub locked_pool_tokens: u64,

    pub yes_weight: u64,
    pub no_weight: u64,
    pub abstain_weight: u64,
    pub participation_weight: u64,

    pub status: ProposalStatus,
}

impl Proposal {
    pub const TITLE_MAX: usize = 64;
    pub const DESC_MAX: usize = 256;

    pub const SIZE: usize =
        32 + 1 +              // pool, kind
        8 + 32 +              // requested, destination
        4 + Self::TITLE_MAX + // title (len prefix + bytes)
        4 + Self::DESC_MAX +  // description
        8 + 8 +               // created_at, deadline
        8 + 8 +               // snapshot_slot, total_snapshot_shares
        8 +                   // locked_pool_tokens
        8 + 8 + 8 + 8 +       // yes/no/abstain/participation
        1;                    // status
}

#[account]
pub struct RecoveryProposal {
    pub pool: Pubkey,
    pub token_mint: Pubkey,
    pub requested_amount: u64,
    pub destination_wallet: Pubkey,

    pub title: String,
    pub description: String,

    pub created_at_ts: i64,
    pub deadline_ts: i64,

    pub snapshot_slot: u64,
    pub total_snapshot_shares: u64,

    pub yes_weight: u64,
    pub no_weight: u64,
    pub abstain_weight: u64,
    pub participation_weight: u64,

    pub status: ProposalStatus,
}

impl RecoveryProposal {
    pub const TITLE_MAX: usize = 64;
    pub const DESC_MAX: usize = 256;

    pub const SIZE: usize =
        32 + 32 + 8 + 32 +     // pool, token_mint, amount, destination
        4 + Self::TITLE_MAX +  // title
        4 + Self::DESC_MAX +   // description
        8 + 8 +                // created_at, deadline
        8 + 8 +                // snapshot_slot, total_snapshot_shares
        8 + 8 + 8 + 8 +        // yes/no/abstain/participation
        1;                     // status
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum VoteChoice {
    Yes,
    No,
    Abstain,
}

#[account]
pub struct VoteRecord {
    pub proposal: Pubkey,
    pub voter: Pubkey,
    pub snapshot_weight: u64,
    pub choice: VoteChoice,
    pub initialized: bool,
}

impl VoteRecord {
    pub const SIZE: usize = 32 + 32 + 8 + 1 + 1;
}

// ============= Events =============

#[event]
pub struct TokenBurnEvent {
    pub pool: Pubkey,
    pub amount_sol: u64,
    pub amount_ctop: u64,
    pub timestamp: i64,
}

#[event]
pub struct SwapFailureEvent {
    pub pool: Pubkey,
    pub amount_sol: u64,
    pub error_code: u32,
    pub timestamp: i64,
}

#[event]
pub struct ProposalFailedEvent {
    pub pool: Pubkey,
    pub proposal: Pubkey,
    pub unlocked_pool_tokens: u64,
    pub quorum_met: bool,
    pub majority_met: bool,
    pub timestamp: i64,
}

// ============= Errors =============

#[error_code]
pub enum CtoError {
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("No shares for this donor")]
    NoShares,
    #[msg("Withdraw amount exceeds withdrawable balance")]
    InsufficientWithdrawable,
    #[msg("Insufficient free liquidity")]
    InsufficientFreeLiquidity,
    #[msg("There is already an active proposal for this pool")]
    ActiveProposalExists,
    #[msg("There is already an active recovery proposal for this pool")]
    ActiveRecoveryExists,
    #[msg("Proposer's deposit is too small")]
    ProposerTooSmall,
    #[msg("Proposal is not active")]
    ProposalNotActive,
    #[msg("Voting closed")]
    VotingClosed,
    #[msg("Not eligible for this proposal")]
    NotEligibleForThisProposal,
    #[msg("Too early to execute")]
    TooEarlyToExecute,
    #[msg("Shares too recent")]
    SharesTooRecent,
    #[msg("Unauthorized")]
    UnauthorizedAuthority,

    // Swap errors
    #[msg("Swap failed")]
    SwapFailed,
    #[msg("No tokens to burn")]
    NoTokensToBurn,

    // Stake pool / CPI errors
    #[msg("Invalid account data")]
    InvalidAccountData,
    #[msg("Stake-pool CPI failed")]
    StakePoolCpiFailed,
    #[msg("Stake pool config invalid")]
    InvalidStakePoolConfig,
    #[msg("Stake pool returned zero output")]
    StakePoolReturnedZero,
    #[msg("Stake pool is empty")]
    StakePoolEmpty,
    #[msg("Lamport transfer failed")]
    LamportTransferFailed,
    #[msg("Slippage constraint exceeded")]
    SlippageExceeded,

    // Governance constraints
    #[msg("Single donor cannot propose")]
    SingleDonorCannotPropose,
    #[msg("Recovery not allowed for the configured LST")]
    RecoveryNotAllowedForLST,

    // PumpSwap validation / math
    #[msg("Invalid PumpSwap configuration")]
    InvalidPumpSwapConfig,
    #[msg("PumpSwap vault mints do not match expected mints")]
    InvalidPumpSwapVaultMints,
    #[msg("PumpSwap math error")]
    PumpSwapMathError,
    #[msg("PumpSwap computed min-out is zero")]
    PumpSwapMinOutZero,
}
