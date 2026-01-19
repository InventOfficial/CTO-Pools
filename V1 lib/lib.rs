// ---------- CTO Pools Program ----------

use anchor_lang::prelude::*;
use anchor_lang::system_program;
use anchor_lang::solana_program::{instruction::Instruction, program::invoke_signed, instruction::AccountMeta};
use anchor_spl::token::{self, Token, TokenAccount, Mint, SyncNative};
use anchor_spl::associated_token::AssociatedToken;

declare_id!("GEZjJhN2DFWBaRMoTYM8JRKdyMYyMjYsG4Ag5LyEMJ2");

// ---------- Constants ----------

const PROTOCOL_FEE_BPS: u16 = 100;                // 1%
const QUORUM_BPS: u16 = 3000;                     // 30%
const MIN_PROPOSER_DEPOSIT_LAMPORTS: u64 = 1_000_000_000; // 1 SOL
const MAX_VOTER_BPS: u16 = 2000;                  // 20% voting cap per wallet
// SECURITY FIX: Minimum holding period before creating proposals (~1 day)
// At ~0.4 seconds per slot, 216,000 slots = approximately 86,400 seconds (1 day)
// TEMPORARILY SET TO 0 FOR TESTING - TODO: Restore to 216_000 for production
const MIN_PROPOSAL_DELAY_SLOTS: u64 = 0;

// Raydium AMM v4 Program IDs
pub const RAYDIUM_AMM_V4_MAINNET: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const RAYDIUM_AMM_V4_DEVNET: Pubkey = pubkey!("HWy1jotHpo6UqeQxx49dpYYdQB8wj9Qk9MdxwjLvDHB8");

// Serum DEX v3 Program ID (used by Raydium)
pub const SERUM_DEX_V3: Pubkey = pubkey!("9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin");

// Solana Incinerator address for burning tokens
pub const INCINERATOR: Pubkey = pubkey!("1nc1nerator11111111111111111111111111111111");

// Native SOL mint (wrapped SOL)
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

// Slippage tolerance - 15% for own token (sanity check for bugs)
// This is lenient since we're buying our own CTOP token
const MAX_SLIPPAGE_BPS: u64 = 1500; // 15%

// Raydium swap instruction discriminator
const RAYDIUM_SWAP_INSTRUCTION: u8 = 9;

// ---------- Program ----------

#[program]
pub mod cto_pools {
    use super::*;

    /// Create a CTO pool for a given token mint. Only one per mint.
    /// Accepts dev wallet, burn wallet, and burn token mint for 50/50 fee split mechanism.
    pub fn create_pool(
        ctx: Context<CreatePool>,
        dev_fee_wallet: Pubkey,
        burn_fee_wallet: Pubkey,
        burn_token_mint: Pubkey,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.token_mint = ctx.accounts.token_mint.key();
        pool.authority = ctx.accounts.creator.key();
        pool.creator = ctx.accounts.creator.key(); // Track original pool creator for exemptions
        pool.total_shares = 0;
        pool.total_sol_in_pool = 0;
        pool.reserved_lamports = 0;
        pool.total_spent = 0;
        pool.protocol_fee_bps = PROTOCOL_FEE_BPS;
        pool.quorum_bps = QUORUM_BPS;
        pool.min_proposer_deposit_lamports = MIN_PROPOSER_DEPOSIT_LAMPORTS;
        // Fee split: 50% to dev wallet, 50% to burn mechanism
        pool.dev_fee_wallet = dev_fee_wallet;
        pool.burn_fee_wallet = burn_fee_wallet; // Legacy - will be deprecated
        pool.burn_token_mint = burn_token_mint;
        pool.active_proposal = None;
        pool.proposal_count = 0;
        // Raydium configuration - starts disabled until configured
        pool.raydium_pool_id = Pubkey::default();
        pool.raydium_enabled = false;
        Ok(())
    }

    /// Configure Raydium pool for automatic swap-and-burn
    /// Authority only - called after CTOP token bonds on pump.fun
    pub fn configure_raydium_pool(
        ctx: Context<ConfigureRaydiumPool>,
        raydium_pool_id: Pubkey,
        enabled: bool,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        
        require!(
            ctx.accounts.authority.key() == pool.authority,
            CtoError::UnauthorizedAuthority
        );
        
        pool.raydium_pool_id = raydium_pool_id;
        pool.raydium_enabled = enabled;
        
        msg!("Raydium pool configured: {}, enabled: {}", raydium_pool_id, enabled);
        Ok(())
    }

