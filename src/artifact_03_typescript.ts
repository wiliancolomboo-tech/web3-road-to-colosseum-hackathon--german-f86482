import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Kraftwerk } from "../target/types/kraftwerk";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
  TOKEN_2022_PROGRAM_ID,
} from "@solana/spl-token";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { expect } from "chai";

describe("KRAFTWERK Protocol E2E Verification Matrix", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.Kraftwerk as Program<Kraftwerk>;
  const admin = (provider.wallet as anchor.Wallet).payer;

  let settlementMint: PublicKey;
  let escrowVaultPda: PublicKey;
  let configPda: PublicKey;

  const operator = Keypair.generate();
  const producerOwner = Keypair.generate();
  const producerDevice = Keypair.generate();
  const consumerOwner = Keypair.generate();
  const consumerDevice = Keypair.generate();
  const feeTreasuryKeypair = Keypair.generate();

  let feeTreasuryAta: PublicKey;
  let consumerAta: PublicKey;
  let producerAta: PublicKey;

  const gridId = Array.from(Buffer.from("BERLIN_MITTE_01")); // 16-byte grid id
  let gridNodePda: PublicKey;
  let producerMeterPda: PublicKey;
  let consumerMeterPda: PublicKey;

  const channelId = new anchor.BN(42069);
  let channelPda: PublicKey;

  before(async () => {
    // 1. Airdrop SOL to participants
    const fundAccounts = [operator, producerOwner, consumerOwner];
    for (const acc of fundAccounts) {
      const sig = await provider.connection.requestAirdrop(acc.publicKey, 5 * anchor.web3.LAMPORTS_PER_SOL);
      await provider.connection.confirmTransaction(sig);
    }

    // 2. Deploy Mock Token-2022 Stablecoin (EURk - 6 Decimals)
    settlementMint = await createMint(
      provider.connection,
      admin,
      admin.publicKey,
      null,
      6,
      Keypair.generate(),
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // 3. Derive Protocol PDAs
    [configPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    );

    [escrowVaultPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault"), settlementMint.toBuffer()],
      program.programId
    );

    [gridNodePda] = PublicKey.findProgramAddressSync(
      [Buffer.from("grid_node"), Buffer.from(gridId)],
      program.programId
    );

    [producerMeterPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("meter"), producerDevice.publicKey.toBuffer()],
      program.programId
    );

    [consumerMeterPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("meter"), consumerDevice.publicKey.toBuffer()],
      program.programId
    );

    [channelPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("channel"), channelId.toArrayLike(Buffer, "le", 8)],
      program.programId
    );

    // 4. Create Token Accounts
    feeTreasuryAta = await createAccount(
      provider.connection,
      admin,
      settlementMint,
      feeTreasuryKeypair.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    consumerAta = await createAccount(
      provider.connection,
      consumerOwner,
      settlementMint,
      consumerOwner.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    producerAta = await createAccount(
      provider.connection,
      producerOwner,
      settlementMint,
      producerOwner.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // 5. Mint 1,000 EURk ($1000.000000) to Consumer
    await mintTo(
      provider.connection,
      admin,
      settlementMint,
      consumerAta,
      admin,
      1_000_000_000,
      [],
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
  });

  it("Step 1: Initializes Global Protocol Configuration", async () => {
    const protocolFeeBps = 100; // 1.00%
    await program.methods
      .initializeProtocol(protocolFeeBps, operator.publicKey)
      .accounts({
        config: configPda,
        settlementMint: settlementMint,
        admin: admin.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const configAccount = await program.account.config.fetch(configPda);
    expect(configAccount.admin.toBase58()).to.equal(admin.publicKey.toBase58());
    expect(configAccount.protocolFeeBps).to.equal(100);
    expect(configAccount.settlementMint.toBase58()).to.equal(settlementMint.toBase58());
  });

  it("Step 2: Registers Substation Grid Node with Dynamic Capacity", async () => {
    const baseRatePerKwh = new anchor.BN(300_000); // 0.30 EUR per kWh
    const maxCapacityKw = new anchor.BN(50_000); // 50 MW Substation capacity

    await program.methods
      .registerGridNode(gridId, baseRatePerKwh, maxCapacityKw)
      .accounts({
        grid_node: gridNodePda,
        operator: operator.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([operator])
      .rpc();

    const gridNode = await program.account.gridNode.fetch(gridNodePda);
    expect(gridNode.baseRatePerKwh.toNumber()).to.equal(300_000);
    expect(gridNode.congestionMultiplierBps.toNumber()).to.equal(10_000); // 1.0x
  });

  it("Step 3: Registers Producer & Consumer Smart Meter PDAs", async () => {
    // Register Producer Meter (e.g. 50kW Solar Roof)
    await program.methods
      .registerSmartMeter(producerDevice.publicKey, { producer: {} })
      .accounts({
        meterAccount: producerMeterPda,
        gridNode: gridNodePda,
        owner: producerOwner.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([producerOwner])
      .rpc();

    // Register Consumer Meter (e.g. EV Fleet Fast Charger)
    await program.methods
      .registerSmartMeter(consumerDevice.publicKey, { consumer: {} })
      .accounts({
        meterAccount: consumerMeterPda,
        gridNode: gridNodePda,
        owner: consumerOwner.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .signers([consumerOwner])
      .rpc();

    const pMeter = await program.account.smartMeter.fetch(producerMeterPda);
    const cMeter = await program.account.smartMeter.fetch(consumerMeterPda);

    expect(pMeter.owner.toBase58()).to.equal(producerOwner.publicKey.toBase58());
    expect(cMeter.owner.toBase58()).to.equal(consumerOwner.publicKey.toBase58());
  });

  it("Step 4: Opens Bilateral Micro-Settlement Channel with Vault Escrow", async () => {
    const depositAmount = new anchor.BN(200_000_000); // 200.000000 EURk

    // Create the Escrow Vault ATA owned by Config PDA
    await createAccount(
      provider.connection,
      consumerOwner,
      settlementMint,
      configPda,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Fetch config vault account
    const configVaultAta = await anchor.utils.token.associatedAddress({
      mint: settlementMint,
      owner: configPda,
    });

    await program.methods
      .openStreamChannel(channelId, depositAmount)
      .accounts({
        streamChannel: channelPda,
        producerMeter: producerMeterPda,
        consumerMeter: consumerMeterPda,
        escrowVault: configVaultAta,
        consumerTokenAccount: consumerAta,
        settlementMint: settlementMint,
        consumerAuthority: consumerOwner.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([consumerOwner])
      .rpc();

    const channel = await program.account.streamChannel.fetch(channelPda);
    expect(channel.escrowBalance.toNumber()).to.equal(200_000_000);
    expect(channel.isClosed).to.be.false;
  });

  it("Step 5: Simulates High-Frequency Meter Telemetry Tick Settlement", async () => {
    const kwhDelta = new anchor.BN(150_000); // 150.000 kWh delivered
    const currentTime = Math.floor(Date.now() / 1000);

    const configVaultAta = await anchor.utils.token.associatedAddress({
      mint: settlementMint,
      owner: configPda,
    });

    const initialProducerBal = (await getAccount(provider.connection, producerAta, undefined, TOKEN_2022_PROGRAM_ID)).amount;

    await program.methods
      .settleMeterTick(kwhDelta, new anchor.BN(currentTime))
      .accounts({
        config: configPda,
        streamChannel: channelPda,
        producerMeter: producerMeterPda,
        consumerMeter: consumerMeterPda,
        gridNode: gridNodePda,
        escrowVault: configVaultAta,
        producerTokenAccount: producerAta,
        feeTreasury: feeTreasuryAta,
        settlementMint: settlementMint,
        telemetryAuthority: admin.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .rpc();

    // Verification:
    // Rate = 300,000 micro-EUR ($0.30). Multiplier = 10000 (1.0x).
    // Total raw cost = (150,000 * 300,000) / 1,000,000 = 45,000,000 micro-EUR (45.00 EURk).
    // Protocol Fee = 1% = 450,000 micro-EUR.
    // Net Producer = 44,550,000 micro-EUR.
    const updatedProducerBal = (await getAccount(provider.connection, producerAta, undefined, TOKEN_2022_PROGRAM_ID)).amount;
    expect(Number(updatedProducerBal) - Number(initialProducerBal)).to.equal(44_550_000);

    const channel = await program.account.streamChannel.fetch(channelPda);
    expect(channel.escrowBalance.toNumber()).to.equal(200_000_000 - 45_000_000);
    expect(channel.totalSettledKwh.toNumber()).to.equal(150_000);
  });

  it("Step 6: Prevents Malicious Replay / Stale Timestamp Exploit", async () => {
    const kwhDelta = new anchor.BN(5_000);
    const staleTime = Math.floor(Date.now() / 1000) - 600; // 10 minutes stale

    const configVaultAta = await anchor.utils.token.associatedAddress({
      mint: settlementMint,
      owner: configPda,
    });

    try {
      await program.methods
        .settleMeterTick(kwhDelta, new anchor.BN(staleTime))
        .accounts({
          config: configPda,
          streamChannel: channelPda,
          producerMeter: producerMeterPda,
          consumerMeter: consumerMeterPda,
          gridNode: gridNodePda,
          escrowVault: configVaultAta,
          producerTokenAccount: producerAta,
          feeTreasury: feeTreasuryAta,
          settlementMint: settlementMint,
          telemetryAuthority: admin.publicKey,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
        })
        .rpc();
      expect.fail("Execution should fail due to stale telemetry");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("TelemetryTimestampStale");
    }
  });

  it("Step 7: Closes Stream Channel and Returns Escrow Remainder", async () => {
    const configVaultAta = await anchor.utils.token.associatedAddress({
      mint: settlementMint,
      owner: configPda,
    });

    const consumerBalBefore = (await getAccount(provider.connection, consumerAta, undefined, TOKEN_2022_PROGRAM_ID)).amount;

    await program.methods
      .closeStreamChannel()
      .accounts({
        config: configPda,
        streamChannel: channelPda,
        escrowVault: configVaultAta,
        consumerTokenAccount: consumerAta,
        settlementMint: settlementMint,
        consumerAuthority: consumerOwner.publicKey,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([consumerOwner])
      .rpc();

    const channel = await program.account.streamChannel.fetch(channelPda);
    expect(channel.isClosed).to.be.true;
    expect(channel.escrowBalance.toNumber()).to.equal(0);

    const consumerBalAfter = (await getAccount(provider.connection, consumerAta, undefined, TOKEN_2022_PROGRAM_ID)).amount;
    expect(Number(consumerBalAfter)).to.be.greaterThan(Number(consumerBalBefore));
  });
});