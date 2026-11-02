import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Kraftwerk } from "../target/types/kraftwerk";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  Ed25519Program,
  sendAndConfirmTransaction,
  Transaction,
} from "@solana/web3.js";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { expect } from "chai";
import * as nacl from "tweetnacl";

describe("KraftWerk: Decentralized Virtual Power Plant Test Suite", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.Kraftwerk as Program<Kraftwerk>;

  const admin = Keypair.generate();
  const batteryOwner = Keypair.generate();
  const keeper = Keypair.generate();
  const treasuryOwner = Keypair.generate();

  // Hardware secure element mock keypair
  const hardwareEnclave = nacl.sign.keyPair();

  // Zone ID: "GER_AMP1" (Amprion Grid Sector 1)
  const zoneId = Array.from(Buffer.from("GER_AMP1"));
  const deviceUuid = Array.from(Buffer.from("BESS_DE_88492011")); // 16 bytes

  let eurcMint: PublicKey;
  let gridConfigPda: PublicKey;
  let escrowVaultPda: PublicKey;
  let deviceRegistryPda: PublicKey;
  let epochVaultPda: PublicKey;
  let ownerEurcAta: PublicKey;
  let treasuryEurcAta: PublicKey;

  const initialClearingPrice = new anchor.BN(450_000); // 0.45 EURC per kWh
  const epochId = new anchor.BN(20241018);

  before(async () => {
    // Airdrop SOL to actors
    for (const actor of [admin, batteryOwner, keeper, treasuryOwner]) {
      const sig = await provider.connection.requestAirdrop(actor.publicKey, 10 * anchor.web3.LAMPORTS_PER_SOL);
      const latestBlockhash = await provider.connection.getLatestBlockhash();
      await provider.connection.confirmTransaction({
        signature: sig,
        ...latestBlockhash,
      });
    }

    // Initialize mock EURC token mint
    eurcMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6
    );

    // Derive PDAs
    [gridConfigPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("grid_config"), Buffer.from(zoneId)],
      program.programId
    );

    [escrowVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("escrow"), Buffer.from(zoneId)],
      program.programId
    );

    [deviceRegistryPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("device"), batteryOwner.publicKey.toBuffer(), Buffer.from(deviceUuid)],
      program.programId
    );

    [epochVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("epoch_vault"), Buffer.from(zoneId), epochId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    // Create Token Accounts
    ownerEurcAta = await createAccount(
      provider.connection,
      batteryOwner,
      eurcMint,
      batteryOwner.publicKey
    );

    treasuryEurcAta = await createAccount(
      provider.connection,
      treasuryOwner,
      eurcMint,
      treasuryOwner.publicKey
    );
  });

  it("1. Initializes German Grid Balancing Config & Escrow Vault", async () => {
    await program.methods
      .initializeGridConfig(zoneId, initialClearingPrice)
      .accountsStrict({
        admin: admin.publicKey,
        gridConfig: gridConfigPda,
        settlementMint: eurcMint,
        escrowVault: escrowVaultPda,
        systemProgram: SystemProgram.programId,
        tokenProgram: TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .signers([admin])
      .rpc();

    // Fund the Escrow Vault with 50,000 EURC (50,000 * 10^6 units)
    await mintTo(
      provider.connection,
      admin,
      eurcMint,
      escrowVaultPda,
      admin,
      50_000 * 1_000_000
    );

    const configAccount = await program.account.gridConfig.fetch(gridConfigPda);
    expect(configAccount.currentClearingPrice.toNumber()).to.equal(450_000);
    expect(configAccount.isEmergencyPaused).to.be.false;
  });

  it("2. Registers Residential BESS Inverter with Hardware Enclave Public Key", async () => {
    const maxCapacityWh = new anchor.BN(15_000); // 15 kWh battery

    await program.methods
      .registerDevice(deviceUuid, Array.from(hardwareEnclave.publicKey), maxCapacityWh)
      .accountsStrict({
        owner: batteryOwner.publicKey,
        gridConfig: gridConfigPda,
        deviceRegistry: deviceRegistryPda,
        systemProgram: SystemProgram.programId,
      })
      .signers([batteryOwner])
      .rpc();

    const device = await program.account.deviceRegistry.fetch(deviceRegistryPda);
    expect(device.owner.toBase58()).to.equal(batteryOwner.publicKey.toBase58());
    expect(device.maxStorageCapacityWh.toNumber()).to.equal(15_000);
    expect(device.cumulativeWhDischarged.toNumber()).to.equal(0);
    expect(device.nonce.toNumber()).to.equal(0);
  });

  it("3. Publishes High-Frequency Intraday EPEX Price Tick", async () => {
    const spikeClearingPrice = new anchor.BN(680_000); // Spike to 0.68 EURC/kWh during dunkelflaute

    await program.methods
      .publishPriceTick(spikeClearingPrice, epochId)
      .accountsStrict({
        oracleAuthority: admin.publicKey,
        gridConfig: gridConfigPda,
        admin: admin.publicKey,
        epochVault: epochVaultPda,
        systemProgram: SystemProgram.programId,
      })
      .signers([admin])
      .rpc();

    const config = await program.account.gridConfig.fetch(gridConfigPda);
    expect(config.currentClearingPrice.toNumber()).to.equal(680_000);
  });

  it("4. Discharges Battery, Verifies Hardware Signature via Ed25519 & Settles Instantly", async () => {
    const wattHoursDelta = new anchor.BN(8_500); // 8.5 kWh delivered
    const currentBlockTime = (await provider.connection.getBlockTime(
      await provider.connection.getSlot()
    )) || Math.floor(Date.now() / 1000);
    const telemetryTimestamp = new anchor.BN(currentBlockTime);
    const telemetryNonce = new anchor.BN(1);

    // Construct raw binary payload matching contract expectations
    // [wh_delta (8B LE) | timestamp (8B LE) | nonce (8B LE)] = 24 bytes
    const msgBuffer = Buffer.alloc(24);
    msgBuffer.writeBigUInt64LE(BigInt(wattHoursDelta.toString()), 0);
    msgBuffer.writeBigInt64LE(BigInt(telemetryTimestamp.toString()), 8);
    msgBuffer.writeBigUInt64LE(BigInt(telemetryNonce.toString()), 16);

    // Sign payload with hardware enclave secret key
    const signature = nacl.sign.detached(msgBuffer, hardwareEnclave.secretKey);

    // Create Ed25519Program Instruction
    const ed25519Ix = Ed25519Program.createInstructionWithPublicKey({
      publicKey: Buffer.from(hardwareEnclave.publicKey),
      message: msgBuffer,
      signature: Buffer.from(signature),
    });

    // Create Anchor Settlement Instruction
    const settleIx = await program.methods
      .verifyTelemetryAndSettle(wattHoursDelta, telemetryTimestamp, telemetryNonce)
      .accountsStrict({
        keeperOrOwner: keeper.publicKey,
        gridConfig: gridConfigPda,
        deviceRegistry: deviceRegistryPda,
        epochVault: epochVaultPda,
        escrowVault: escrowVaultPda,
        ownerTokenAccount: ownerEurcAta,
        treasuryTokenAccount: treasuryEurcAta,
        instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .instruction();

    // Bundle instructions into single atomic transaction
    const tx = new Transaction().add(ed25519Ix).add(settleIx);
    await sendAndConfirmTransaction(provider.connection, tx, [keeper]);

    // Validate balances
    // Clearing price = 680,000 micro-EURC / kWh
    // 8,500 Wh = 8.5 kWh
    // Gross payout = 8.5 * 680,000 = 5,780,000 micro-units (5.78 EURC)
    // Fee = 5% = 289,000 micro-units
    // Net beneficiary = 5,491,000 micro-units
    const ownerAtaAccount = await getAccount(provider.connection, ownerEurcAta);
    expect(Number(ownerAtaAccount.amount)).to.equal(5_491_000);

    const treasuryAtaAccount = await getAccount(provider.connection, treasuryEurcAta);
    expect(Number(treasuryAtaAccount.amount)).to.equal(289_000);

    const updatedDevice = await program.account.deviceRegistry.fetch(deviceRegistryPda);
    expect(updatedDevice.cumulativeWhDischarged.toNumber()).to.equal(8_500);
    expect(updatedDevice.nonce.toNumber()).to.equal(1);
  });

  it("5. Replay Attack Fails: Attempting to settle using same nonce", async () => {
    const wattHoursDelta = new anchor.BN(8_500);
    const telemetryTimestamp = new anchor.BN(Math.floor(Date.now() / 1000));
    const staleNonce = new anchor.BN(1); // Already processed

    const msgBuffer = Buffer.alloc(24);
    msgBuffer.writeBigUInt64LE(BigInt(wattHoursDelta.toString()), 0);
    msgBuffer.writeBigInt64LE(BigInt(telemetryTimestamp.toString()), 8);
    msgBuffer.writeBigUInt64LE(BigInt(staleNonce.toString()), 16);

    const signature = nacl.sign.detached(msgBuffer, hardwareEnclave.secretKey);
    const ed25519Ix = Ed25519Program.createInstructionWithPublicKey({
      publicKey: Buffer.from(hardwareEnclave.publicKey),
      message: msgBuffer,
      signature: Buffer.from(signature),
    });

    const settleIx = await program.methods
      .verifyTelemetryAndSettle(wattHoursDelta, telemetryTimestamp, staleNonce)
      .accountsStrict({
        keeperOrOwner: keeper.publicKey,
        gridConfig: gridConfigPda,
        deviceRegistry: deviceRegistryPda,
        epochVault: epochVaultPda,
        escrowVault: escrowVaultPda,
        ownerTokenAccount: ownerEurcAta,
        treasuryTokenAccount: treasuryEurcAta,
        instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .instruction();

    const tx = new Transaction().add(ed25519Ix).add(settleIx);

    try {
      await sendAndConfirmTransaction(provider.connection, tx, [keeper]);
      expect.fail("Transaction should have failed with StaleTelemetryNonce");
    } catch (err: any) {
      expect(err.toString()).to.include("StaleTelemetryNonce");
    }
  });

  it("6. Trips Safety Circuit Breaker and Blocks Settlement", async () => {
    await program.methods
      .setCircuitBreaker(true)
      .accountsStrict({
        gridConfig: gridConfigPda,
        admin: admin.publicKey,
      })
      .signers([admin])
      .rpc();

    const config = await program.account.gridConfig.fetch(gridConfigPda);
    expect(config.isEmergencyPaused).to.be.true;
  });
});