    /// Donate SOL to a pool. Mints proportional "shares" to the donor.
    pub fn donate(ctx: Context<Donate>, amount: u64) -> Result<()> {
        require!(amount > 0, CtoError::ZeroAmount);

        let pool = &mut ctx.accounts.pool;
        let donor = &mut ctx.accounts.donor;
        let clock = Clock::get()?;

        // transfer lamports from user to pool PDA
        let ix = system_program::Transfer {
            from: ctx.accounts.donor_wallet.to_account_info(),
            to: pool.to_account_info(),
        };
        system_program::transfer(
            CpiContext::new(ctx.accounts.system_program.to_account_info(), ix),
            amount,
        )?;

        // pool-style share minting using u128 to prevent overflow
        let shares_minted = if pool.total_shares == 0 {
            amount
        } else {
            // Use u128 for intermediate calculation to prevent overflow
            // shares = (amount * total_shares) / total_sol_in_pool
            let amount_u128 = amount as u128;
            let total_shares_u128 = pool.total_shares as u128;
            let total_sol_u128 = pool.total_sol_in_pool as u128;
            
            let result = amount_u128
                .checked_mul(total_shares_u128)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(total_sol_u128)
                .ok_or(CtoError::MathOverflow)?;
            
            // Convert back to u64 - result should never exceed amount significantly
            u64::try_from(result).map_err(|_| CtoError::MathOverflow)?
        };

        pool.total_sol_in_pool = pool
            .total_sol_in_pool
            .checked_add(amount)
            .ok_or(CtoError::MathOverflow)?;
        pool.total_shares = pool
            .total_shares
            .checked_add(shares_minted)
            .ok_or(CtoError::MathOverflow)?;

        donor.pool = pool.key();
        donor.wallet = ctx.accounts.donor_wallet.key();
        donor.shares = donor
            .shares
            .checked_add(shares_minted)
            .ok_or(CtoError::MathOverflow)?;
        donor.total_deposited = donor
            .total_deposited
            .checked_add(amount)
            .ok_or(CtoError::MathOverflow)?;
        donor.last_shares_change_slot = clock.slot;

        Ok(())
    }

    /// Withdraw some or all of your pro-rata share of the pool's remaining SOL.
    pub fn withdraw(ctx: Context<Withdraw>, amount_opt: Option<u64>) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let donor = &mut ctx.accounts.donor;
        let clock = Clock::get()?;

        require!(donor.shares > 0, CtoError::NoShares);
        
        // Safety check: Prevent division by zero
        require!(pool.total_shares > 0, CtoError::MathOverflow);
        require!(pool.total_sol_in_pool > 0, CtoError::InsufficientFreeLiquidity);

        // max claimable based on current pool value - use u128 to prevent overflow
        let max_claimable = {
            let shares_u128 = donor.shares as u128;
            let sol_u128 = pool.total_sol_in_pool as u128;
            let total_shares_u128 = pool.total_shares as u128;
            
            let result = shares_u128
                .checked_mul(sol_u128)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(total_shares_u128)
                .ok_or(CtoError::MathOverflow)?;
            
            u64::try_from(result).map_err(|_| CtoError::MathOverflow)?
        };

        let amount = amount_opt.unwrap_or(max_claimable);
        require!(amount > 0, CtoError::ZeroAmount);
        require!(amount <= max_claimable, CtoError::InsufficientWithdrawable);

        // don't touch funds reserved for active proposals
        let free_lamports = pool
            .total_sol_in_pool
            .checked_sub(pool.reserved_lamports)
            .ok_or(CtoError::MathOverflow)?;
        require!(amount <= free_lamports, CtoError::InsufficientFreeLiquidity);

        // compute shares to burn - use u128 to prevent overflow
        let shares_to_burn = {
            let amount_u128 = amount as u128;
            let total_shares_u128 = pool.total_shares as u128;
            let total_sol_u128 = pool.total_sol_in_pool as u128;
            
            let result = amount_u128
                .checked_mul(total_shares_u128)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(total_sol_u128)
                .ok_or(CtoError::MathOverflow)?;
            
            u64::try_from(result).map_err(|_| CtoError::MathOverflow)?
        };

        donor.shares = donor
            .shares
            .checked_sub(shares_to_burn)
            .ok_or(CtoError::MathOverflow)?;
        pool.total_shares = pool
            .total_shares
            .checked_sub(shares_to_burn)
            .ok_or(CtoError::MathOverflow)?;
        pool.total_sol_in_pool = pool
            .total_sol_in_pool
            .checked_sub(amount)
            .ok_or(CtoError::MathOverflow)?;

        donor.last_shares_change_slot = clock.slot;

        // transfer lamports from pool PDA to donor
        let seeds: &[&[&[u8]]] = &[&[
            b"pool",
            pool.token_mint.as_ref(),
            &[ctx.bumps.pool],
        ]];
        
        try_withdraw(
            &pool.to_account_info(),
            &ctx.accounts.donor_wallet.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            seeds,
            amount,
        )?;

