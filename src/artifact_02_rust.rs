use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("KRFTh8rZtqV4sTcxR7Y1mJmK6e5L2o8t1V3X7Y2zPump");

/// Precision scaling factor for fixed-point arithmetic (1e6)
pub const FIXED_PRECISION: u128 = 1_000_000;
/// Nominal German Grid Frequency in milliHertz (50.00 Hz = 50,000 mHz)
pub const NOMINAL_FREQUENCY_MHZ: u32 = 50_000;
/// Allowed standard frequency deadband in mHz (+/- 20 mHz)
pub const FREQUENCY_DEADBAND_MHZ: u32 = 20;
/// Slashed penalty fee points: 1000 = 10%
pub const PENALTY_BPS: u64 = 1_000;

#[program]
pub mod kraftwerk {
    use super::*;

    /// Initialize a new regional grid node responsible for balancing a localized sector
    pub fn initialize_grid_node(
        ctx: Context<InitializeGridNode>,
        node_id: String,
        base_rate_per_kwh: u64,
    ) -> Result<()> {
        require!(node_id.len() <= 32, KraftWerkError::NodeIdTooLong);
        let node = &mut ctx.accounts.grid_node;
        node.authority = ctx.accounts.authority.key();
        node.node_id = node_id;
        node.base_rate_per_kwh = base_rate_per_kwh;
        node.active_devices = 0;
        node.total_capacity_watts = 0;
        node.last_cleared_timestamp = Clock::get()?.unix_timestamp;
        node.current_frequency_mhz = NOMINAL_FREQUENCY_MHZ;
        node.bump = ctx.bumps.grid_node;
        node.vault_bump = ctx.bumps.settlement_vault;
        Ok(())
    }

    /// Register an edge smart inverter or residential storage asset
    pub fn register_device(
        ctx: Context<RegisterDevice>,
        max_capacity_watts: u64,
        device_signing_pubkey: Pubkey,
    ) -> Result<()> {
        require!(max_capacity_watts > 0, KraftWerkError::InvalidCapacity);
        
        let device = &mut ctx.accounts.device_record;
        device.owner = ctx.accounts.owner.key();
        device.grid_node = ctx.accounts.grid_node.key();
        device.hardware_pubkey = device_signing_pubkey;
        device.max_capacity_watts = max_capacity_watts;
        device.staked_collateral = ctx.accounts.collateral_account.amount;
        device.accumulated_unsettled_mwh = 0;
        device.last_telemetry_timestamp = Clock::get()?.unix_timestamp;
        device.is_active = true;
        device.bump = ctx.bumps.device_record;

        let node = &mut ctx.accounts.grid_node;
        node.active_devices = node.active_devices.checked_add(1)
            .ok_or(KraftWerkError::MathOverflow)?;
        node.total_capacity_watts = node.total_capacity_watts.checked_add(max_capacity_watts)
            .ok_or(KraftWerkError::MathOverflow)?;

        Ok(())
    }

    /// Ingest telemetry data. In production, this verifies that the prior instruction
    /// was the native Ed25519 precompile instruction matching `device_record.hardware_pubkey`.
    pub fn commit_energy_telemetry(
        ctx: Context<CommitEnergyTelemetry>,
        energy_delta_mwh: i64,
        observed_frequency_mhz: u32,
        timestamp: i64,
    ) -> Result<()> {
        let clock = Clock::get()?;
        require!(
            timestamp <= clock.unix_timestamp && timestamp >= clock.unix_timestamp - 300,
            KraftWerkError::StaleTelemetryTimestamp
        );

        let device = &mut ctx.accounts.device_record;
        require!(device.is_active, KraftWerkError::DeviceDisabled);
        
        // Ensure device owner or registered keeper signed
        require_keys_eq!(ctx.accounts.signer.key(), device.owner, KraftWerkError::Unauthorized);

        // Update accumulated unsettled energy (micro-watt hours)
        device.accumulated_unsettled_mwh = device
            .accumulated_unsettled_mwh
            .checked_add(energy_delta_mwh)
            .ok_or(KraftWerkError::MathOverflow)?;
        device.last_telemetry_timestamp = timestamp;

        // Update current localized grid frequency status
        let node = &mut ctx.accounts.grid_node;
        node.current_frequency_mhz = observed_frequency_mhz;

        emit!(TelemetryCommittedEvent {
            device: device.key(),
            energy_delta_mwh,
            frequency_mhz: observed_frequency_mhz,
            timestamp,
        });

        Ok(())
    }

