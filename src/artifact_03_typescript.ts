import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Kraftwerk } from "../target/types/kraftwerk";
import {
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { assert, expect } from "chai";

describe("KraftWerk Decentralized Grid Settlement Engine Tests", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.Kraftwerk as Program<Kraftwerk>;

  let euroeMint: anchor.web3.PublicKey;
  let authority = provider.wallet as anchor.Wallet;
  let prosumer = anchor.web3.Keypair.generate();
  let hardwareKey = anchor.web3.Keypair.generate();

  const nodeId = "DE-TENNET-BAYERN-01";
  let gridNodePda: anchor.web3.PublicKey;
  let settlementVaultPda: anchor.web3.PublicKey;
  let deviceRecordPda: anchor.web3.PublicKey;
  let prosumerTokenAccount: anchor.web3.PublicKey;

  before(async () => {
    // 1. Airdrop prosumer wallet
    const signature = await provider.connection.requestAirdrop(
      prosumer.publicKey,
      2 * anchor.web3.LAMPORTS_PER_SOL
    );
    await provider.connection.confirmTransaction(signature);

    // 2. Create EUROe mock stablecoin mint
    euroeMint = await createMint(
      provider.connection,
      authority.payer,
      authority.publicKey,
      null,
      6
    );

    // 3. Derive PDAs
    [gridNodePda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("grid_node"), Buffer.from(nodeId)],
      program.programId
    );

    [settlementVaultPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [Buffer.from("settlement_vault"), Buffer.from(nodeId)],
      program.programId
    );

    [deviceRecordPda] = anchor.web3.PublicKey.findProgramAddressSync(
      [
        Buffer.from("device"),
        prosumer.publicKey.toBuffer(),
        gridNodePda.toBuffer(),
      ],
      program.programId
    );

    // 4. Create prosumer token balance
    prosumerTokenAccount = await createAccount(
      provider.connection,
      prosumer,
      euroeMint,
      prosumer.publicKey
    );
  });

  it("Initializes a localized German Grid Balancing Node", async () => {
    const baseRate = new anchor.BN(20_000_000); // 20 Euro Cents per kWh scaled by 1e6

    await program.methods
      .initializeGridNode(nodeId, baseRate)
      .accounts({
        gridNode: gridNodePda,
        settlementVault: settlementVaultPda,
        settlementMint: euroeMint,
        authority: authority.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    // Fund the settlement vault with 50,000 EUROe for liquidity
    await mintTo(
      provider.connection,
      authority.payer,
      euroeMint,
      settlementVaultPda,
      authority.publicKey,
      50_000_000_000
    );

    const nodeAccount = await program.account.gridNode.fetch(gridNodePda);
    expect(nodeAccount.nodeId).to.equal(nodeId);
    expect(nodeAccount.currentFrequencyMhz).to.equal(50000); // 50.00 Hz
  });

  it("Registers an edge residential battery device with stake collateral", async () => {
    // Mint 500 EUROe collateral to prosumer
    await mintTo(
      provider.connection,
      authority.payer,
      euroeMint,
      prosumerTokenAccount,
      authority.publicKey,
      500_000_000
    );

    const maxWatts = new anchor.BN(15_000); // 15 kW inverter capacity

    await program.methods
      .registerDevice(maxWatts, hardwareKey.publicKey)
      .accounts({
        gridNode: gridNodePda,
        deviceRecord: deviceRecordPda,
        collateralAccount: prosumerTokenAccount,
        owner: prosumer.publicKey,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .signers([prosumer])
      .rpc();

    const device = await program.account.deviceRecord.fetch(deviceRecordPda);
    expect(device.owner.toBase58()).to.equal(prosumer.publicKey.toBase58());
    expect(device.maxCapacityWatts.toNumber()).to.equal(15_000);
    expect(device.isActive).to.be.true;
  });

  it("Commits telemetry during grid under-frequency (49.92 Hz)", async () => {
    const now = Math.floor(Date.now() / 1000);
    const injectedEnergyMwh = new anchor.BN(5_000_000); // 5.0 kWh injected
    const underFreqMhz = 49_920; // 49.92 Hz (severe drop, high demand for power)

    await program.methods
      .commitEnergyTelemetry(injectedEnergyMwh, underFreqMhz, new anchor.BN(now))
      .accounts({
        gridNode: gridNodePda,
        deviceRecord: deviceRecordPda,
        signer: prosumer.publicKey,
      })
      .signers([prosumer])
      .rpc();

    const device = await program.account.deviceRecord.fetch(deviceRecordPda);
    expect(device.accumulatedUnsettledMwh.toNumber()).to.equal(5_000_000);

    const node = await program.account.gridNode.fetch(gridNodePda);
    expect(node.currentFrequencyMhz).to.equal(underFreqMhz);
  });

  it("Settles balancing round with dynamic scarcity pricing", async () => {
    const preVaultBalance = (
      await getAccount(provider.connection, settlementVaultPda)
    ).amount;

    await program.methods
      .settleBalancingRound()
      .accounts({
        gridNode: gridNodePda,
        deviceRecord: deviceRecordPda,
        settlementVault: settlementVaultPda,
        devicePayoutAccount: prosumerTokenAccount,
        signer: prosumer.publicKey,
        tokenProgram: anchor.utils.token.TOKEN_PROGRAM_ID,
      })
      .signers([prosumer])
      .rpc();

    const postVaultBalance = (
      await getAccount(provider.connection, settlementVaultPda)
    ).amount;
    const postProsumerBalance = (
      await getAccount(provider.connection, prosumerTokenAccount)
    ).amount;

    // Prosumer injected power during under-frequency: rewarded from the vault
    expect(Number(postVaultBalance)).to.be.lessThan(Number(preVaultBalance));
    expect(Number(postProsumerBalance)).to.be.greaterThan(500_000_000);

    const device = await program.account.deviceRecord.fetch(deviceRecordPda);
    expect(device.accumulatedUnsettledMwh.toNumber()).to.equal(0);
  });

  it("Rejects stale or future timestamp telemetry submissions", async () => {
    const badTimestamp = new anchor.BN(Math.floor(Date.now() / 1000) - 600); // 10 mins old
    try {
      await program.methods
        .commitEnergyTelemetry(new anchor.BN(100_000), 50000, badTimestamp)
        .accounts({
          gridNode: gridNodePda,
          deviceRecord: deviceRecordPda,
          signer: prosumer.publicKey,
        })
        .signers([prosumer])
        .rpc();
      assert.fail("Should have failed on stale timestamp");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("StaleTelemetryTimestamp");
    }
  });
});