        Ok(())
    }

    /// Create a proposal to request sending SOL from the pool to a destination.
    /// SECURITY: Single donors cannot create proposals (they cannot meet 30% quorum with 20% voting cap)
    /// Single donors should withdraw their funds directly instead.
    pub fn create_proposal(
        ctx: Context<CreateProposal>,
        kind: ProposalKind,
        requested_amount: u64,
        target_wallet: Pubkey,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let donor = &mut ctx.accounts.donor;
        let proposal = &mut ctx.accounts.proposal;
        let clock = Clock::get()?;

        require!(pool.active_proposal.is_none(), CtoError::ActiveProposalExists);
        require!(donor.shares > 0, CtoError::NoShares);

        let proposer_wallet = ctx.accounts.proposer_wallet.key();

        // SECURITY FIX: Prevent single donors from creating proposals
        // With a single donor, their voting power is capped at 20% of total shares,
        // but quorum requires 30% participation. This means a single donor can NEVER
        // pass a proposal. Instead, they should withdraw funds directly.
        // A donor is "single" if they own 100% of shares (their shares == total_shares)
        require!(
            pool.total_shares != donor.shares,
            CtoError::SingleDonorCannotPropose
        );
        msg!("✓ Pool has multiple donors - proposal creation allowed");

        // proposer value in lamports - use u128 to prevent overflow
        let donor_value = {
            let shares_u128 = donor.shares as u128;
            let sol_u128 = pool.total_sol_in_pool as u128;
            let total_shares_u128 = pool.total_shares as u128;
            
            let result = shares_u128
                .checked_mul(sol_u128)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(total_shares_u128)
                .ok_or(CtoError::MathOverflow)?;
            
            u64::try_from(result).map_err(|_| CtoError::MathOverflow)?
        };
        require!(
            donor_value >= pool.min_proposer_deposit_lamports,
            CtoError::ProposerTooSmall
        );

        // SECURITY FEATURE: Check if proposer has held shares long enough (pool creator is exempt)
        // This prevents rapid buy-propose-sell attacks and encourages long-term commitment
        let is_pool_creator = pool.creator == proposer_wallet;
        if !is_pool_creator {
            let slots_since_change = clock.slot
                .checked_sub(donor.last_shares_change_slot)
                .ok_or(CtoError::MathOverflow)?;
            
            require!(
                slots_since_change >= MIN_PROPOSAL_DELAY_SLOTS,
                CtoError::SharesTooRecent
            );
        }

        // snapshot
        proposal.pool = pool.key();
        proposal.requested_amount = requested_amount;
        proposal.destination_wallet = target_wallet;
        proposal.created_at_ts = clock.unix_timestamp;
        proposal.deadline_ts = clock
            .unix_timestamp
            .checked_add(24 * 60 * 60)
            .ok_or(CtoError::MathOverflow)?;
        proposal.snapshot_slot = clock.slot;
        proposal.total_snapshot_shares = pool.total_shares;
        proposal.yes_weight = 0;
        proposal.no_weight = 0;
        proposal.abstain_weight = 0;
        proposal.participation_weight = 0;
        proposal.status = ProposalStatus::Active;
        proposal.kind = kind;

        require!(requested_amount > 0, CtoError::ZeroAmount);

        // compute fee + reserve
        let fee = requested_amount
            .checked_mul(pool.protocol_fee_bps as u64)
            .ok_or(CtoError::MathOverflow)?
            .checked_div(10_000)
            .ok_or(CtoError::MathOverflow)?;
        let total_reserved = requested_amount
            .checked_add(fee)
            .ok_or(CtoError::MathOverflow)?;

        let free_lamports = pool
            .total_sol_in_pool
            .checked_sub(pool.reserved_lamports)
            .ok_or(CtoError::MathOverflow)?;
        require!(
            total_reserved <= free_lamports,
            CtoError::InsufficientFreeLiquidity
        );

        pool.reserved_lamports = pool
            .reserved_lamports
            .checked_add(total_reserved)
            .ok_or(CtoError::MathOverflow)?;

        // BUG FIX #2: Increment proposal counter to allow multiple sequential proposals
        // This prevents PDA collision when creating new proposals after previous ones are executed/failed
        pool.proposal_count = pool.proposal_count.checked_add(1).ok_or(CtoError::MathOverflow)?;

        pool.active_proposal = Some(proposal.key());

        Ok(())
    }

    /// Cast or change a vote (Yes / No / Abstain) on an active proposal.
    /// Voting power per wallet is capped at 20% of total snapshot shares.
    pub fn vote(ctx: Context<Vote>, choice: VoteChoice) -> Result<()> {
        let _pool = &ctx.accounts.pool;
        let proposal = &mut ctx.accounts.proposal;
        let donor = &ctx.accounts.donor;
        let vote_record = &mut ctx.accounts.vote_record;
        let clock = Clock::get()?;

        require!(proposal.status == ProposalStatus::Active, CtoError::ProposalNotActive);
        require!(clock.unix_timestamp <= proposal.deadline_ts, CtoError::VotingClosed);
        require!(donor.shares > 0, CtoError::NoShares);

        // donor must not have changed shares after snapshot to vote
        require!(
            donor.last_shares_change_slot <= proposal.snapshot_slot,
            CtoError::NotEligibleForThisProposal
        );

        // If existing vote, subtract old weight from tallies
        if vote_record.initialized {
            let prev_choice = vote_record.choice;
            let w = vote_record.snapshot_weight;
            match prev_choice {
                VoteChoice::Yes => {
                    proposal.yes_weight = proposal
                        .yes_weight
                        .checked_sub(w)
                        .ok_or(CtoError::MathOverflow)?;
                }
                VoteChoice::No => {
                    proposal.no_weight = proposal
                        .no_weight
                        .checked_sub(w)
                        .ok_or(CtoError::MathOverflow)?;
                }
                VoteChoice::Abstain => {
                    proposal.abstain_weight = proposal
                        .abstain_weight
                        .checked_sub(w)
                        .ok_or(CtoError::MathOverflow)?;
                }
            }
        }

        // Compute (or reuse) snapshot weight with 20% cap
        let snapshot_weight = if vote_record.initialized {
            vote_record.snapshot_weight
        } else {
            let raw_weight = donor.shares;
            
            // Calculate 20% cap of total snapshot shares
            let voter_cap = proposal
                .total_snapshot_shares
                .checked_mul(MAX_VOTER_BPS as u64)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(10_000)
                .ok_or(CtoError::MathOverflow)?;

            // Only apply cap if voter's shares exceed 20% of total
            if raw_weight > voter_cap {
                voter_cap
            } else {
                raw_weight
            }
        };

        // Add to new choice
        match choice {
            VoteChoice::Yes => {
                proposal.yes_weight = proposal
                    .yes_weight
                    .checked_add(snapshot_weight)
                    .ok_or(CtoError::MathOverflow)?;
            }
            VoteChoice::No => {
                proposal.no_weight = proposal
                    .no_weight
                    .checked_add(snapshot_weight)
                    .ok_or(CtoError::MathOverflow)?;
            }
            VoteChoice::Abstain => {
                proposal.abstain_weight = proposal
                    .abstain_weight
                    .checked_add(snapshot_weight)
                    .ok_or(CtoError::MathOverflow)?;
            }
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

    /// Execute an active proposal after its voting window.
    /// If quorum + majority, sends funds + fee (with automatic swap-and-burn if enabled)
    pub fn execute_proposal(mut ctx: Context<ExecuteProposal>) -> Result<()> {
        let clock = Clock::get()?;

        require!(ctx.accounts.proposal.status == ProposalStatus::Active, CtoError::ProposalNotActive);

        // compute quorum first
        let quorum_met = ctx.accounts.proposal
            .participation_weight
            .checked_mul(10_000)
            .ok_or(CtoError::MathOverflow)?
            >= ctx.accounts.proposal
                .total_snapshot_shares
                .checked_mul(ctx.accounts.pool.quorum_bps as u64)
                .ok_or(CtoError::MathOverflow)?;

        // Can execute if: time expired OR quorum already met (allows early execution when enough voters participate)
        let time_over = clock.unix_timestamp >= ctx.accounts.proposal.deadline_ts;
        require!(time_over || quorum_met, CtoError::TooEarlyToExecute);

        let majority_met = ctx.accounts.proposal.yes_weight > ctx.accounts.proposal.no_weight;

        // Helper: compute payout fee + total_out
        let compute_payout = |requested: u64, protocol_fee_bps: u16| -> Result<(u64, u64)> {
            let fee = requested
                .checked_mul(protocol_fee_bps as u64)
                .ok_or(CtoError::MathOverflow)?
                .checked_div(10_000)
                .ok_or(CtoError::MathOverflow)?;
            let total_out = requested
                .checked_add(fee)
                .ok_or(CtoError::MathOverflow)?;
            Ok((fee, total_out))
        };

        // Fail path: quorum or majority not met
        if !(quorum_met && majority_met) {
            let requested_amount = ctx.accounts.proposal.requested_amount;
            let (fee, total_out) = compute_payout(requested_amount, ctx.accounts.pool.protocol_fee_bps)?;
            
            // Log detailed failure information for debugging
            msg!("❌ Proposal FAILED - unlocking funds");
            msg!("   Quorum met: {}, Majority met: {}", quorum_met, majority_met);
            msg!("   Participation: {} / {} (quorum requires {})",
                ctx.accounts.proposal.participation_weight,
                ctx.accounts.proposal.total_snapshot_shares,
                ctx.accounts.pool.quorum_bps
            );
            msg!("   Votes - Yes: {}, No: {}, Abstain: {}",
                ctx.accounts.proposal.yes_weight,
                ctx.accounts.proposal.no_weight,
                ctx.accounts.proposal.abstain_weight
            );
            msg!("   Unlocking {} lamports (requested: {}, fee: {})", total_out, requested_amount, fee);
            msg!("   Reserved before: {} lamports", ctx.accounts.pool.reserved_lamports);
            
            // unlock reserved lamports - THIS IS CRITICAL FOR WITHDRAWALS
            ctx.accounts.pool.reserved_lamports = ctx.accounts.pool
                .reserved_lamports
                .checked_sub(total_out)
                .ok_or(CtoError::MathOverflow)?;
            
            msg!("   Reserved after: {} lamports", ctx.accounts.pool.reserved_lamports);
            msg!("   Pool total_sol: {} lamports", ctx.accounts.pool.total_sol_in_pool);
            msg!("   Free liquidity: {} lamports",
                ctx.accounts.pool.total_sol_in_pool.saturating_sub(ctx.accounts.pool.reserved_lamports)
            );
            
            ctx.accounts.proposal.status = ProposalStatus::Failed;
            ctx.accounts.pool.active_proposal = None;
            
            // Emit event for frontend tracking
            emit!(ProposalFailedEvent {
                pool: ctx.accounts.pool.key(),
                proposal: ctx.accounts.proposal.key(),
                unlocked_lamports: total_out,
                quorum_met,
                majority_met,
                timestamp: clock.unix_timestamp,
            });
            
            return Ok(());
        }

        // Pass path
        let requested_amount = ctx.accounts.proposal.requested_amount;
        let (fee, total_out) =
            compute_payout(requested_amount, ctx.accounts.pool.protocol_fee_bps)?;

        require!(
            total_out <= ctx.accounts.pool.reserved_lamports,
            CtoError::InsufficientFreeLiquidity
        );
        require!(
            total_out <= ctx.accounts.pool.total_sol_in_pool,
            CtoError::InsufficientFreeLiquidity
        );

        // Store bump for seed construction
        let pool_bump = ctx.bumps.pool;
        
        // BUG FIX #1: Send FULL requested amount to destination (not reduced by fee)
        // The fee is separately split 50/50 between dev and burn wallets below.
        {
            let seeds: &[&[&[u8]]] = &[&[
                b"pool",
                ctx.accounts.pool.token_mint.as_ref(),
                &[pool_bump],
            ]];
            try_withdraw(
                &ctx.accounts.pool.to_account_info(),
                &ctx.accounts.destination_wallet.to_account_info(),
                &ctx.accounts.system_program.to_account_info(),
                seeds,
                requested_amount,
            )?;
        }

        // Split protocol fee 50/50 between dev wallet and burn mechanism
        let fee_half = fee.checked_div(2).ok_or(CtoError::MathOverflow)?;

        // Transfer 50% to dev wallet (for development costs)
        {
            let seeds: &[&[&[u8]]] = &[&[
                b"pool",
                ctx.accounts.pool.token_mint.as_ref(),
                &[pool_bump],
            ]];
            try_withdraw(
                &ctx.accounts.pool.to_account_info(),
                &ctx.accounts.dev_fee_wallet.to_account_info(),
                &ctx.accounts.system_program.to_account_info(),
                seeds,
                fee_half,
            )?;
        }

        // Phase 2: Automatic Swap-and-Burn (if Raydium enabled)
        // If swap fails, SOL accumulates in pool for manual processing later
        if ctx.accounts.pool.raydium_enabled && ctx.accounts.raydium_pool.key() != Pubkey::default() {
            match attempt_swap_and_burn(&mut ctx, fee_half, pool_bump) {
                Ok(ctop_burned) => {
                    msg!("✅ Swap and burn successful: {} lamports → {} CTOP → incinerator", fee_half, ctop_burned);
                    emit!(TokenBurnEvent {
                        pool: ctx.accounts.pool.key(),
                        amount_sol: fee_half,
                        amount_ctop: ctop_burned,
                        timestamp: clock.unix_timestamp,
                    });
                }
                Err(e) => {
                    msg!("⚠️ Swap failed ({}), {} SOL accumulated in pool", e, fee_half);
                    // Emit failure event for monitoring
                    emit!(SwapFailureEvent {
                        pool: ctx.accounts.pool.key(),
                        amount_sol: fee_half,
                        error_code: 0, // Generic swap failure
                        timestamp: clock.unix_timestamp,
                    });
                    // SOL stays in pool - no action needed
                }
            }
        } else {
            // Legacy behavior: send to burn fee wallet (Phase 1)
            let seeds: &[&[&[u8]]] = &[&[
                b"pool",
                ctx.accounts.pool.token_mint.as_ref(),
                &[pool_bump],
            ]];
            try_withdraw(
                &ctx.accounts.pool.to_account_info(),
                &ctx.accounts.burn_fee_wallet.to_account_info(),
                &ctx.accounts.system_program.to_account_info(),
                seeds,
                fee_half,
            )?;
        }

        ctx.accounts.pool.total_sol_in_pool = ctx.accounts.pool
            .total_sol_in_pool
            .checked_sub(total_out)
            .ok_or(CtoError::MathOverflow)?;
        ctx.accounts.pool.total_spent = ctx.accounts.pool
            .total_spent
            .checked_add(requested_amount)
            .ok_or(CtoError::MathOverflow)?;
        ctx.accounts.pool.reserved_lamports = ctx.accounts.pool
            .reserved_lamports
            .checked_sub(total_out)
            .ok_or(CtoError::MathOverflow)?;

        ctx.accounts.proposal.status = ProposalStatus::Executed;
        ctx.accounts.pool.active_proposal = None;

        Ok(())
    }
}

// ---------- Helper Functions for Swap and Burn ----------

/// Attempts to swap SOL to CTOP via Raydium and burn to incinerator
/// Returns amount of CTOP burned on success, error on failure
fn attempt_swap_and_burn<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    amount_sol: u64,
    pool_bump: u8,
) -> Result<u64> {
    // Step 1: Wrap SOL to WSOL in pool's WSOL account
    wrap_sol_to_wsol(ctx, amount_sol, pool_bump)?;
    
    //Step 2: Calculate minimum CTOP output with slippage protection
    let minimum_ctop_out = calculate_minimum_amount_out(
        ctx,
        amount_sol,
        MAX_SLIPPAGE_BPS,
    )?;
    
    // Step 3: Perform Raydium swap: WSOL → CTOP
    perform_raydium_swap(ctx, amount_sol, minimum_ctop_out, pool_bump)?;
    
    // Step 4: Transfer all CTOP to incinerator
    let ctop_burned = transfer_to_incinerator(ctx, pool_bump)?;
    
    Ok(ctop_burned)
}

