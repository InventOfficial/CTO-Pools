Below is a **complete, implementation-accurate `libV2.md`** you can drop into the repo.
It is written to be consumed by **roo-code**, Anchor test writers, and frontend engineers.
It is intentionally explicit about **semantics, invariants, and UX implications**.

---

# CTO Pools — Program V2 Specification (`libV2.md`)

## Overview

CTO Pools V2 is a **fully staked, governance-controlled treasury system** built on Solana.

**Core principle:**

> **All pool value is always staked as LST (jitoSOL).**
> Native SOL exists only *transiently* during withdrawals and proposal execution.

There is **no long-term SOL custody** by the program.

V2 replaces V1’s SOL-custody model with:

* Immediate staking on deposit
* LST-based accounting
* Governance-locked proportional liquidity
* Deterministic unstaking on withdrawal and execution

---

## High-Level Architecture

### Assets

* **Backing asset:** LST (jitoSOL or equivalent stake-pool mint)
* **Transient asset:** SOL (only during CPI withdraws)
* **Non-pool assets:** any other SPL token or SOL accidentally sent (recoverable via governance)

### Programs Used

* **CTO Pools Program** (this program)
* **SPL Stake Pool Program**

  * Devnet: `DPoo15wWDqpPJJtS2MUZ49aRxqz5ZaaJCJP4z8bLuib`
  * Mainnet: `SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy`
* **Token Program**
* **System Program**
* **Raydium / DEX (optional, CTOP buy-burn only)**

---

## Core Concepts

### Shares

* Shares represent **proportional ownership of the pool’s LST balance**
* Shares are minted **only from actual LST received**
* Shares are burned **based on actual LST consumed**

Shares never represent SOL directly.

---

### Pool Value Model

Let:

* `pool_lsts` = LST balance of pool ATA
* `total_shares` = total issued shares
* `donor_shares` = shares held by a donor

Then:

```
donor_claimable_lsts =
  floor(donor_shares / total_shares * pool_lsts)
```

---

### Reserved Liquidity (Governance Locks)

Governance proposals **reserve LST globally**, not per donor.

* `reserved_lsts` = total LST locked by active proposals
* `free_lsts = pool_lsts - reserved_lsts`

For any donor:

```
donor_free_lsts =
  floor(donor_shares / total_shares * free_lsts)
```

This guarantees:

* **All donors are locked proportionally**
* No per-donor bookkeeping is required
* Withdrawal limits emerge naturally

---

## Instructions

---

### 1. `create_pool`

Initializes a new CTO Pool.

#### Stores

* Stake pool program ID
* Stake pool state account
* LST mint
* Governance config
* Fee config

#### Important

* No token accounts are created here
* All ATAs are user-funded later

---

### 2. `donate_sol_and_stake`

**User deposits native SOL → immediately staked → pool receives LST.**

#### Flow

1. User sends SOL
2. Program CPI deposits SOL into stake pool
3. Pool receives LST into its ATA
4. Shares minted from **actual LST delta**

#### UX

* User deposits SOL
* UI shows “X jitoSOL minted”
* Shares increase

#### Invariants

* Pool never holds SOL after instruction
* Shares are minted only from confirmed LST received

---

### 3. `withdraw_sol`

**User withdraws value by requesting SOL (estimate).**

#### Inputs

* `requested_sol_estimate`

#### Flow

1. Compute donor’s `donor_free_lsts`
2. Convert requested SOL → required LST using **current stake-pool rate**
3. Require:

   ```
   required_lsts ≤ donor_free_lsts
   ```
4. CPI withdraw LST → receive SOL
5. Transfer **whatever SOL is produced** to the user
6. Burn shares based on actual LST consumed

#### UX

* UI displays:

  > “Your share: X jitoSOL ≈ Y SOL”
* User confirms withdrawal
* Final SOL may differ slightly due to rate/fees

#### No Withdraw Buffer

* Shares are exact
* No safety padding
* Explicitly disclosed in UI

---

### 4. `create_proposal`

Creates a governance proposal that **locks a percentage of the pool**.

