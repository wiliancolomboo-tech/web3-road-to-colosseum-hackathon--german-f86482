use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    ed25519_program,
    instruction::Instruction,
    sysvar::instructions::{load_current_index_checked, load_instruction_at_checked},
};
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

declare_id!("KRFThE78Z4bB2XoX1DMWj3tK37xN2yB9z1cW88v7JgQ");

pub const GRID_CONFIG_SEED: &[u8] = b"grid_config";
pub const DEVICE_SEED: &[u8] = b"device";
pub const EPOCH_VAULT_SEED: &[u8] = b"epoch_vault";
pub const ESCROW_SEED: &[u8] = b"escrow";

pub const MAX_DISCHARGE_RATE_WATT_HOURS: u64 = 50_000; // 50 kWh max burst per single dispatch slot
pub const BASIS_POINTS_DIVISOR: u64 = 10_000;
pub const PROTOCOL_FEE_BPS: u64 = 500; // 5% fee on captured arbitrage

#[program]
pub mut kraftwerk {
    use super::*;

    /// Initializes a German Regional Grid Balancer configuration (e.g. Amprion, TenneT zones).
    pub fn initialize_grid_config(
        ctx: Context<InitializeGridConfig>,
        zone_id: [u8; 8],
        base_feedin_rate: u64, // Micro-EURC per kWh
    ) -> Result<()> {
        let config = &mut ctx.accounts.grid_config;
        config.admin = ctx.accounts.admin.key();
        config.zone_id = zone_id;
        config.settlement_mint = ctx.accounts.settlement_mint.key();
        config.escrow_vault = ctx.accounts.escrow_vault.key();
        config.current_clearing_price = base_feedin_rate;
        config.active_devices = 0;
        config.total_capacity_wh = 0;
        config.is_emergency_paused = false;
        config.bump = ctx.bumps.grid_config;
        config.escrow_bump = ctx.bumps.escrow_vault;

        emit!(GridConfigInitialized {
            zone_id,
            admin: config.admin,
            clearing_price: base_feedin_rate,
        });

        Ok(())
    }

    /// Registers a residential or industrial battery unit equipped with a cryptographic hardware meter.
    pub fn register_device(
        ctx: Context<RegisterDevice>,
        device_uuid: [u8; 16],
        hardware_pubkey: [u8; 32],
        max_storage_capacity_wh: u64,
    ) -> Result<()> {
        require!(max_storage_capacity_wh > 0, KraftWerkError::InvalidDeviceCapacity);

        let device = &mut ctx.accounts.device_registry;
        device.owner = ctx.accounts.owner.key();
        device.device_uuid = device_uuid;
        device.hardware_pubkey = hardware_pubkey;
        device.max_storage_capacity_wh = max_storage_capacity_wh;
        device.cumulative_wh_discharged = 0;
        device.last_telemetry_timestamp = Clock::get()?.unix_timestamp;
        device.nonce = 0;
        device.is_active = true;
        device.bump = ctx.bumps.device_registry;

        let config = &mut ctx.accounts.grid_config;
        config.active_devices = config.active_devices.checked_add(1).ok_or(KraftWerkError::MathOverflow)?;
        config.total_capacity_wh = config.total_capacity_wh
            .checked_add(max_storage_capacity_wh)
            .ok_or(KraftWerkError::MathOverflow)?;

        emit!(DeviceRegistered {
            owner: device.owner,
            device_uuid,
            capacity_wh: max_storage_capacity_wh,
        });

        Ok(())
    }

    /// Updates dynamic price tick from authorized grid oracle (EPEX 15-minute intraday tick).
    pub fn publish_price_tick(
        ctx: Context<PublishPriceTick>,
        new_clearing_price: u64,
        epoch_id: u64,
    ) -> Result<()> {
        let config = &mut ctx.accounts.grid_config;
        require!(!config.is_emergency_paused, KraftWerkError::EmergencyPaused);
        require!(ctx.accounts.oracle_authority.key() == config.admin, KraftWerkError::UnauthorizedOracle);

        config.current_clearing_price = new_clearing_price;

        let epoch_vault = &mut ctx.accounts.epoch_vault;
        if epoch_vault.epoch_id == 0 {
            epoch_vault.epoch_id = epoch_id;
            epoch_vault.clearing_price = new_clearing_price;
            epoch_vault.total_energy_settled_wh = 0;
            epoch_vault.total_payout_amount = 0;
            epoch_vault.bump = ctx.bumps.epoch_vault;
        }

        emit!(PriceUpdated {
            epoch_id,
            clearing_price: new_clearing_price,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Verifies hardware meter discharge telemetry via pre-instruction Ed25519 verification
    /// and performs atomic micro-settlement from the escrow pool to the device beneficiary.
    pub fn verify_telemetry_and_settle(
        ctx: Context<VerifyTelemetryAndSettle>,
        watt_hours_delta: u64,
        telemetry_timestamp: i64,
        telemetry_nonce: u64,
    ) -> Result<()> {
        let config = &ctx.accounts.grid_config;
        let device = &mut ctx.accounts.device_registry;
        let epoch_vault = &mut ctx.accounts.epoch_vault;

        require!(!config.is_emergency_paused, KraftWerkError::EmergencyPaused);
        require!(device.is_active, KraftWerkError::DeviceInactive);
        require!(telemetry_nonce > device.nonce, KraftWerkError::StaleTelemetryNonce);
        require!(watt_hours_delta > 0, KraftWerkError::ZeroDischargeAmount);
        require!(
            watt_hours_delta <= MAX_DISCHARGE_RATE_WATT_HOURS,
            KraftWerkError::DischargeRateExceeded
        );

        let clock = Clock::get()?;
        require!(
            telemetry_timestamp <= clock.unix_timestamp &&
            telemetry_timestamp >= clock.unix_timestamp - 300,
            KraftWerkError::InvalidTelemetryWindow
        );

        // Security: Introspect Instruction Sysvar to assert that an Ed25519Program verification
        // instruction immediately preceded this instruction in the same transaction.
        verify_hardware_test_000000000000000000000000(
            &ctx.accounts.instructions_sysvar,
            &device.hardware_pubkey,
            watt_hours_delta,
            telemetry_timestamp,
            telemetry_nonce,
        )?;

        // Calculate Payout: (watt_hours_delta * clearing_price) / 1,000 (Watt to kWh conversion)
        // clearing_price is denominated in Micro-EURC per kWh (1 EURC = 1,000,000 micro-units)
        let gross_payout = (watt_hours_delta as u128)
            .checked_mul(config.current_clearing_price as u128)
            .ok_or(KraftWerkError::MathOverflow)?
            .checked_div(1_000)
            .ok_or(KraftWerkError::MathOverflow)? as u64;

        let protocol_cut = gross_payout
            .checked_mul(PROTOCOL_FEE_BPS)
            .ok_or(KraftWerkError::MathOverflow)?
            .checked_div(BASIS_POINTS_DIVISOR)
            .ok_or(KraftWerkError::MathOverflow)?;

        let net_beneficiary_payout = gross_payout
            .checked_sub(protocol_cut)
            .ok_or(KraftWerkError::MathOverflow)?;

        // Update State
        device.cumulative_wh_discharged = device.cumulative_wh_discharged
            .checked_add(watt_hours_delta)
            .ok_or(KraftWerkError::MathOverflow)?;
        device.last_telemetry_timestamp = telemetry_timestamp;
        device.nonce = telemetry_nonce;

        epoch_vault.total_energy_settled_wh = epoch_vault.total_energy_settled_wh
            .checked_add(watt_hours_delta)
            .ok_or(KraftWerkError::MathOverflow)?;
        epoch_vault.total_payout_amount = epoch_vault.total_payout_amount
            .checked_add(net_beneficiary_payout)
            .ok_or(KraftWerkError::MathOverflow)?;

        // Execute CPI Token Transfers using Escrow PDA signer seeds
        let zone_id = config.zone_id;
        let bump = config.escrow_bump;
        let seeds: &[&[&[u8]]] = &[&[
            ESCROW_SEED,
            zone_id.as_ref(),
            &[bump],
        ]];

        // 1. Transfer Net Yield to Inverter Owner ATA
        let transfer_user_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.escrow_vault.to_account_info(),
                to: ctx.accounts.owner_token_account.to_account_info(),
                authority: ctx.accounts.escrow_vault.to_account_info(),
            },
            seeds,
        );
        token::transfer(transfer_user_ctx, net_beneficiary_payout)?;

        // 2. Transfer Protocol Fee to Protocol Treasury ATA
        if protocol_cut > 0 {
            let transfer_fee_ctx = CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.escrow_vault.to_account_info(),
                    to: ctx.accounts.treasury_token_account.to_account_info(),
                    authority: ctx.accounts.escrow_vault.to_account_info(),
                },
                seeds,
            );
            token::transfer(transfer_fee_ctx, protocol_cut)?;
        }

        emit!(TelemetrySettled {
            device_uuid: device.device_uuid,
            watt_hours_discharged: watt_hours_delta,
            payout_micro_eurc: net_beneficiary_payout,
            protocol_fee: protocol_cut,
            nonce: telemetry_nonce,
        });

        Ok(())
    }

    /// Toggles circuit breaker in case of grid detachment or anomalous EPEX frequency swing.
    pub fn set_circuit_breaker(ctx: Context<SetCircuitBreaker>, paused: bool) -> Result<()> {
        let config = &mut ctx.accounts.grid_config;
        config.is_emergency_paused = paused;
        emit!(CircuitBreakerTripped { is_paused: paused });
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Telemetry Instruction Introspection
// -----------------------------------------------------------------------------

fn verify_hardware_test_000000000000000000000000(
    instructions_sysvar: &AccountInfo,
    expected_hardware_pubkey: &[u8; 32],
    wh_delta: u64,
    timestamp: i64,
    nonce: u64,
) -> Result<()> {
    let current_index = load_current_index_checked(instructions_sysvar)?;
    require!(current_index > 0, KraftWerkError::MissingEd25519VerificationPreInstruction);

    let ed25519_ix = load_instruction_at_checked((current_index - 1) as usize, instructions_sysvar)?;
    require!(
        ed25519_ix.program_id == ed25519_program::id(),
        KraftWerkError::InvalidEd25519ProgramId
    );

    // Expected byte payload layout: [wh_delta (8 bytes LE) | timestamp (8 bytes LE) | nonce (8 bytes LE)] = 24 bytes
    let mut expected_message = [0u8; 24];
    expected_message[0..8].copy_from_slice(&wh_delta.to_le_bytes());
    expected_message[8..16].copy_from_slice(&timestamp.to_le_bytes());
    expected_message[16..24].copy_from_slice(&nonce.to_le_bytes());

    parse_and_validate_ed25519_data(&ed25519_ix, expected_hardware_pubkey, &expected_message)
}

fn parse_and_validate_ed25519_data(
    ix: &Instruction,
    expected_pubkey: &[u8; 32],
    expected_message: &[u8; 24],
) -> Result<()> {
    let data = &ix.data;
    // Ed25519 instruction data layout header is at least 14 bytes + signature offsets
    require!(data.len() >= 112, KraftWerkError::MalformedEd25519Instruction);

    // Number of signatures in the batch (byte 0)
    let num_signatures = data[0];
    require!(num_signatures == 1, KraftWerkError::InvalidSignatureCount);

    // Offsets according to Solana Ed25519 program specs
    let pubkey_offset = u16::from_le_bytes([data[4], data[5]]) as usize;
    let msg_offset = u16::from_le_bytes([data[8], data[9]]) as usize;
    let msg_size = u16::from_le_bytes([data[10], data[11]]) as usize;

    require!(msg_size == 24, KraftWerkError::InvalidTelemetryPayloadSize);
    require!(
        data.len() >= pubkey_offset + 32 && data.len() >= msg_offset + msg_size,
        KraftWerkError::MalformedEd25519Offsets
    );

    let extracted_pubkey = &data[pubkey_offset..pubkey_offset + 32];
    let extracted_msg = &data[msg_offset..msg_offset + msg_size];

    require!(
        extracted_pubkey == expected_pubkey,
        KraftWerkError::HardwareKeyMismatch
    );
    require!(
        extracted_msg == expected_message,
        KraftWerkError::TelemetryDataTampered
    );

    Ok(())
}

// -----------------------------------------------------------------------------
// Account Contexts & Validation
// -----------------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(zone_id: [u8; 8])]
pub struct InitializeGridConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + GridConfig::INIT_SPACE,
        seeds = [GRID_CONFIG_SEED, zone_id.as_ref()],
        bump
    )]
    pub grid_config: Account<'info, GridConfig>,

    pub settlement_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        seeds = [ESCROW_SEED, zone_id.as_ref()],
        bump,
        token::mint = settlement_mint,
        token::authority = escrow_vault,
    )]
    pub escrow_vault: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(device_uuid: [u8; 16])]
