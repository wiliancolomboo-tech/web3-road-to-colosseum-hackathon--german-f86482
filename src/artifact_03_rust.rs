use anchor_lang::prelude::*;
use anchor_spl::token_2022::{self, TransferChecked};
use anchor_spl::token_interface::{Mint, Token2022, TokenAccount};

declare_id!("KREiS11111111111111111111111111111111111111");

pub const PRECISION_SCALE: u128 = 1_000_000_000_000; // 1e12 Fixed Point Scaling
pub const SECONDS_PER_YEAR: u64 = 31_536_000;
pub const MAX_UTILIZATION_BPS: u64 = 10_000; // 100.00%
pub const OPTIMAL_UTILIZATION_BPS: u64 = 8_000; // 80.00%

#[program]
pub mod kreis_protocol {
    use super::*;

    /// Initialize the Institutional Revolving Liquidity Pool
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        base_rate_bps: u16,
        jump_rate_bps: u16,
        reserve_ratio_bps: u16,
    ) -> Result<()> {
        require!(base_rate_bps <= 1500, KreisError::ExcessiveInterestRate); // Max 15% base
        require!(jump_rate_bps <= 5000, KreisError::ExcessiveInterestRate); // Max 50% jump
        require!(reserve_ratio_bps <= 3000, KreisError::InvalidReserveRatio);

        let pool = &mut ctx.accounts.pool;
        pool.authority = ctx.accounts.authority.key();
        pool.euro_mint = ctx.accounts.euro_mint.key();
        pool.pool_vault = ctx.accounts.pool_vault.key();
        pool.base_rate_bps = base_rate_bps;
        pool.jump_rate_bps = jump_rate_bps;
        pool.reserve_ratio_bps = reserve_ratio_bps;
        pool.total_deposits = 0;
        pool.total_borrowed = 0;
        pool.accrued_reserve_fees = 0;
        pool.last_accrual_ts = Clock::get()?.unix_timestamp;
        pool.bump = ctx.bumps.pool;
        pool.vault_bump = ctx.bumps.vault_authority;

        emit!(PoolInitializedEvent {
            pool: pool.key(),
            authority: pool.authority,
            euro_mint: pool.euro_mint,
            base_rate_bps,
            jump_rate_bps,
        });

        Ok(())
    }

    /// Supply Euro Liquidity to Earn Continuous Compounding Yield
    pub fn deposit_liquidity(ctx: Context<DepositLiquidity>, amount: u64) -> Result<()> {
        require!(amount > 0, KreisError::ZeroAmount);

        let pool = &mut ctx.accounts.pool;
        accrue_interest(pool)?;

        // Transfer funds from Liquidity Provider to Pool Vault using Token-2022
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.depositor_token_account.to_account_info(),
            mint: ctx.accounts.euro_mint.to_account_info(),
            to: ctx.accounts.pool_vault.to_account_info(),
            authority: ctx.accounts.depositor.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        token_2022::transfer_checked(cpi_ctx, amount, ctx.accounts.euro_mint.decimals)?;

        pool.total_deposits = pool
            .total_deposits
            .checked_add(amount)
            .ok_or(KreisError::MathOverflow)?;

        emit!(LiquidityDepositedEvent {
            depositor: ctx.accounts.depositor.key(),
            amount,
            pool_total_deposits: pool.total_deposits,
        });

        Ok(())
    }

    /// Register, Verify and Drawdown an Approved Corporate Invoice
    pub fn factor_invoice(
        ctx: Context<FactorInvoice>,
        invoice_id: [u8; 32],
        nominal_value: u64,
        advance_rate_bps: u16,
        maturity_ts: i64,
    ) -> Result<()> {
        require!(advance_rate_bps <= 9000, KreisError::AdvanceRateExceeded); // Max 90%
        let current_ts = Clock::get()?.unix_timestamp;
        require!(maturity_ts > current_ts, KreisError::InvalidMaturity);
        require!(
            maturity_ts <= current_ts + (180 * 86400),
            KreisError::MaturityTooDistant
        ); // Max 180 days

        let pool = &mut ctx.accounts.pool;
        accrue_interest(pool)?;

        // Calculate max draw amount
        let advance_amount = (nominal_value as u128)
            .checked_mul(advance_rate_bps as u128)
            .ok_or(KreisError::MathOverflow)?
            .checked_div(MAX_UTILIZATION_BPS as u128)
            .ok_or(KreisError::MathOverflow)? as u64;

        // Verify Pool Solvency & Available Reserves
        let available_liquidity = pool
            .total_deposits
            .checked_sub(pool.total_borrowed)
            .ok_or(KreisError::InsufficientPoolLiquidity)?;
        require!(
            available_liquidity >= advance_amount,
            KreisError::InsufficientPoolLiquidity
        );

        // Populate Invoice State
        let invoice = &mut ctx.accounts.invoice;
        invoice.pool = pool.key();
        invoice.invoice_id = invoice_id;
        invoice.supplier = ctx.accounts.supplier.key();
        invoice.debtor = ctx.accounts.debtor_verifier.key();
        invoice.nominal_value = nominal_value;
        invoice.drawn_amount = advance_amount;
        invoice.repaid_amount = 0;
        invoice.advance_rate_bps = advance_rate_bps;
        invoice.maturity_ts = maturity_ts;
        invoice.factored_ts = current_ts;
        invoice.status = InvoiceStatus::Active;
        invoice.bump = ctx.bumps.invoice;

        // Update Pool aggregates
        pool.total_borrowed = pool
            .total_borrowed
            .checked_add(advance_amount)
            .ok_or(KreisError::MathOverflow)?;

        // Transfer advance directly from pool vault to supplier
        let euro_mint_key = pool.euro_mint;
        let seeds = &[
            b"vault_authority".as_ref(),
            euro_mint_key.as_ref(),
            &[pool.vault_bump],
        ];
        let signer = &[&seeds[..]];

        let cpi_accounts = TransferChecked {
            from: ctx.accounts.pool_vault.to_account_info(),
            mint: ctx.accounts.euro_mint.to_account_info(),
            to: ctx.accounts.supplier_token_account.to_account_info(),
            authority: ctx.accounts.vault_authority.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
        token_2022::transfer_checked(cpi_ctx, advance_amount, ctx.accounts.euro_mint.decimals)?;

        emit!(InvoiceFactoredEvent {
            invoice: invoice.key(),
            supplier: invoice.supplier,
            advance_amount,
            nominal_value,
            maturity_ts,
        });

        Ok(())
    }

    /// Full or Partial Repayment of Invoice by Debtor or Supplier
    pub fn settle_invoice(ctx: Context<SettleInvoice>, repayment_amount: u64) -> Result<()> {
        require!(repayment_amount > 0, KreisError::ZeroAmount);
        let invoice = &mut ctx.accounts.invoice;
        require!(
            invoice.status == InvoiceStatus::Active,
            KreisError::InvoiceNotActive
        );

        let pool = &mut ctx.accounts.pool;
        accrue_interest(pool)?;

        // Calculate dynamic interest on borrowed capital
        let elapsed_time = (Clock::get()?.unix_timestamp)
            .checked_sub(invoice.factored_ts)
            .max(0) as u64;
        let rate_bps = calculate_current_rate(pool)?;
        let interest_due = (invoice.drawn_amount as u128)
            .checked_mul(rate_bps as u128)
            .ok_or(KreisError::MathOverflow)?
            .checked_mul(elapsed_time as u128)
            .ok_or(KreisError::MathOverflow)?
            .checked_div((SECONDS_PER_YEAR as u128) * (MAX_UTILIZATION_BPS as u128))
            .ok_or(KreisError::MathOverflow)? as u64;

        let total_debt_due = invoice
            .drawn_amount
            .checked_add(interest_due)
            .ok_or(KreisError::MathOverflow)?
            .checked_sub(invoice.repaid_amount)
            .unwrap_or(0);

        let actual_payment = repayment_amount.min(total_debt_due);

        // Transfer funds from payer into pool vault
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.payer_token_account.to_account_info(),
            mint: ctx.accounts.euro_mint.to_account_info(),
            to: ctx.accounts.pool_vault.to_account_info(),
            authority: ctx.accounts.payer.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
        token_2022::transfer_checked(cpi_ctx, actual_payment, ctx.accounts.euro_mint.decimals)?;

        invoice.repaid_amount = invoice
            .repaid_amount
            .checked_add(actual_payment)
            .ok_or(KreisError::MathOverflow)?;

        // Deduct principal balance from total_borrowed
        let principal_repaid = actual_payment.min(invoice.drawn_amount);
        pool.total_borrowed = pool.total_borrowed.saturating_sub(principal_repaid);

        // Protocol reserve deduction
        let reserve_fee = (actual_payment.saturating_sub(principal_repaid) as u128)
            .checked_mul(pool.reserve_ratio_bps as u128)
            .ok_or(KreisError::MathOverflow)?
            .checked_div(MAX_UTILIZATION_BPS as u128)
            .ok_or(KreisError::MathOverflow)? as u64;

        pool.accrued_reserve_fees = pool
            .accrued_reserve_fees
            .checked_add(reserve_fee)
            .ok_or(KreisError::MathOverflow)?;

        if invoice.repaid_amount >= invoice.drawn_amount.checked_add(interest_due).unwrap_or(u64::MAX) {
            invoice.status = InvoiceStatus::Settled;
            emit!(InvoiceSettledEvent {
                invoice: invoice.key(),
                total_settled: invoice.repaid_amount,
            });
        }

        Ok(())
    }

    /// Dutch Auction Liquidation Crank for Overdue Defaults
    pub fn liquidate_default(ctx: Context<LiquidateDefault>) -> Result<()> {
        let current_ts = Clock::get()?.unix_timestamp;
        let invoice = &mut ctx.accounts.invoice;
        let pool = &mut ctx.accounts.pool;

        require!(
            invoice.status == InvoiceStatus::Active,
            KreisError::InvoiceNotActive
        );
        // Default trigger: 7 days post-maturity grace period
        let grace_period = 7 * 86400;
        require!(
            current_ts > invoice.maturity_ts.checked_add(grace_period).ok_or(KreisError::MathOverflow)?,
            KreisError::GracePeriodActive
        );

        // Dutch auction price decay: 1% discount per hour elapsed post grace period (max 50% discount)
        let elapsed_liquidation = current_ts.saturating_sub(invoice.maturity_ts + grace_period) as u64;
        let hours_elapsed = elapsed_liquidation / 3600;
        let discount_bps = (hours_elapsed * 100).min(5000); // Max 50.00% discount

        let buyout_cost = (invoice.nominal_value as u128)
            .checked_mul(MAX_UTILIZATION_BPS.saturating_sub(discount_bps) as u128)
            .ok_or(KreisError::MathOverflow)?
            .checked_div(MAX_UTILIZATION_BPS as u128)
            .ok_or(KreisError::MathOverflow)? as u64;

        // Keeper covers the buyout_cost into the pool
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.keeper_token_account.to_account_info(),
            mint: ctx.accounts.euro_mint.to_account_info(),
            to: ctx.accounts.pool_vault.to_account_info(),
            authority: ctx.accounts.keeper.to_account_info(),
        };
        token_2022::transfer_checked(
            CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts),
            buyout_cost,
            ctx.accounts.euro_mint.decimals,
        )?;

        // Close outstanding debt inside pool
        pool.total_borrowed = pool.total_borrowed.saturating_sub(invoice.drawn_amount);
        invoice.status = InvoiceStatus::Liquidated;

        emit!(InvoiceLiquidatedEvent {
            invoice: invoice.key(),
            liquidator: ctx.accounts.keeper.key(),
            buyout_cost,
            nominal_value: invoice.nominal_value,
        });

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal Interest & Mathematical Functions (Q64.64 / Integer Math)
// ---------------------------------------------------------------------------

