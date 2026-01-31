# CTOPools Solana Program – Technical Documentation (V3.1)

This document describes the **CTOPools Solana Program** as implemented in the latest `lib.rs` (V3.1). It is written in a **technical / audit-friendly style** and reflects the *actual code behavior*, not aspirational features.

---

## 1. High-level Overview

CTOPools is a **trustless, pooled-funding governance system** built on Solana.

Core objectives:

* Pool donor funds using **liquid staking (JitoSOL)**
* Allow **governance-controlled payouts** via proposals
* Enforce **anti-whale, anti-flashloan, and anti-rug safeguards**
* Fund protocol development via a **1% execution fee**, half of which is used for **buy & burn**
* Remain **permissionless**: anyone can execute proposals, but no executor can redirect funds

---

## 2. Trust Model & Threat Model

### Trust Assumptions

* **No trusted executor**: anyone may execute proposals
* **No trusted proposer**: proposals are adversarial by default
* **No identity assumptions**: wallets are anonymous, Sybil attacks are assumed possible

### Explicit Non-goals

* Sybil resistance via identity or reputation
* Blacklists or allowlists (rejected due to rent / DoS concerns)

### Core Security Philosophy

> *If an attack cannot be reliably prevented, the system ensures funds cannot be stolen.*

---

## 3. Pool Architecture

### Pool PDA

* One pool per SPL token mint
* PDA seed: `['pool', token_mint]`

### Stored Configuration

The pool permanently stores:

* Token mint being governed
* Jito stake pool program + pool + LST mint
* Governance parameters (quorum, caps)
* Dev fee wallet
* Buy & burn configuration:

  * PumpSwap pool id
  * Base vault (CTOP)
  * Quote vault (WSOL)
  * Global config
  * Protocol fee recipient

This design **prevents executors from injecting malicious accounts** during swaps.

---

## 4. Liquid Staking Integration (Jito)

### Accepted Configurations

The program **hard-restricts** stake pools to:

**Mainnet / Testnet**

* Program: `SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy`
* Stake Pool: `Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb`
* Mint: `J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn`

**Devnet**

* Devnet Jito equivalents only

Any deviation **hard-fails pool creation**.

---

## 5. Donor Model

### Deposits

* Donors deposit SOL
* SOL is staked via Jito
* Pool receives JitoSOL
* Donor receives **shares**

Shares represent proportional claim on **unreserved pool liquidity**.

### Withdrawals

* Donors may withdraw at any time *unless liquidity is reserved*
* Withdrawals burn shares and unstake JitoSOL

---

## 6. Governance: Proposals

### Proposal Creation Requirements

* ≥ 1 SOL deposited
* Shares held for ≥ 2 hours (`MIN_PROPOSAL_DELAY_SLOTS`)
* Not the sole donor in the pool
* No active proposal already exists

### Proposal Parameters

* Requested lamports
* Destination wallet
* Title (length-limited)
* Description (length-limited)

Requested amount is **buffered** before locking liquidity to protect against slippage.

---

## 7. Voting Mechanics

### Vote Choices

* YES
* NO
* ABSTAIN
* ABORT

### Vote Weight

* Snapshot-based at proposal creation
* Per-wallet cap: **20% of total shares**

### Quorum

* Participation (YES + NO + ABSTAIN + ABORT)
* Must reach **30% of total snapshot shares (capped)**

This ensures **no single whale can meet quorum alone**.

---

## 8. Abort Governance (M-04)

Abort is a **safety valve**, not a governance outcome.

### Abort Eligibility

Abort voters must meet **the same requirements as proposers**:

* ≥ 1 SOL deposited
* Shares aged ≥ 24 hours

### Abort Threshold

* **2 unique abort voters required**

### Abort Execution

* Executed via `execute_proposal`
* Unlocks reserved liquidity
* Marks proposal as `Aborted`
* Starts proposal cooldown window

### Design Rationale

Abort is intentionally weak:

* Sybil attacks are tolerated
* Abort cannot steal funds
* Worst-case outcome is temporary governance paralysis

---

## 9. Abort Penalty System

When a proposal is aborted:

### Penalized Parties (B)

* **Both abort voters**
* **The proposer**

### Penalty Mechanics

* Penalties are **uncapped and escalating**
* Applied as **mandatory SOL fees** on next action:

  * Abort voters: next Abort vote
  * Proposer: next Proposal creation

### Reset Conditions

* Abort voters: 3 consecutive non-abort participations
* Proposers: 3 proposals participated in without proposing

All penalty fees are sent to the **pool treasury**, benefiting donors.

---

## 10. Proposal Execution

### Timing

* Minimum execution delay: **12 hours**, even if quorum is met early

### Pass Conditions

* Quorum met
* YES > NO
* Not aborted

### Fail Conditions

* Quorum unmet
* NO ≥ YES

Failure **only unlocks liquidity**, no funds move.

---

## 11. Fee Model

### Protocol Fee

* 1% of actual SOL withdrawn

### Split

* 50% → Dev wallet
* 50% → Buy & burn (best effort)

### Failure Handling

* If swap fails, **100% goes to dev wallet**
* Proposal execution **never reverts** due to swap failure

---

## 12. Buy & Burn (PumpSwap)

### Swap Safety

* Uses **on-chain vault balances**
* Computes conservative `min_out`
* Applies slippage haircut

### Frontrun Protection

* On-chain min-out
* Vault mint validation
* Output forced to PDA-owned ATA

### Burn

* Tokens transferred to Solana incinerator

---

## 13. Executor Safety

Executors:

* Cannot redirect funds
* Cannot change swap recipients
* Cannot bypass penalties

Worst-case executor behavior:

* Pay gas to abort or execute
* Funds remain safe

---

## 14. Recovery Proposals

Used only for:

* Non-LST tokens accidentally sent to pool

Governed identically to payout proposals.

---

## 15. Upgrade Strategy

* Program remains upgradeable during beta
* Upgrade authority should be renounced only after:

  * PumpSwap interface stability
  * Live burn verification
  * Governance stress-testing

---

## 16. Summary

CTOPools V3.1 prioritizes:

* Fund safety over liveness
* Permissionless execution over trust
* Explicit tradeoffs over hidden assumptions

> *Any attack that cannot be prevented is converted into an inconvenience — never a loss of funds.*