pub struct RegisterDevice<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [GRID_CONFIG_SEED, grid_config.zone_id.as_ref()],
        bump = grid_config.bump
    )]
    pub grid_config: Account<'info, GridConfig>,

    #[account(
        init,
        payer = owner,
        space = 8 + DeviceRegistry::INIT_SPACE,
        seeds = [DEVICE_SEED, owner.key().as_ref(), device_uuid.as_ref()],
        bump
    )]
    pub device_registry: Account<'info, DeviceRegistry>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(new_clearing_price: u64, epoch_id: u64)]
pub struct PublishPriceTick<'info> {
    #[account(mut)]
    pub oracle_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [GRID_CONFIG_SEED, grid_config.zone_id.as_ref()],
        bump = grid_config.bump,
        has_one = admin @ KraftWerkError::UnauthorizedOracle
    )]
    pub grid_config: Account<'info, GridConfig>,

    pub admin: SystemAccount<'info>,

    #[account(
        init_if_needed,
        payer = oracle_authority,
        space = 8 + EpochYieldVault::INIT_SPACE,
        seeds = [EPOCH_VAULT_SEED, grid_config.zone_id.as_ref(), &epoch_id.to_le_bytes()],
        bump
    )]
    pub epoch_vault: Account<'info, EpochYieldVault>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct VerifyTelemetryAndSettle<'info> {
    #[account(mut)]
    pub keeper_or_owner: Signer<'info>,

    #[account(
        seeds = [GRID_CONFIG_SEED, grid_config.zone_id.as_ref()],
        bump = grid_config.bump
    )]
    pub grid_config: Account<'info, GridConfig>,

    #[account(
        mut,
        seeds = [DEVICE_SEED, device_registry.owner.as_ref(), device_registry.device_uuid.as_ref()],
        bump = device_registry.bump
    )]
    pub device_registry: Account<'info, DeviceRegistry>,

    #[account(
        mut,
        seeds = [EPOCH_VAULT_SEED, grid_config.zone_id.as_ref(), &epoch_vault.epoch_id.to_le_bytes()],
        bump = epoch_vault.bump
    )]
    pub epoch_vault: Account<'info, EpochYieldVault>,

    #[account(
        mut,
        seeds = [ESCROW_SEED, grid_config.zone_id.as_ref()],
        bump = grid_config.escrow_bump
    )]
    pub escrow_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = owner_token_account.owner == device_registry.owner @ KraftWerkError::InvalidTokenBeneficiary,
        constraint = owner_token_account.mint == grid_config.settlement_mint @ KraftWerkError::InvalidMint
    )]
    pub owner_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = treasury_token_account.mint == grid_config.settlement_mint @ KraftWerkError::InvalidMint
    )]
    pub treasury_token_account: Account<'info, TokenAccount>,

    /// CHECK: Instructions sysvar checked via load_current_index_checked
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SetCircuitBreaker<'info> {
    #[account(
        mut,
        has_one = admin @ KraftWerkError::UnauthorizedAdmin
    )]
    pub grid_config: Account<'info, GridConfig>,
    pub admin: Signer<'info>,
}