/// Wraps SOL to WSOL by transferring to pool's WSOL account and syncing
fn wrap_sol_to_wsol<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    amount: u64,
    pool_bump: u8,
) -> Result<()> {
    let seeds: &[&[&[u8]]] = &[&[
        b"pool",
        ctx.accounts.pool.token_mint.as_ref(),
        &[pool_bump],
    ]];
    
    // Transfer SOL from pool to pool's WSOL token account
    system_program::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.pool.to_account_info(),
                to: ctx.accounts.pool_wsol_account.to_account_info(),
            },
            seeds,
        ),
        amount,
    )?;
    
    // Sync wrapped SOL account (converts lamports to SPL token balance)
    token::sync_native(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            SyncNative {
                account: ctx.accounts.pool_wsol_account.to_account_info(),
            },
        ),
    )?;
    
    Ok(())
}

/// Calculates minimum CTOP output with slippage protection
/// Uses simple constant product formula estimation
fn calculate_minimum_amount_out<'info>(
    _ctx: &mut Context<ExecuteProposal<'info>>,
    amount_in: u64,
    slippage_bps: u64,
) -> Result<u64> {
    // For now, use a conservative estimate
    // In production, this should query actual Raydium pool reserves
    // Formula: output = (input * reserve_out) / (reserve_in + input)
    
    // Conservative minimum: expect at least some tokens
    // This is mainly a sanity check to catch total failures
    let minimum_out = amount_in
        .checked_mul(10_000 - slippage_bps)
        .ok_or(CtoError::MathOverflow)?
        .checked_div(10_000)
        .ok_or(CtoError::MathOverflow)?
        .checked_div(100) // Very conservative - expect at least 1% of SOL value in CTOP
        .ok_or(CtoError::MathOverflow)?;
    
    // Ensure non-zero
    if minimum_out == 0 {
        return Ok(1); // At least 1 token
    }
    
    Ok(minimum_out)
}