    /// Settle balancing rounds: pays prosumers delivering counter-frequency support
    /// or slashes prosumers draining the grid during under-frequency events.
    pub fn settle_balancing_round(ctx: Context<SettleBalancingRound>) -> Result<()> {
        let node = &ctx.accounts.grid_node;
        let device = &mut ctx.accounts.device_record;
        require!(device.is_active, KraftWerkError::DeviceDisabled);
        require!(device.accumulated_unsettled_mwh != 0, KraftWerkError::NoUnsettledEnergy);

        // Dynamic pricing model:
        // German Frequency Containment Reserve (FCR) Rule:
        // If Frequency < 50.00 Hz (under-frequency), grid needs injection. Injection price surges.
        // If Frequency > 50.00 Hz (over-frequency), grid needs absorption. Consumption is rewarded or subsidized.
        let freq_delta: i64 = (node.current_frequency_mhz as i64) - (NOMINAL_FREQUENCY_MHZ as i64);
        
        let multiplier_scaled: i128 = if freq_delta.abs() <= (FREQUENCY_DEADBAND_MHZ as i64) {
            FIXED_PRECISION as i128
        } else {
            // Price scale shifts dynamically up to +/- 50% based on deviation
            let raw_multiplier = (FIXED_PRECISION as i128) - ((freq_delta as i128) * 10_000);
            if raw_multiplier < (FIXED_PRECISION as i128 / 2) {
                FIXED_PRECISION as i128 / 2
            } else {
                raw_multiplier
            }
        };

        let base_rate = node.base_rate_per_kwh as i128;
        let effective_rate_per_kwh = (base_rate * multiplier_scaled) / (FIXED_PRECISION as i128);

        // Convert mWh (micro-watt hours) to kWh for settlement:
        // 1 kWh = 1_000_000 mWh
        let energy_kwh = (device.accumulated_unsettled_mwh as i128) / 1_000_000;
        let settlement_amount = (energy_kwh * effective_rate_per_kwh) / (FIXED_PRECISION as i128);

        let node_id_bytes = node.node_id.as_bytes();
        let seeds = &[
            b"settlement_vault".as_ref(),
            node_id_bytes,
            &[node.vault_bump],
        ];
        let signer_seeds = &[&seeds[..]];

        if settlement_amount > 0 {
            // Pay prosumer from vault
            let payout_u64 = settlement_amount as u64;
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.settlement_vault.to_account_info(),
                        to: ctx.accounts.device_payout_account.to_account_info(),
                        authority: ctx.accounts.settlement_vault.to_account_info(),
                    },
                    signer_seeds,
                ),
                payout_u64,
            )?;
        } else if settlement_amount < 0 {
            // Energy absorbed or punitive draw; prosumer pays vault
            let penalty_u64 = (-settlement_amount) as u64;
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.device_payout_account.to_account_info(),
                        to: ctx.accounts.settlement_vault.to_account_info(),
                        authority: ctx.accounts.signer.to_account_info(),
                    },
                ),
                penalty_u64,
            )?;
        }

        emit!(BalancingSettledEvent {
            device: device.key(),
            net_amount: settlement_amount,
            cleared_mwh: device.accumulated_unsettled_mwh,
            clearing_rate_scaled: effective_rate_per_kwh as u64,
        });

        device.accumulated_unsettled_mwh = 0;
        Ok(())
    }

    /// Slash device collateral in the event of Byzantine reporting or hardware spoofing
    pub fn slash_deviant_device(
        ctx: Context<SlashDeviantDevice>,
        slash_amount: u64,
        reason: String,
    ) -> Result<()> {
        let device = &mut ctx.accounts.device_record;
        require!(slash_amount <= device.staked_collateral, KraftWerkError::InsufficientCollateral);
        
        device.staked_collateral = device.staked_collateral.checked_sub(slash_amount)
            .ok_or(KraftWerkError::MathOverflow)?;
        
        if device.staked_collateral == 0 {
            device.is_active = false;
        }

        // Transfer slashed funds from device collateral to grid reserve
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.device_collateral_account.to_account_info(),
                    to: ctx.accounts.slashed_reserve_account.to_account_info(),
                    authority: ctx.accounts.grid_authority.to_account_info(),
                },
            ),
            slash_amount,
        )?;

        emit!(DeviceSlashedEvent {
            device: device.key(),
            slashed_amount: slash_amount,
            reason,
        });

        Ok(())
    }
}