pub fn calculate_current_rate(pool: &PoolAccount) -> Result<u16> {
    if pool.total_deposits == 0 {
        return Ok(pool.base_rate_bps);
    }

    let utilization_bps = (pool.total_borrowed as u128)
        .checked_mul(MAX_UTILIZATION_BPS as u128)
        .ok_or(KreisError::MathOverflow)?
        .checked_div(pool.total_deposits as u128)
        .ok_or(KreisError::MathOverflow)? as u64;

    if utilization_bps <= OPTIMAL_UTILIZATION_BPS {
        // Linear slope 1: base + (utilization / optimal) * base
        let slope = (utilization_bps as u128)
            .checked_mul(pool.base_rate_bps as u128)
            .ok_or(KreisError::MathOverflow)?
            .checked_div(OPTIMAL_UTILIZATION_BPS as u128)
            .ok_or(KreisError::MathOverflow)? as u16;
        Ok(pool.base_rate_bps + slope)
    } else {
        // Jump slope 2: base + base + ((utilization - optimal) / (1 - optimal)) * jump_rate
        let excess_utilization = utilization_bps.saturating_sub(OPTIMAL_UTILIZATION_BPS);
        let remaining_range = MAX_UTILIZATION_BPS.saturating_sub(OPTIMAL_UTILIZATION_BPS);
        let jump = (excess_utilization as u128)
            .checked_mul(pool.jump_rate_bps as u128)
            .ok_or(KreisError::MathOverflow)?
            .checked_div(remaining_range as u128)
            .ok_or(KreisError::MathOverflow)? as u16;
        Ok(pool.base_rate_bps * 2 + jump)
    }
}