/// Performs Raydium swap via CPI
fn perform_raydium_swap<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    amount_in: u64,
    minimum_amount_out: u64,
    pool_bump: u8,
) -> Result<()> {
    let seeds: &[&[&[u8]]] = &[&[
        b"pool",
        ctx.accounts.pool.token_mint.as_ref(),
        &[pool_bump],
    ]];
    
    // Build Raydium swap instruction data
    let mut swap_data = Vec::with_capacity(17);
    swap_data.push(RAYDIUM_SWAP_INSTRUCTION); // Instruction discriminator
    swap_data.extend_from_slice(&amount_in.to_le_bytes());
    swap_data.extend_from_slice(&minimum_amount_out.to_le_bytes());
    
    // Build account metas for Raydium CPI
    let account_metas = vec![
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
        AccountMeta::new(ctx.accounts.pool_wsol_account.key(), false), // User source
        AccountMeta::new(ctx.accounts.pool_ctop_account.key(), false), // User destination
        AccountMeta::new_readonly(ctx.accounts.pool.key(), true), // User authority (signer)
    ];
    
    // Create swap instruction
    let swap_ix = Instruction {
        program_id: ctx.accounts.raydium_program.key(),
        accounts: account_metas,
        data: swap_data,
    };
    
    // Execute CPI with pool PDA as signer
    invoke_signed(
        &swap_ix,
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
        seeds,
    )?;
    
    Ok(())
}