#### Inputs

* Title
* Description
* `requested_sol_estimate`
* Destination wallet

#### Locking Semantics

1. Estimate protocol fee (1%)
2. Add **50 bps buffer**
3. Convert estimated SOL → LST at **creation-time rate**
4. Lock that LST amount:

   ```
   reserved_lsts += jitosol_locked
   ```

#### Stored on Proposal

* `requested_sol_estimate` (UI only)
* `jitosol_locked` (**authoritative**)
* Rate snapshot metadata
* Buffer bps (50)

#### Meaning of a Vote

> “Approve locking **X LST** and sending **all SOL produced from unstaking it** to the destination.”

This is **not** a promise of exact SOL.

---

### 5. `vote`

Standard snapshot-based governance:

* Vote weight = shares at snapshot
* 20% max per wallet
* Quorum enforced
* Single active proposal at a time

---

### 6. `execute_proposal`

Executes an approved proposal.

#### Flow

1. Unstake **exactly `jitosol_locked`**
2. Receive `sol_out`
3. Compute fee:

   ```
   fee = sol_out * protocol_fee_bps
   ```
4. Destination receives:

   ```
   sol_out - fee
   ```
5. Fee split:

   * 50% → Dev wallet
   * 50% → CTOP buy-burn

     * If burn fails → routed to Dev wallet
6. Unlock:

   ```
   reserved_lsts -= jitosol_locked
   ```

#### Key Semantics

* **Destination receives whatever SOL comes out**
* Upside from staking accrues to destination
* Protocol fee scales with realized value

---

### 7. `recover_funds` (Governance-Only)

Recovers **non-LST assets accidentally sent to the pool**.

#### What It Can Recover

* Native SOL accidentally transferred
* Any SPL token **except the pool’s LST mint**

#### What It Cannot Recover

* LST (jitoSOL)
* Any asset tracked as pool backing

#### Flow

1. Create **Recover Proposal**
2. Vote
3. Execute → transfer asset to destination

#### No Protocol Fee

Recovery is **fee-free** by design.

#### Why No “Sender Fast-Path”

On-chain programs cannot determine who sent unsolicited funds after the fact.
Therefore:

* All recovery is governance-based
* This avoids spoofing and false claims

---

## Fee Model

| Action             | Fee                      |
| ------------------ | ------------------------ |
| Withdraw           | 0                        |
| Proposal Execution | % of actual SOL received |
| Recovery           | 0                        |

Buy-burn failure always routes funds to Dev wallet.

---

## Devnet vs Mainnet Configuration

### Devnet

* Stake Pool Program: `DPoo15wWD…`
* Stake Pool: `JitoY5pc…`
* LST Mint: `J1tos8m…`

### Mainnet

* Stake Pool Program: `SPoo1Ku…`
* Stake Pool: Jito mainnet pool
* LST Mint: jitoSOL mint

These values are stored **per pool**, allowing one binary to serve all clusters.

---

## Critical Invariants (Must Hold)

1. Pool never holds SOL long-term
2. Shares only change when LST changes
3. `reserved_lsts ≤ pool_lsts`
4. Withdrawals are bounded by free liquidity
5. Proposals lock pool-wide percentage, not per donor
6. LST can never be recovered via `recover_funds`

---

## Frontend Responsibilities

* Display **LST-based ownership**, not SOL custody
* Clearly label:

  * “SOL estimate”
  * “Actual amount may differ”
* Show locked percentage during active proposals
* Require executor wallet to fund ATA creation
* Gracefully handle burn-swap failure messaging

---

## Anchor Test Expectations

Tests must validate:

* Stake-on-donate
* Withdraw bounded by free liquidity
* Proportional locking across donors
* Execution payout semantics
* Fee routing on burn failure
* Governance-only recovery
* Explicit rejection of LST recovery

---

## Summary

CTO Pools V2 is:

* Fully staked
* Insolvency-safe
* Governance-deterministic
* Scalable to thousands of pools
* Honest about yield and variability

This document is the **source of truth** for:

* Anchor tests
* Frontend UI
* Future audits