pub fn accrue_interest(pool: &mut PoolAccount) -> Result<()> {
    let current_ts = Clock::get()?.unix_timestamp;
    let delta_t = current_ts.saturating_sub(pool.last_accrual_ts) as u64;

    if delta_t == 0 || pool.total_borrowed == 0 {
        pool.last_accrual_ts = current_ts;
        return Ok(());
    }

    let current_rate = calculate_current_rate(pool)?;
    let interest_accumulated = (pool.total_borrowed as u128)
        .checked_mul(current_rate as u128)
        .ok_or(KreisError::MathOverflow)?
        .checked_mul(delta_t as u128)
        .ok_or(KreisError::MathOverflow)?
        .checked_div((SECONDS_PER_YEAR as u128) * (MAX_UTILIZATION_BPS as u128))
        .ok_or(KreisError::MathOverflow)? as u64;

    pool.total_deposits = pool
        .total_deposits
        .checked_add(interest_accumulated)
        .ok_or(KreisError::MathOverflow)?;
    pool.last_accrual_ts = current_ts;

    Ok(())
}

// ---------------------------------------------------------------------------
// Account Context Structs
// ---------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializePool<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    pub euro_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        init,
        payer = authority,
        space = 8 + PoolAccount::INIT_SPACE,
        seeds = [b"liquidity_pool", euro_mint.key().as_ref()],
        bump
    )]
    pub pool: Box<Account<'info, PoolAccount>>,

    /// CHECK: Native PDA authority for token vault transfers
    #[account(
        seeds = [b"vault_authority", euro_mint.key().as_ref()],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = authority,
        token::mint = euro_mint,
        token::authority = vault_authority,
        token::token_program = token_program,
        seeds = [b"pool_vault", pool.key().as_ref()],
        bump
    )]
    pub pool_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, Token2022>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct DepositLiquidity<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        seeds = [b"liquidity_pool", euro_mint.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, PoolAccount>>,

    pub euro_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        constraint = depositor_token_account.mint == euro_mint.key(),
        constraint = depositor_token_account.owner == depositor.key()
    )]
    pub depositor_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        address = pool.pool_vault
    )]
    pub pool_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, Token2022>,
}