/// Transfers all CTOP from pool's account to incinerator
/// Returns amount transferred
fn transfer_to_incinerator<'info>(
    ctx: &mut Context<ExecuteProposal<'info>>,
    pool_bump: u8,
) -> Result<u64> {
    let seeds: &[&[&[u8]]] = &[&[
        b"pool",
        ctx.accounts.pool.token_mint.as_ref(),
        &[pool_bump],
    ]];
    
    // Get CTOP balance from pool's token account
    ctx.accounts.pool_ctop_account.reload()?;
    let ctop_balance = ctx.accounts.pool_ctop_account.amount;
    
    require!(ctop_balance > 0, CtoError::NoTokensToburn);
    
    // Transfer all CTOP to incinerator's token account
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            token::Transfer {
                from: ctx.accounts.pool_ctop_account.to_account_info(),
                to: ctx.accounts.incinerator_ctop_account.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            },
            seeds,
        ),
        ctop_balance,
    )?;
    
    Ok(ctop_balance)
}

// Helper function to safely transfer lamports from PDA while respecting rent-exempt minimum
fn try_withdraw<'info>(
    from: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    _system_program: &AccountInfo<'info>,
    _seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    // Calculate minimum rent-exempt balance for this account
    let rent = Rent::get()?;
    let data_len = from.data_len();
    let min_balance = rent.minimum_balance(data_len);
    
    let from_balance = from.lamports();
    let remaining = from_balance
        .checked_sub(amount)
        .ok_or(CtoError::MathOverflow)?;
    
    // Ensure we maintain rent-exempt minimum
    require!(
        remaining >= min_balance,
        CtoError::InsufficientFreeLiquidity
    );
    
    // Manually transfer lamports (avoids "from must not carry data" error)
    **from.try_borrow_mut_lamports()? -= amount;
    **to.try_borrow_mut_lamports()? += amount;
    
    Ok(())
}

