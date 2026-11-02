import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Kraftwerk } from "../target/types/kraftwerk";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { Keypair, SystemProgram } from "@solana/web3.js";
import { expect } from "chai";

describe("Kraftwerk: Decentralized Microgrid Settlement Engine", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.Kraftwerk as Program<Kraftwerk>;

  const gridAdmin = Keypair.generate();
  const producer = Keypair.generate();
  const producerGateway = Keypair.generate();
  const consumer = Keypair.generate();

  const gridId = new anchor.BN(4901); // German Grid Area Code: 4901 (NRW)
  const tariffMin = new anchor.BN(50_000); // 0.05 EUR / kWh in micro-EUR
  const tariffMax = new anchor.BN(500_000); // 0.50 EUR / kWh in micro-EUR
  const feeBps = 150; // 1.5% fee

  let gridConfigPda: anchor.web3.PublicKey;
  let producerMeterPda: anchor.web3.PublicKey;
  let consumerMeterPda: anchor.web3.PublicKey;
  let energyEscrowPda: anchor.web3.PublicKey;

  let eurcMint: anchor.web3.PublicKey;
  let escrowVaultTokenAccount: anchor.web3.PublicKey;
  let producerTokenAccount: anchor.web3.PublicKey;
  let adminFeeTokenAccount: anchor.web3.PublicKey;

  before(async () => {
    // Fund all functional wallets
    const airdropSigners = [gridAdmin, producer, producerGateway, consumer];
    for (const signer of airdropSigners) {
      const sig = await provider.connection.requestAirdrop(
        signer.publicKey,
        5 * anchor.web3.LAMPORTS_PER_SOL
      );
      const latestBlockhash = await provider.connection.getLatestBlockhash();
      await provider.connection.confirmTransaction({
        signature: sig,
        ...latestBlockhash,
      });
    }

    // Derive PDAs
    [gridConfigPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("grid-config"), gridId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    [producerMeterPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("meter"), gridConfigPda.toBuffer(), producer.publicKey.toBuffer()],
      program.programId
    );

    [consumerMeterPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("meter"), gridConfigPda.toBuffer(), consumer.publicKey.toBuffer()],
      program.programId
    );

    [energyEscrowPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("energy-escrow"), gridConfigPda.toBuffer(), consumer.publicKey.toBuffer()],
      program.programId
    );

    // Initialize Mint simulating EURC (6 Decimals)
    eurcMint = await createMint(
      provider.connection,
      gridAdmin,
      gridAdmin.publicKey,
      null,
      6
    );

    // Create Token Accounts
    producerTokenAccount = await createAccount(
      provider.connection,
      producer,
      eurcMint,
      producer.publicKey
    );

    adminFeeTokenAccount = await createAccount(
      provider.connection,
      gridAdmin,
      eurcMint,
      gridAdmin.publicKey
    );

    escrowVaultTokenAccount = await createAccount(
      provider.connection,
      consumer,
      eurcMint,
      energyEscrowPda
    );
  });

  it("Initializes a German Regional Grid Configuration", async () => {
    await program.methods
      .initializeGrid(gridId, tariffMin, tariffMax, feeBps)
      .accounts({
        gridConfig: gridConfigPda,
        admin: gridAdmin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([gridAdmin])
      .rpc();

    const config = await program.account.gridConfig.fetch(gridConfigPda);
    expect(config.gridId.toString()).to.equal(gridId.toString());
    expect(config.feeBasisPoints).to.equal(150);
    expect(config.isHalted).to.be.false;
  });

  it("Registers a Prosumer Smart Meter Gateway", async () => {
    await program.methods
      .registerMeter(producerGateway.publicKey, true)
      .accounts({
        meterState: producerMeterPda,
        gridConfig: gridConfigPda,
        owner: producer.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([producer])
      .rpc();

    const meter = await program.account.meterState.fetch(producerMeterPda);
    expect(meter.isProducer).to.be.true;
    expect(meter.gatewayPubkey.toBase58()).to.equal(producerGateway.publicKey.toBase58());
    expect(meter.unallocatedEnergyWh.toNumber()).to.equal(0);
  });

  it("Ingests Verified Smart Meter Telemetry and Increments Available Green Power", async () => {
    const clock = Math.floor(Date.now() / 1000);
    const cumulativeReadingWh = new anchor.BN(125_000); // 125 kWh injected to grid

    await program.methods
      .recordTelemetry(cumulativeReadingWh, new anchor.BN(clock))
      .accounts({
        meterState: producerMeterPda,
        gridConfig: gridConfigPda,
        gatewayPubkey: producerGateway.publicKey,
      })
      .signers([producerGateway])
      .rpc();

    const meter = await program.account.meterState.fetch(producerMeterPda);
    expect(meter.totalEnergyRecordedWh.toString()).to.equal("125000");
    expect(meter.unallocatedEnergyWh.toString()).to.equal("125000");
  });

  it("Executes Atomic Sub-Second P2P Settlement with Direct Token Escrow Transfer", async () => {
    // Fund Escrow Vault with 50 EURC (50_000_000 micro-units)
    await mintTo(
      provider.connection,
      gridAdmin,
      eurcMint,
      escrowVaultTokenAccount,
      gridAdmin,
      50_000_000
    );

    // Consumer settles purchase of 25 kWh (25,000 Wh) @ 0.20 EUR/kWh (200,000 micro-EUR)
    const tradeVolumeWh = new anchor.BN(25_000);
    const clearingRateMicroEur = new anchor.BN(200_000);

    // Expected:
    // Gross: (25000 * 200000) / 1000000 = 5,000,000 micro-EUR (5.00 EURC)
    // Fee (1.5%): 75,000 micro-EUR
    // Net to Producer: 4,925,000 micro-EUR
    await program.methods
      .settleEnergyTrade(tradeVolumeWh, clearingRateMicroEur)
      .accounts({
        gridConfig: gridConfigPda,
        producerMeter: producerMeterPda,
        energyEscrow: energyEscrowPda,
        escrowVault: escrowVaultTokenAccount,
        producerTokenAccount: producerTokenAccount,
        adminFeeTokenAccount: adminFeeTokenAccount,
        consumer: consumer.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([consumer])
      .rpc();

    // Verify producer meter account state deducted
    const meter = await program.account.meterState.fetch(producerMeterPda);
    expect(meter.unallocatedEnergyWh.toNumber()).to.equal(100_000); // 125,000 - 25,000

    // Verify token vault balances
    const producerTokenBalance = await getAccount(provider.connection, producerTokenAccount);
    const adminTokenBalance = await getAccount(provider.connection, adminFeeTokenAccount);

    expect(Number(producerTokenBalance.amount)).to.equal(4_925_000);
    expect(Number(adminTokenBalance.amount)).to.equal(75_000);
  });

  it("Rejects Stale or Non-Monotonic Telemetry Readings (BSI Security Guard)", async () => {
    const invalidPastTimestamp = new anchor.BN(1000);
    const lowerReadingWh = new anchor.BN(90_000);

    try {
      await program.methods
        .recordTelemetry(lowerReadingWh, invalidPastTimestamp)
        .accounts({
          meterState: producerMeterPda,
          gridConfig: gridConfigPda,
          gatewayPubkey: producerGateway.publicKey,
        })
        .signers([producerGateway])
        .rpc();
      expect.fail("Telemetry with past timestamp should have failed");
    } catch (err: any) {
      expect(err.toString()).to.include("InvalidTimestamp");
    }
  });
});