// -----------------------------------------------------------------------------
// State Structs
// -----------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct GridConfig {
    pub admin: Pubkey,
    pub zone_id: [u8; 8],
    pub settlement_mint: Pubkey,
    pub escrow_vault: Pubkey,
    pub current_clearing_price: u64, // Micro-EURC per kWh
    pub active_devices: u64,
    pub total_capacity_wh: u64,
    pub is_emergency_paused: bool,
    pub bump: u8,
    pub escrow_bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct DeviceRegistry {
    pub owner: Pubkey,
    pub device_uuid: [u8; 16],
    pub hardware_pubkey: [u8; 32],
    pub max_storage_capacity_wh: u64,
    pub cumulative_wh_discharged: u64,
    pub last_telemetry_timestamp: i64,
    pub nonce: u64,
    pub is_active: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct EpochYieldVault {
    pub epoch_id: u64,
    pub clearing_price: u64,
    pub total_energy_settled_wh: u64,
    pub total_payout_amount: u64,
    pub bump: u8,
}

// -----------------------------------------------------------------------------
// Events
// -----------------------------------------------------------------------------

#[event]
pub struct GridConfigInitialized {
    pub zone_id: [u8; 8],
    pub admin: Pubkey,
    pub clearing_price: u64,
}

#[event]
pub struct DeviceRegistered {
    pub owner: Pubkey,
    pub device_uuid: [u8; 16],
    pub capacity_wh: u64,
}

#[event]
pub struct PriceUpdated {
    pub epoch_id: u64,
    pub clearing_price: u64,
    pub timestamp: i64,
}

#[event]
pub struct TelemetrySettled {
    pub device_uuid: [u8; 16],
    pub watt_hours_discharged: u64,
    pub payout_micro_eurc: u64,
    pub protocol_fee: u64,
    pub nonce: u64,
}

#[event]
pub struct CircuitBreakerTripped {
    pub is_paused: bool,
}

// -----------------------------------------------------------------------------
// Error Codes
// -----------------------------------------------------------------------------

#[error_code]
pub enum KraftWerkError {
    #[msg("Calculation triggered mathematical overflow.")]
    MathOverflow,
    #[msg("Provided storage capacity must be strictly greater than 0.")]
    InvalidDeviceCapacity,
    #[msg("Grid configuration has paused operations.")]
    EmergencyPaused,
    #[msg("Caller is not authorized as oracle coordinator.")]
    UnauthorizedOracle,
    #[msg("Signer is not authorized grid administrator.")]
    UnauthorizedAdmin,
    #[msg("Telemetry device status is marked as inactive.")]
    DeviceInactive,
    #[msg("Provided telemetry nonce is stale or replay attacked.")]
    StaleTelemetryNonce,
    #[msg("Reported discharge amount is zero.")]
    ZeroDischargeAmount,
    #[msg("Telemetry watt-hour burst exceeds single-slot physical safety boundary.")]
    DischargeRateExceeded,
    #[msg("Telemetry timestamp is outside of the accepted delta window.")]
    InvalidTelemetryWindow,
    #[msg("Preceding Ed25519 verification instruction is missing.")]
    MissingEd25519VerificationPreInstruction,
    #[msg("Program ID does not match Ed25519 Native Program.")]
    InvalidEd25519ProgramId,
    #[msg("Ed25519 payload failed byte structure validation.")]
    MalformedEd25519Instruction,
    #[msg("Expected exactly 1 signature instruction entry.")]
    InvalidSignatureCount,
    #[msg("Telemetry data length does not match expected byte payload.")]
    InvalidTelemetryPayloadSize,
    #[msg("Ed25519 byte buffer offsets out of memory slice.")]
    MalformedEd25519Offsets,
    #[msg("Signer key from Ed25519 check does not match device hardware key.")]
    HardwareKeyMismatch,
    #[msg("Decoded telemetry contents do not match instruction payload arguments.")]
    TelemetryDataTampered,
    #[msg("Settlement recipient token account owner must match device registry owner.")]
    InvalidTokenBeneficiary,
    #[msg("Token account mint does not conform to GridConfig EURC mint.")]
    InvalidMint,
}