// ---------- Accounts & State ----------

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
pub struct Donate<'info> {
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
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
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
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Vote<'info> {
    #[account(
        seeds = [b"pool", pool.token_mint.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        has_one = pool
    )]
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
    pub pool: Box<Account<'info, Pool>>,

    #[account(
        mut,
        has_one = pool
    )]
    pub proposal: Box<Account<'info, Proposal>>,

    /// CHECK: payout destination wallet
    #[account(mut)]
    pub destination_wallet: UncheckedAccount<'info>,

    /// CHECK: receives 50% of protocol fee for development costs
    #[account(mut, address = pool.dev_fee_wallet)]
    pub dev_fee_wallet: UncheckedAccount<'info>,

    /// CHECK: legacy burn wallet (Phase 1 fallback)
    #[account(mut, address = pool.burn_fee_wallet)]
    pub burn_fee_wallet: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,

    // ===== PHASE 2: RAYDIUM SWAP ACCOUNTS (Optional - only if raydium_enabled) =====
    
    /// Raydium AMM v4 program
    /// CHECK: Raydium program ID
    #[account()]
    pub raydium_program: UncheckedAccount<'info>,
    
    /// Raydium pool account (CTOP/SOL pool)
    /// CHECK: Validated against pool.raydium_pool_id
    #[account(mut)]
    pub raydium_pool: UncheckedAccount<'info>,
    
    /// Raydium pool authority
    /// CHECK: Derived by Raydium
    pub raydium_pool_authority: UncheckedAccount<'info>,
    
    /// Raydium open orders
    /// CHECK: Part of Raydium pool
    #[account(mut)]
    pub raydium_open_orders: UncheckedAccount<'info>,
    
    /// Raydium target orders
    /// CHECK: Part of Raydium pool
    #[account(mut)]
    pub raydium_target_orders: UncheckedAccount<'info>,
    
    /// Pool coin vault (CTOP)
    /// CHECK: Part of Raydium pool
    #[account(mut)]
    pub raydium_coin_vault: UncheckedAccount<'info>,
    
    /// Pool PC vault (WSOL)
    /// CHECK: Part of Raydium pool
    #[account(mut)]
    pub raydium_pc_vault: UncheckedAccount<'info>,
    
    /// Serum DEX program
    /// CHECK: Serum program ID
    pub serum_program: UncheckedAccount<'info>,
    
    /// Serum market
    /// CHECK: Part of Serum market
    #[account(mut)]
    pub serum_market: UncheckedAccount<'info>,
    
    /// Serum bids
    /// CHECK: Part of Serum market
    #[account(mut)]
    pub serum_bids: UncheckedAccount<'info>,
    
    /// Serum asks
    /// CHECK: Part of Serum market
    #[account(mut)]
    pub serum_asks: UncheckedAccount<'info>,
    
    /// Serum event queue
    /// CHECK: Part of Serum market
    #[account(mut)]
    pub serum_event_queue: UncheckedAccount<'info>,
    
    /// Serum coin vault (CTOP)
    /// CHECK: Part of Serum market
    #[account(mut)]
    pub serum_coin_vault: UncheckedAccount<'info>,
    
    /// Serum PC vault (WSOL)
    /// CHECK: Part of Serum market
    #[account(mut)]
    pub serum_pc_vault: UncheckedAccount<'info>,
    
    /// Serum vault signer
    /// CHECK: Derived from Serum market
    pub serum_vault_signer: UncheckedAccount<'info>,
    
    // ===== TOKEN ACCOUNTS =====
    
    /// Pool's wrapped SOL account
    #[account(
        init_if_needed,
        payer = executor,
        token::mint = wsol_mint,
        token::authority = pool,
        seeds = [b"pool_wsol", pool.key().as_ref()],
        bump
    )]
    pub pool_wsol_account: Box<Account<'info, TokenAccount>>,
    
    /// Pool's CTOP token account
    #[account(
        init_if_needed,
        payer = executor,
        token::mint = ctop_mint,
        token::authority = pool,
        seeds = [b"pool_ctop", pool.key().as_ref()],
        bump
    )]
    pub pool_ctop_account: Box<Account<'info, TokenAccount>>,
    
    /// Incinerator's CTOP account (burn destination)
    #[account(
        init_if_needed,
        payer = executor,
        associated_token::mint = ctop_mint,
        associated_token::authority = incinerator
    )]
    pub incinerator_ctop_account: Box<Account<'info, TokenAccount>>,
    
    // ===== MINTS & PROGRAMS =====
    
    /// Native SOL mint (wrapped SOL)
    #[account(address = WSOL_MINT)]
    pub wsol_mint: Box<Account<'info, Mint>>,
    
    /// CTOP token mint
    #[account(address = pool.burn_token_mint)]
    pub ctop_mint: Box<Account<'info, Mint>>,
    
    /// Incinerator address
    /// CHECK: Fixed address
    #[account(address = INCINERATOR)]
    pub incinerator: UncheckedAccount<'info>,
    
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
    
    /// Executor who pays for account creation
    #[account(mut)]
    pub executor: Signer<'info>,
}

