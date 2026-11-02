use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("KFTWk4n1xV7eL8oX8n2iK1uP4QW5xR9pS3tM7aB2cDE");

pub const CONFIG_SEED: &[u8] = b"config";
pub const GRID_NODE_SEED: &[u8] = b"grid_node";
pub const METER_SEED: &[u8] = b"meter";
pub const CHANNEL_SEED: &[u8] = b"channel";
pub const VAULT_SEED: &[u8] = b"vault";

pub const PRECISION: u128 = 1_000_000_000_000; // 1e12 fixed-point scale
pub const MAX_BPS: u64 = 10_000;

#[program]
pub mod kraftwerk {
    use super::*;

    /// Initialize Global Protocol Configuration
    pub fn initialize_protocol(
        ctx: Context<InitializeProtocol>,
        protocol_fee_bps: u16,
        operator: Pubkey,
    ) -> Result<()> {
        require!(protocol_fee_bps <= 500, KraftwerkError::FeeExceedsCap); // Max 5%
        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();
        config.operator = operator;
        config.settlement_mint = ctx.accounts.settlement_mint.key();
        config.protocol_fee_bps = protocol_fee_bps;
        config.total_energy_settled_kwh = 0;
        config.bump = ctx.bumps.config;
        emit!(ProtocolInitializedEvent {
            admin: config.admin,
            settlement_mint: config.settlement_mint,
            protocol_fee_bps,
        });
        Ok(())
    }

    /// Register a Regional Grid Node (Substation) with Dynamic Congestion Limits
    pub fn register_grid_node(
        ctx: Context<RegisterGridNode>,
        grid_id: [u8; 16],
        base_rate_per_kwh: u64,
        max_capacity_kw: u64,
    ) -> Result<()> {
        let node = &mut ctx.accounts.grid_node;
        node.grid_id = grid_id;
        node.operator = ctx.accounts.operator.key();
        node.base_rate_per_kwh = base_rate_per_kwh;
        node.max_capacity_kw = max_capacity_kw;
        node.current_load_kw = 0;
        node.congestion_multiplier_bps = MAX_BPS; // 1.0x baseline
        node.bump = ctx.bumps.grid_node;

        emit!(GridNodeRegisteredEvent {
            grid_id,
            base_rate_per_kwh,
            max_capacity_kw,
        });
        Ok(())
    }

    /// Register Hardware Smart Meter Binding with Cryptographic Enclave Key
    pub fn register_smart_meter(
        ctx: Context<RegisterSmartMeter>,
        device_pubkey: Pubkey,
        meter_type: MeterType,
    ) -> Result<()> {
        let meter = &mut ctx.accounts.meter_account;
        meter.owner = ctx.accounts.owner.key();
        meter.device_pubkey = device_pubkey;
        meter.grid_node = ctx.accounts.grid_node.key();
        meter.meter_type = meter_type;
        meter.total_kwh_accumulated = 0;
        meter.is_active = true;
        meter.bump = ctx.bumps.meter_account;

        emit!(MeterRegisteredEvent {
            owner: meter.owner,
            device_pubkey,
            meter_type,
        });
        Ok(())
    }