#[derive(Accounts)]
#[instruction(invoice_id: [u8; 32])]
pub struct FactorInvoice<'info> {
    #[account(mut)]
    pub supplier: Signer<'info>,

    /// Enterprise Debtor Compliance Witness
    pub debtor_verifier: Signer<'info>,

    #[account(
        mut,
        seeds = [b"liquidity_pool", euro_mint.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, PoolAccount>>,

    pub euro_mint: Box<InterfaceAccount<'info, Mint>>,

    /// CHECK: PDA signer for vault disbursements
    #[account(
        seeds = [b"vault_authority", euro_mint.key().as_ref()],
        bump = pool.vault_bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        address = pool.pool_vault
    )]
    pub pool_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = supplier_token_account.mint == euro_mint.key(),
        constraint = supplier_token_account.owner == supplier.key()
    )]
    pub supplier_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        init,
        payer = supplier,
        space = 8 + InvoiceAccount::INIT_SPACE,
        seeds = [b"invoice", pool.key().as_ref(), invoice_id.as_ref()],
        bump
    )]
    pub invoice: Box<Account<'info, InvoiceAccount>>,

    pub token_program: Interface<'info, Token2022>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct SettleInvoice<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        mut,
        seeds = [b"liquidity_pool", euro_mint.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, PoolAccount>>,

    pub euro_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [b"invoice", pool.key().as_ref(), invoice.invoice_id.as_ref()],
        bump = invoice.bump
    )]
    pub invoice: Box<Account<'info, InvoiceAccount>>,

    #[account(
        mut,
        constraint = payer_token_account.mint == euro_mint.key(),
        constraint = payer_token_account.owner == payer.key()
    )]
    pub payer_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        address = pool.pool_vault
    )]
    pub pool_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, Token2022>,
}