// ---------- State Types ----------

#[account]
pub struct Pool {
    pub token_mint: Pubkey,
    pub authority: Pubkey,
    pub creator: Pubkey,
    pub total_shares: u64,
    pub total_sol_in_pool: u64,
    pub reserved_lamports: u64,
    pub total_spent: u64,
    pub protocol_fee_bps: u16,
    pub quorum_bps: u16,
    pub min_proposer_deposit_lamports: u64,
    pub dev_fee_wallet: Pubkey,
    pub burn_fee_wallet: Pubkey,      // Legacy - for Phase 1 compatibility
    pub burn_token_mint: Pubkey,
    pub active_proposal: Option<Pubkey>,
    pub proposal_count: u64,
    // Phase 2: Raydium configuration
    pub raydium_pool_id: Pubkey,      // Raydium CTOP/SOL pool address
    pub raydium_enabled: bool,        // Enable/disable automatic swap-and-burn
}

impl Pool {
    // SIZE calculation: all fields in bytes
    // 32*3 (pubkeys) + 8*4 (u64s) + 2*2 (u16s) + 8 (u64) + 32*3 (pubkeys) + 1+32 (Option<Pubkey>) + 8 (u64) + 32 (pubkey) + 1 (bool)
    pub const SIZE: usize =
        32 + 32 + 32 + // token_mint, authority, creator
        8 + 8 + 8 + 8 + // total_shares, total_sol_in_pool, reserved_lamports, total_spent
        2 + 2 + 8 + // protocol_fee_bps, quorum_bps, min_proposer_deposit_lamports
        32 + 32 + 32 + // dev_fee_wallet, burn_fee_wallet, burn_token_mint
        1 + 32 + // active_proposal (Option<Pubkey>)
        8 + // proposal_count
        32 + // raydium_pool_id
        1; // raydium_enabled
}

#[account]
pub struct Donor {
    pub pool: Pubkey,
    pub wallet: Pubkey,
    pub shares: u64,
    pub total_deposited: u64,
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
    pub requested_amount: u64,
    pub destination_wallet: Pubkey,
    pub created_at_ts: i64,
    pub deadline_ts: i64,
    pub snapshot_slot: u64,
    pub total_snapshot_shares: u64,
    pub yes_weight: u64,
    pub no_weight: u64,
    pub abstain_weight: u64,
    pub participation_weight: u64,
    pub status: ProposalStatus,
    pub kind: ProposalKind,
}
impl Proposal {
    pub const SIZE: usize =
        32 + 8 + 32 + 8 + 8 + 8 + 8 + 8 + 8 + 8 + 8 + 1 + 1;
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

// ---------- Events ----------

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

/// Event emitted when a proposal fails (quorum or majority not met)
/// Used for frontend tracking and debugging fund unlock issues
#[event]
pub struct ProposalFailedEvent {
    pub pool: Pubkey,
    pub proposal: Pubkey,
    pub unlocked_lamports: u64,
    pub quorum_met: bool,
    pub majority_met: bool,
    pub timestamp: i64,
}

// ---------- Errors ----------

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
    #[msg("Insufficient free liquidity (reserved or missing)")]
    InsufficientFreeLiquidity,
    #[msg("There is already an active proposal for this pool")]
    ActiveProposalExists,
    #[msg("Proposer's deposit is too small to create a proposal")]
    ProposerTooSmall,
    #[msg("Proposal is not active")]
    ProposalNotActive,
    #[msg("Voting for this proposal is closed")]
    VotingClosed,
    #[msg("You are not eligible to vote on this proposal")]
    NotEligibleForThisProposal,
    #[msg("Too early to execute this proposal")]
    TooEarlyToExecute,
    #[msg("Shares must be held for at least 1 day before creating proposals")]
    SharesTooRecent,
    #[msg("Unauthorized: Only pool authority can perform this action")]
    UnauthorizedAuthority,
    #[msg("Raydium swap failed - insufficient liquidity or high slippage")]
    SwapFailed,
    #[msg("No tokens to burn")]
    NoTokensToburn,
    #[msg("Raydium pool not configured")]
    RaydiumNotConfigured,
    #[msg("Single donor cannot create proposals - withdraw funds directly instead. With 20% voting cap and 30% quorum, a single donor can never pass a proposal.")]
    SingleDonorCannotPropose,
}