    /// Open a Microgrid Streaming Channel between Producer & Consumer
    pub fn open_stream_channel(
        ctx: Context<OpenStreamChannel>,
        channel_id: u64,
        deposit_amount: u64,
    ) -> Result<()> {
        require!(deposit_amount > 0, KraftwerkError::ZeroDeposit);

        let channel = &mut ctx.accounts.stream_channel;
        channel.channel_id = channel_id;
        channel.producer_meter = ctx.accounts.producer_meter.key();
        channel.consumer_meter = ctx.accounts.consumer_meter.key();
        channel.consumer_authority = ctx.accounts.consumer_authority.key();
        channel.escrow_balance = deposit_amount;
        channel.total_settled_kwh = 0;
        channel.last_update_slot = Clock::get()?.slot;
        channel.is_closed = false;
        channel.bump = ctx.bumps.stream_channel;

        // Transfer funds from consumer to protocol escrow vault
        let cpi_accounts = TransferChecked {
            from: ctx.accounts.consumer_token_account.to_account_info(),
            mint: ctx.accounts.settlement_mint.to_account_info(),
            to: ctx.accounts.escrow_vault.to_account_info(),
            authority: ctx.accounts.consumer_authority.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token_interface::transfer_checked(cpi_ctx, deposit_amount, ctx.accounts.settlement_mint.decimals)?;

        emit!(ChannelOpenedEvent {
            channel_id,
            deposit_amount,
            consumer: channel.consumer_authority,
        });
        Ok(())
    }

    /// Process Meter Telemetry Tick & Continuously Settle Energy Micro-Transfers
    pub fn settle_meter_tick(
        ctx: Context<SettleMeterTick>,
        kwh_delta: u64,
        unix_timestamp: i64,
    ) -> Result<()> {
        let channel = &mut ctx.accounts.stream_channel;
        require!(!channel.is_closed, KraftwerkError::ChannelClosed);

        let producer_meter = &mut ctx.accounts.producer_meter;
        let consumer_meter = &mut ctx.accounts.consumer_meter;
        let grid_node = &ctx.accounts.grid_node;
        let config = &ctx.accounts.config;

        require!(producer_meter.is_active && consumer_meter.is_active, KraftwerkError::MeterInactive);
        require!(producer_meter.meter_type == MeterType::Producer, KraftwerkError::InvalidMeterConfiguration);
        require!(consumer_meter.meter_type == MeterType::Consumer, KraftwerkError::InvalidMeterConfiguration);

        let current_clock = Clock::get()?;
        require!(
            (current_clock.unix_timestamp - unix_timestamp).abs() < 120,
            KraftwerkError::TelemetryTimestampStale
        );

        // Price calculation: base_rate * (congestion_multiplier_bps / 10000)
        let effective_rate = (grid_node.base_rate_per_kwh as u128)
            .checked_mul(grid_node.congestion_multiplier_bps as u128)
            .ok_or(KraftwerkError::MathOverflow)?
            .checked_div(MAX_BPS as u128)
            .ok_or(KraftwerkError::MathOverflow)?;

        // Settled value = kwh_delta * effective_rate
        let raw_cost = (kwh_delta as u128)
            .checked_mul(effective_rate)
            .ok_or(KraftwerkError::MathOverflow)?
            .checked_div(1_000_000) // Normalized for 6-decimal stablecoins
            .ok_or(KraftwerkError::MathOverflow)? as u64;

        require!(channel.escrow_balance >= raw_cost, KraftwerkError::InsufficientChannelBalance);

        // Protocol fee deduction
        let protocol_fee = (raw_cost as u128)
            .checked_mul(config.protocol_fee_bps as u128)
            .ok_or(KraftwerkError::MathOverflow)?
            .checked_div(MAX_BPS as u128)
            .ok_or(KraftwerkError::MathOverflow)? as u64;
        let net_producer_payment = raw_cost.checked_sub(protocol_fee).ok_or(KraftwerkError::MathOverflow)?;

        // State mutations
        channel.escrow_balance = channel.escrow_balance.checked_sub(raw_cost).ok_or(KraftwerkError::MathOverflow)?;
        channel.total_settled_kwh = channel.total_settled_kwh.checked_add(kwh_delta).ok_or(KraftwerkError::MathOverflow)?;
        channel.last_update_slot = current_clock.slot;

        producer_meter.total_kwh_accumulated = producer_meter
            .total_kwh_accumulated
            .checked_add(kwh_delta)
            .ok_or(KraftwerkError::MathOverflow)?;
        consumer_meter.total_kwh_accumulated = consumer_meter
            .total_kwh_accumulated
            .checked_add(kwh_delta)
            .ok_or(KraftwerkError::MathOverflow)?;

        let config_bump = config.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];

        // Transfer payment to producer
        let cpi_payout = TransferChecked {
            from: ctx.accounts.escrow_vault.to_account_info(),
            mint: ctx.accounts.settlement_mint.to_account_info(),
            to: ctx.accounts.producer_token_account.to_account_info(),
            authority: ctx.accounts.config.to_account_info(),
        };
        let cpi_payout_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_payout,
            signer_seeds,
        );
        token_interface::transfer_checked(cpi_payout_ctx, net_producer_payment, ctx.accounts.settlement_mint.decimals)?;

