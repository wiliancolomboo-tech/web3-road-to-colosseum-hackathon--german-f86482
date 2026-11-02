use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

pub mod errors;
pub mod state;

use errors::*;
use state::*;

declare_id!("Kraft11111111111111111111111111111111111111");

#[program]
pub mod kraftwerk {
    use super::*;

    pub fn initialize_grid(
        ctx: Context<InitializeGrid>,
        grid_id: u64,
        tariff_min_micro_eur: u64,
        tariff_max_micro_eur: u64,
        fee_basis_points: u16,
    ) -> Result<()> {
        require!(tariff_min_micro_eur < tariff_max_micro_eur, KraftwerkError::PriceBoundaryViolation);
        require!(fee_basis_points <= 1000, KraftwerkError::PriceBoundaryViolation); // Max 10%

        let grid_config = &mut ctx.accounts.grid_config;
        grid_config.admin = ctx.accounts.admin.key();
        grid_config.grid_id = grid_id;
        grid_config.tariff_min_micro_eur = tariff_min_micro_eur;
        grid_config.tariff_max_micro_eur = tariff_max_micro_eur;
        grid_config.fee_basis_points = fee_basis_points;
        grid_config.is_halted = false;
        grid_config.bump = ctx.bumps.grid_config;

        msg!("Kraftwerk Grid initialized: ID {}", grid_id);
        Ok(())
    }

    pub fn register_meter(
        ctx: Context<RegisterMeter>,
        gateway_pubkey: Pubkey,
        is_producer: bool,
    ) -> Result<()> {
        let meter_state = &mut ctx.accounts.meter_state;
        meter_state.owner = ctx.accounts.owner.key();
        meter_state.grid_config = ctx.accounts.grid_config.key();
        meter_state.gateway_pubkey = gateway_pubkey;
        meter_state.is_producer = is_producer;
        meter_state.total_energy_recorded_wh = 0;
        meter_state.unallocated_energy_wh = 0;
        meter_state.last_reading_timestamp = Clock::get()?.unix_timestamp;
        meter_state.bump = ctx.bumps.meter_state;

        msg!("Meter registered: Owner {}, Producer: {}", meter_state.owner, is_producer);
        Ok(())
    }

    pub fn record_telemetry(
        ctx: Context<RecordTelemetry>,
        cumulative_reading_wh: u64,
        timestamp: i64,
    ) -> Result<()> {
        let grid_config = &ctx.accounts.grid_config;
        require!(!grid_config.is_halted, KraftwerkError::GridHalted);

        let clock = Clock::get()?;
        let current_time = clock.unix_timestamp;

        // BSI TR-03109 Drift Guard: reading must be within [-300s, +60s] of on-chain clock
        require!(
            timestamp >= current_time - 300 && timestamp <= current_time + 60,
            KraftwerkError::InvalidTimestamp
        );

        let meter = &mut ctx.accounts.meter_state;
        require!(timestamp > meter.last_reading_timestamp, KraftwerkError::InvalidTimestamp);
        require!(
            cumulative_reading_wh >= meter.total_energy_recorded_wh,
            KraftwerkError::NonMonotonicMeterReading
        );

        let delta_energy = cumulative_reading_wh
            .checked_sub(meter.total_energy_recorded_wh)
            .ok_or(KraftwerkError::MathOverflow)?;

        meter.total_energy_recorded_wh = cumulative_reading_wh;
        meter.last_reading_timestamp = timestamp;

        if meter.is_producer {
            meter.unallocated_energy_wh = meter
                .unallocated_energy_wh
                .checked_add(delta_energy)
                .ok_or(KraftwerkError::MathOverflow)?;
        }

        msg!("Telemetry recorded. Cumulative: {} Wh, Delta: {} Wh", cumulative_reading_wh, delta_energy);
        Ok(())
    }