// -------------------------------------------------------------------------
// ACCOUNTS CONTEXT STRUCTS
// -------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(node_id: String)]
pub struct InitializeGridNode<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + GridNode::INIT_SPACE,
        seeds = [b"grid_node", node_id.as_bytes()],
        bump
    )]
    pub grid_node: Account<'info, GridNode>,

    #[account(
        init,
        payer = authority,
        seeds = [b"settlement_vault", node_id.as_bytes()],
        bump,
        token::mint = settlement_mint,
        token::authority = settlement_vault
    )]
    pub settlement_vault: Account<'info, TokenAccount>,

    pub settlement_mint: Account<'info, Mint>,

    #[account(mut)]
    pub authority: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct RegisterDevice<'info> {
    #[account(mut)]
    pub grid_node: Account<'info, GridNode>,

    #[account(
        init,
        payer = owner,
        space = 8 + DeviceRecord::INIT_SPACE,
        seeds = [b"device", owner.key().as_ref(), grid_node.key().as_ref()],
        bump
    )]
    pub device_record: Account<'info, DeviceRecord>,

    #[account(
        mut,
        constraint = collateral_account.owner == owner.key() @ KraftWerkError::Unauthorized
    )]
    pub collateral_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CommitEnergyTelemetry<'info> {
    #[account(mut)]
    pub grid_node: Account<'info, GridNode>,

    #[account(
        mut,
        seeds = [b"device", device_record.owner.as_ref(), grid_node.key().as_ref()],
        bump = device_record.bump
    )]
    pub device_record: Account<'info, DeviceRecord>,

    pub signer: Signer<'info>,
}

#[derive(Accounts)]
pub struct SettleBalancingRound<'info> {
    #[account(
        seeds = [b"grid_node", grid_node.node_id.as_bytes()],
        bump = grid_node.bump
    )]
    pub grid_node: Account<'info, GridNode>,

    #[account(
        mut,
        seeds = [b"device", device_record.owner.as_ref(), grid_node.key().as_ref()],
        bump = device_record.bump
    )]
    pub device_record: Account<'info, DeviceRecord>,

    #[account(
        mut,
        seeds = [b"settlement_vault", grid_node.node_id.as_bytes()],
        bump = grid_node.vault_bump
    )]
    pub settlement_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub device_payout_account: Account<'info, TokenAccount>,

    pub signer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SlashDeviantDevice<'info> {
    pub grid_authority: Signer<'info>,

    #[account(
        has_one = grid_authority @ KraftWerkError::Unauthorized
    )]
    pub grid_node: Account<'info, GridNode>,

    #[account(
        mut,
        seeds = [b"device", device_record.owner.as_ref(), grid_node.key().as_ref()],
        bump = device_record.bump
    )]
    pub device_record: Account<'info, DeviceRecord>,

    #[account(mut)]
    pub device_collateral_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub slashed_reserve_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

// -------------------------------------------------------------------------
// STATE STRUCTS
// -------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct GridNode {
    pub authority: Pubkey,         // 32
    #[max_len(32)]
    pub node_id: String,           // 4 + 32
    pub base_rate_per_kwh: u64,    // 8 (Euro cents with 1e6 precision)
    pub active_devices: u32,       // 4
    pub total_capacity_watts: u64, // 8
    pub last_cleared_timestamp: i64,// 8
    pub current_frequency_mhz: u32,// 4
    pub bump: u8,                  // 1
    pub vault_bump: u8,            // 1
}

#[account]
#[derive(InitSpace)]
pub struct DeviceRecord {
    pub owner: Pubkey,                     // 32
    pub grid_node: Pubkey,                 // 32
    pub hardware_pubkey: Pubkey,           // 32 (Ed25519 SMGW onboard key)
    pub max_capacity_watts: u64,           // 8
    pub staked_collateral: u64,            // 8
    pub accumulated_unsettled_mwh: i64,    // 8 (Signed micro-watt-hours)
    pub last_telemetry_timestamp: i64,     // 8
    pub is_active: bool,                   // 1
    pub bump: u8,                          // 1
}

// -------------------------------------------------------------------------
// EVENTS
// -------------------------------------------------------------------------

#[event]
pub struct TelemetryCommittedEvent {
    pub device: Pubkey,
    pub energy_delta_mwh: i64,
    pub frequency_mhz: u32,
    pub timestamp: i64,
}

#[event]
pub struct BalancingSettledEvent {
    pub device: Pubkey,
    pub net_amount: i128,
    pub cleared_mwh: i64,
    pub clearing_rate_scaled: u64,
}

#[event]
pub struct DeviceSlashedEvent {
    pub device: Pubkey,
    pub slashed_amount: u64,
    pub reason: String,
}

// -------------------------------------------------------------------------
// ERRORS
// -------------------------------------------------------------------------

#[error_code]
pub enum KraftWerkError {
    #[msg("Node identifier exceeds maximum length of 32 characters.")]
    NodeIdTooLong,
    #[msg("Max capacity in watts must be greater than zero.")]
    InvalidCapacity,
    #[msg("Arithmetic overflow occurred during computation.")]
    MathOverflow,
    #[msg("Telemetry timestamp is stale or from the future.")]
    StaleTelemetryTimestamp,
    #[msg("Device is disabled or slashed.")]
    DeviceDisabled,
    #[msg("Signer is not authorized for this operation.")]
    Unauthorized,
    #[msg("Device has no unsettled energy.")]
    NoUnsettledEnergy,
    #[msg("Requested slash amount exceeds available staked collateral.")]
    InsufficientCollateral,
}