        // Transfer fee to protocol treasury
        if protocol_fee > 0 {
            let cpi_fee = TransferChecked {
                from: ctx.accounts.escrow_vault.to_account_info(),
                mint: ctx.accounts.settlement_mint.to_account_info(),
                to: ctx.accounts.fee_treasury.to_account_info(),
                authority: ctx.accounts.config.to_account_info(),
            };
            let cpi_fee_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_fee,
                signer_seeds,
            );
            token_interface::transfer_checked(cpi_fee_ctx, protocol_fee, ctx.accounts.settlement_mint.decimals)?;
        }

        emit!(TickSettledEvent {
            channel_id: channel.channel_id,
            kwh_delivered: kwh_delta,
            cleared_payment: raw_cost,
            net_to_producer: net_producer_payment,
            protocol_fee,
        });

        Ok(())
    }

    /// Close Channel and Reclaim Unspent Escrow Balance
    pub fn close_stream_channel(ctx: Context<CloseStreamChannel>) -> Result<()> {
        let channel = &mut ctx.accounts.stream_channel;
        let remaining_balance = channel.escrow_balance;

        channel.is_closed = true;
        channel.escrow_balance = 0;

        if remaining_balance > 0 {
            let config_bump = ctx.accounts.config.bump;
            let signer_seeds: &[&[&[u8]]] = &[&[CONFIG_SEED, &[config_bump]]];

            let cpi_refund = TransferChecked {
                from: ctx.accounts.escrow_vault.to_account_info(),
                mint: ctx.accounts.settlement_mint.to_account_info(),
                to: ctx.accounts.consumer_token_account.to_account_info(),
                authority: ctx.accounts.config.to_account_info(),
            };
            let cpi_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                cpi_refund,
                signer_seeds,
            );
            token_interface::transfer_checked(cpi_ctx, remaining_balance, ctx.accounts.settlement_mint.decimals)?;
        }

        emit!(ChannelClosedEvent {
            channel_id: channel.channel_id,
            refunded_amount: remaining_balance,
        });

        Ok(())
    }

    /// Update Regional Grid Congestion Multiplier (Keeper Loop)
    pub fn update_grid_congestion(
        ctx: Context<UpdateGridCongestion>,
        current_load_kw: u64,
        congestion_multiplier_bps: u64,
    ) -> Result<()> {
        require!(
            congestion_multiplier_bps >= 5_000 && congestion_multiplier_bps <= 50_000,
            KraftwerkError::CongestionMultiplierOutOfBounds
        ); // 0.5x to 5.0x bounds

        let grid_node = &mut ctx.accounts.grid_node;
        grid_node.current_load_kw = current_load_kw;
        grid_node.congestion_multiplier_bps = congestion_multiplier_bps;

        emit!(GridCongestionUpdatedEvent {
            grid_id: grid_node.grid_id,
            current_load_kw,
            congestion_multiplier_bps,
        });

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// ACCOUNT STRUCTS & VALIDATION
// -----------------------------------------------------------------------------

#[derive(Accounts)]
pub fn initialize_protocol<'info>(ctx: Context<InitializeProtocol>) -> Result<()> {
    Ok(())
}

#[derive(Accounts)]
pub struct InitializeProtocol<'info> {
    #[account(
        init,
        payer = admin,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, Config>,
    pub settlement_mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(grid_id: [u8; 16])]