    pub fn settle_energy_trade(
        ctx: Context<SettleEnergyTrade>,
        energy_wh: u64,
        rate_micro_eur_per_wh: u64,
    ) -> Result<()> {
        let grid_config = &ctx.accounts.grid_config;
        require!(!grid_config.is_halted, KraftwerkError::GridHalted);

        // Verify bounds
        require!(
            rate_micro_eur_per_wh >= grid_config.tariff_min_micro_eur
                && rate_micro_eur_per_wh <= grid_config.tariff_max_micro_eur,
            KraftwerkError::PriceBoundaryViolation
        );

        // Producer check
        let producer_meter = &mut ctx.accounts.producer_meter;
        require!(producer_meter.unallocated_energy_wh >= energy_wh, KraftwerkError::InsufficientVerifiedEnergy);

        // Compute pricing
        // Total cost = (energy_wh * rate_micro_eur_per_wh) / 1_000_000 (scaled to 6 decimals EURC)
        let gross_amount = (energy_wh as u128)
            .checked_mul(rate_micro_eur_per_wh as u128)
            .ok_or(KraftwerkError::MathOverflow)?
            .checked_div(1_000_000)
            .ok_or(KraftwerkError::MathOverflow)? as u64;

        let fee_amount = (gross_amount as u128)
            .checked_mul(grid_config.fee_basis_points as u128)
            .ok_or(KraftwerkError::MathOverflow)?
            .checked_div(10_000)
            .ok_or(KraftwerkError::MathOverflow)? as u64;

        let net_producer_amount = gross_amount
            .checked_sub(fee_amount)
            .ok_or(KraftwerkError::MathOverflow)?;

        // Deduct unallocated inventory
        producer_meter.unallocated_energy_wh = producer_meter
            .unallocated_energy_wh
            .checked_sub(energy_wh)
            .ok_or(KraftwerkError::MathOverflow)?;

        // Execute CPI Token Transfer: Buyer Escrow Token Account -> Producer Token Account
        let grid_id_bytes = grid_config.grid_id.to_le_bytes();
        let seeds = &[
            b"energy-escrow".as_ref(),
            grid_config.to_account_info().key.as_ref(),
            ctx.accounts.consumer.key.as_ref(),
            &[ctx.accounts.energy_escrow.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        let transfer_to_producer_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.escrow_vault.to_account_info(),
                to: ctx.accounts.producer_token_account.to_account_info(),
                authority: ctx.accounts.energy_escrow.to_account_info(),
            },
            signer_seeds,
        );
        token::transfer(transfer_to_producer_ctx, net_producer_amount)?;

        if fee_amount > 0 {
            let transfer_fee_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.escrow_vault.to_account_info(),
                    to: ctx.accounts.admin_fee_token_account.to_account_info(),
                    authority: ctx.accounts.energy_escrow.to_account_info(),
                },
                signer_seeds,
            );
            token::transfer(transfer_fee_ctx, fee_amount)?;
        }

        let escrow = &mut ctx.accounts.energy_escrow;
        escrow.locked_balance = escrow
            .locked_balance
            .checked_sub(gross_amount)
            .ok_or(KraftwerkError::InsufficientEscrowBalance)?;

        msg!(
            "Trade Settled: {} Wh @ {} uEUR/Wh. Total: {} microEUR (Fee: {})",
            energy_wh,
            rate_micro_eur_per_wh,
            gross_amount,
            fee_amount
        );

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT INSTRUCTION CONTEXTS
// -----------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(grid_id: u64)]
pub struct InitializeGrid<'info> {
    #[account(
        init,
        payer = admin,
        space = GridConfig::LEN,
        seeds = [b"grid-config", grid_id.to_le_bytes().as_ref()],
        bump
    )]
    pub grid_config: Account<'info, GridConfig>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RegisterMeter<'info> {
    #[account(
        init,
        payer = owner,
        space = MeterState::LEN,
        seeds = [b"meter", grid_config.key().as_ref(), owner.key().as_ref()],
        bump
    )]
    pub meter_state: Account<'info, MeterState>,
    pub grid_config: Account<'info, GridConfig>,
    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RecordTelemetry<'info> {
    #[account(
        mut,
        seeds = [b"meter", grid_config.key().as_ref(), meter_state.owner.as_ref()],
        bump = meter_state.bump,
        has_one = grid_config,
        has_one = gateway_pubkey @ KraftwerkError::UnauthorizedGateway
    )]
    pub meter_state: Account<'info, MeterState>,
    pub grid_config: Account<'info, GridConfig>,
    pub gateway_pubkey: Signer<'info>,
}

#[derive(Accounts)]
pub struct SettleEnergyTrade<'info> {
    #[account(mut)]
    pub grid_config: Account<'info, GridConfig>,

    #[account(
        mut,
        seeds = [b"meter", grid_config.key().as_ref(), producer_meter.owner.as_ref()],
        bump = producer_meter.bump,
        has_one = grid_config
    )]
    pub producer_meter: Account<'info, MeterState>,

    #[account(
        mut,
        seeds = [b"energy-escrow", grid_config.key().as_ref(), consumer.key().as_ref()],
        bump = energy_escrow.bump,
        has_one = consumer,
        has_one = grid_config,
        has_one = vault @ KraftwerkError::InsufficientEscrowBalance
    )]
    pub energy_escrow: Account<'info, EnergyEscrow>,

    #[account(mut)]
    pub escrow_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub producer_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub admin_fee_token_account: Account<'info, TokenAccount>,

    pub consumer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}