#[derive(Accounts)]
pub struct LiquidateDefault<'info> {
    #[account(mut)]
    pub keeper: Signer<'info>,

    #[account(
        mut,
        seeds = [b"liquidity_pool", euro_mint.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, PoolAccount>>,

    pub euro_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [b"invoice", pool.key().as_ref(), invoice.invoice_id.as_ref()],
        bump = invoice.bump
    )]
    pub invoice: Box<Account<'info, InvoiceAccount>>,

    #[account(
        mut,
        constraint = keeper_token_account.mint == euro_mint.key(),
        constraint = keeper_token_account.owner == keeper.key()
    )]
    pub keeper_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        address = pool.pool_vault
    )]
    pub pool_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, Token2022>,
}

// ---------------------------------------------------------------------------
// State Structures
// ---------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct PoolAccount {
    pub authority: Pubkey,        // 32
    pub euro_mint: Pubkey,        // 32
    pub pool_vault: Pubkey,       // 32
    pub total_deposits: u64,      // 8
    pub total_borrowed: u64,      // 8
    pub accrued_reserve_fees: u64,// 8
    pub last_accrual_ts: i64,     // 8
    pub base_rate_bps: u16,       // 2
    pub jump_rate_bps: u16,       // 2
    pub reserve_ratio_bps: u16,   // 2
    pub bump: u8,                 // 1
    pub vault_bump: u8,           // 1
}

#[account]
#[derive(InitSpace)]
pub struct InvoiceAccount {
    pub pool: Pubkey,             // 32
    pub invoice_id: [u8; 32],     // 32
    pub supplier: Pubkey,         // 32
    pub debtor: Pubkey,           // 32
    pub nominal_value: u64,       // 8
    pub drawn_amount: u64,        // 8
    pub repaid_amount: u64,       // 8
    pub maturity_ts: i64,         // 8
    pub factored_ts: i64,         // 8
    pub advance_rate_bps: u16,    // 2
    pub status: InvoiceStatus,    // 1
    pub bump: u8,                 // 1
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum InvoiceStatus {
    Active,
    Settled,
    Liquidated,
}

// ---------------------------------------------------------------------------
// Events & Errors
// ---------------------------------------------------------------------------

#[event]
pub struct PoolInitializedEvent {
    pub pool: Pubkey,
    pub authority: Pubkey,
    pub euro_mint: Pubkey,
    pub base_rate_bps: u16,
    pub jump_rate_bps: u16,
}

#[event]
pub struct LiquidityDepositedEvent {
    pub depositor: Pubkey,
    pub amount: u64,
    pub pool_total_deposits: u64,
}

#[event]
pub struct InvoiceFactoredEvent {
    pub invoice: Pubkey,
    pub supplier: Pubkey,
    pub advance_amount: u64,
    pub nominal_value: u64,
    pub maturity_ts: i64,
}

#[event]
pub struct InvoiceSettledEvent {
    pub invoice: Pubkey,
    pub total_settled: u64,
}

#[event]
pub struct InvoiceLiquidatedEvent {
    pub invoice: Pubkey,
    pub liquidator: Pubkey,
    pub buyout_cost: u64,
    pub nominal_value: u64,
}

#[error_code]
pub enum KreisError {
    #[msg("Calculation overflow occurred")]
    MathOverflow,
    #[msg("Provided amount must be greater than zero")]
    ZeroAmount,
    #[msg("Base or jump interest rate exceeds safety limit")]
    ExcessiveInterestRate,
    #[msg("Reserve ratio configuration out of bounds")]
    InvalidReserveRatio,
    #[msg("Requested advance rate exceeds protocol ceiling of 90.00%")]
    AdvanceRateExceeded,
    #[msg("Invoice maturity must be strictly in the future")]
    InvalidMaturity,
    #[msg("Invoice term cannot exceed 180 days")]
    MaturityTooDistant,
    #[msg("Insufficient unencumbered liquidity in the pool vault")]
    InsufficientPoolLiquidity,
    #[msg("Invoice is not in Active status")]
    InvoiceNotActive,
    #[msg("Default grace period is still active")]
    GracePeriodActive,
}