pub struct RegisterGridNode<'info> {
    #[account(
        init,
        payer = operator,
        space = 8 + GridNode::INIT_SPACE,
        seeds = [GRID_NODE_SEED, grid_id.as_ref()],
        bump
    )]
    pub grid_node: Account<'info, GridNode>,
    #[account(mut)]
    pub operator: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(device_pubkey: Pubkey)]
pub struct RegisterSmartMeter<'info> {
    #[account(
        init,
        payer = owner,
        space = 8 + SmartMeter::INIT_SPACE,
        seeds = [METER_SEED, device_pubkey.as_ref()],
        bump
    )]
    pub meter_account: Account<'info, SmartMeter>,
    pub grid_node: Account<'info, GridNode>,
    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(channel_id: u64)]
pub struct OpenStreamChannel<'info> {
    #[account(
        init,
        payer = consumer_authority,
        space = 8 + StreamChannel::INIT_SPACE,
        seeds = [CHANNEL_SEED, channel_id.to_le_bytes().as_ref()],
        bump
    )]
    pub stream_channel: Account<'info, StreamChannel>,
    pub producer_meter: Account<'info, SmartMeter>,
    pub consumer_meter: Account<'info, SmartMeter>,
    #[account(
        mut,
        seeds = [VAULT_SEED, settlement_mint.key().as_ref()],
        bump
    )]
    pub escrow_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub consumer_token_account: InterfaceAccount<'info, TokenAccount>,
    pub settlement_mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub consumer_authority: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SettleMeterTick<'info> {
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump
    )]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [CHANNEL_SEED, stream_channel.channel_id.to_le_bytes().as_ref()],
        bump = stream_channel.bump
    )]
    pub stream_channel: Account<'info, StreamChannel>,
    #[account(
        mut,
        constraint = producer_meter.key() == stream_channel.producer_meter @ KraftwerkError::MeterMismatch
    )]
    pub producer_meter: Account<'info, SmartMeter>,
    #[account(
        mut,
        constraint = consumer_meter.key() == stream_channel.consumer_meter @ KraftwerkError::MeterMismatch
    )]
    pub consumer_meter: Account<'info, SmartMeter>,
    #[account(
        seeds = [GRID_NODE_SEED, grid_node.grid_id.as_ref()],
        bump = grid_node.bump
    )]
    pub grid_node: Account<'info, GridNode>,
    #[account(
        mut,
        seeds = [VAULT_SEED, settlement_mint.key().as_ref()],
        bump
    )]
    pub escrow_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub producer_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub fee_treasury: InterfaceAccount<'info, TokenAccount>,
    #[account(address = config.settlement_mint @ KraftwerkError::InvalidSettlementMint)]
    pub settlement_mint: InterfaceAccount<'info, Mint>,
    /// Hardware attestation signature authority or keeper signer
    pub telemetry_authority: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CloseStreamChannel<'info> {
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump
    )]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [CHANNEL_SEED, stream_channel.channel_id.to_le_bytes().as_ref()],
        bump = stream_channel.bump,
        constraint = stream_channel.consumer_authority == consumer_authority.key() @ KraftwerkError::Unauthorized
    )]
    pub stream_channel: Account<'info, StreamChannel>,
    #[account(
        mut,
        seeds = [VAULT_SEED, settlement_mint.key().as_ref()],
        bump
    )]
    pub escrow_vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub consumer_token_account: InterfaceAccount<'info, TokenAccount>,
    pub settlement_mint: InterfaceAccount<'info, Mint>,
    pub consumer_authority: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct UpdateGridCongestion<'info> {
    #[account(
        mut,
        seeds = [GRID_NODE_SEED, grid_node.grid_id.as_ref()],
        bump = grid_node.bump,
        constraint = grid_node.operator == operator.key() @ KraftwerkError::Unauthorized
    )]
    pub grid_node: Account<'info, GridNode>,
    pub operator: Signer<'info>,
}

// -----------------------------------------------------------------------------
// STATE SCHEMAS
// -----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub operator: Pubkey,
    pub settlement_mint: Pubkey,
    pub total_energy_settled_kwh: u64,
    pub protocol_fee_bps: u16,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct GridNode {
    pub grid_id: [u8; 16],
    pub operator: Pubkey,
    pub base_rate_per_kwh: u64,       // In micro-units (e.g., 300_000 = $0.30/kWh)
    pub max_capacity_kw: u64,
    pub current_load_kw: u64,
    pub congestion_multiplier_bps: u64, // 10000 = 1.0x
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum MeterType {
    Producer,
    Consumer,
    Storage,
}

#[account]
#[derive(InitSpace)]
pub struct SmartMeter {
    pub owner: Pubkey,
    pub device_pubkey: Pubkey,
    pub grid_node: Pubkey,
    pub meter_type: MeterType,
    pub total_kwh_accumulated: u64,
    pub is_active: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct StreamChannel {
    pub channel_id: u64,
    pub producer_meter: Pubkey,
    pub consumer_meter: Pubkey,
    pub consumer_authority: Pubkey,
    pub escrow_balance: u64,
    pub total_settled_kwh: u64,
    pub last_update_slot: u64,
    pub is_closed: bool,
    pub bump: u8,
}

// -----------------------------------------------------------------------------
// EVENTS & ERRORS
// -----------------------------------------------------------------------------

#[event]
pub struct ProtocolInitializedEvent {
    pub admin: Pubkey,
    pub settlement_mint: Pubkey,
    pub protocol_fee_bps: u16,
}

#[event]
pub struct GridNodeRegisteredEvent {
    pub grid_id: [u8; 16],
    pub base_rate_per_kwh: u64,
    pub max_capacity_kw: u64,
}

#[event]
pub struct MeterRegisteredEvent {
    pub owner: Pubkey,
    pub device_pubkey: Pubkey,
    pub meter_type: MeterType,
}

#[event]
pub struct ChannelOpenedEvent {
    pub channel_id: u64,
    pub deposit_amount: u64,
    pub consumer: Pubkey,
}

#[event]
pub struct TickSettledEvent {
    pub channel_id: u64,
    pub kwh_delivered: u64,
    pub cleared_payment: u64,
    pub net_to_producer: u64,
    pub protocol_fee: u64,
}

#[event]
pub struct ChannelClosedEvent {
    pub channel_id: u64,
    pub refunded_amount: u64,
}

#[event]
pub struct GridCongestionUpdatedEvent {
    pub grid_id: [u8; 16],
    pub current_load_kw: u64,
    pub congestion_multiplier_bps: u64,
}

#[error_code]
pub enum KraftwerkError {
    #[msg("Protocol fee exceeds maximum allowed percentage.")]
    FeeExceedsCap,
    #[msg("Zero deposit is not permitted when opening a stream channel.")]
    ZeroDeposit,
    #[msg("The specified streaming channel has already been closed.")]
    ChannelClosed,
    #[msg("Smart meter account is marked inactive.")]
    MeterInactive,
    #[msg("Invalid meter configuration for trade direction.")]
    InvalidMeterConfiguration,
    #[msg("Hardware meter telemetry timestamp is outside allowable skew.")]
    TelemetryTimestampStale,
    #[msg("Escrow account contains insufficient balance to clear tick.")]
    InsufficientChannelBalance,
    #[msg("Math calculation caused an arithmetic overflow/underflow.")]
    MathOverflow,
    #[msg("Smart meter does not match registered channel parties.")]
    MeterMismatch,
    #[msg("Unauthorized execution attempt.")]
    Unauthorized,
    #[msg("Invalid settlement mint address provided.")]
    InvalidSettlementMint,
    #[msg("Grid congestion multiplier outside safety bounds [5000, 50000].")]
    CongestionMultiplierOutOfBounds,
}