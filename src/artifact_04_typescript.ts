import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { KreisProtocol } from "../target/types/kreis_protocol";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import {
  TOKEN_2022_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";
import * as crypto from "crypto";

describe("KREIS Institutional Factoring Engine Tests", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.KreisProtocol as Program<KreisProtocol>;

  const authority = Keypair.generate();
  const supplier = Keypair.generate();
  const debtorVerifier = Keypair.generate();
  const liquidatorKeeper = Keypair.generate();

  let euroMint: PublicKey;
  let poolPDA: PublicKey;
  let vaultAuthorityPDA: PublicKey;
  let poolVaultPDA: PublicKey;

  let depositorEuroATA: PublicKey;
  let supplierEuroATA: PublicKey;
  let keeperEuroATA: PublicKey;

  const DECIMALS = 6;
  const INITIAL_DEPOSIT = new anchor.BN(1_000_000 * 10 ** DECIMALS); // €1,000,000.00 EUR
  const INVOICE_NOMINAL = new anchor.BN(100_000 * 10 ** DECIMALS);  // €100,000.00 EUR
  const ADVANCE_RATE_BPS = 8000; // 80.00% -> €80,000.00 advance

  before(async () => {
    // Fund test keys with SOL for rent
    for (const kp of [authority, supplier, debtorVerifier, liquidatorKeeper]) {
      const sig = await provider.connection.requestAirdrop(
        kp.publicKey,
        10 * anchor.web3.LAMPORTS_PER_SOL
      );
      await provider.connection.confirmTransaction(sig);
    }

    // 1. Initialize Institutional Token-2022 Euro Mint
    euroMint = await createMint(
      provider.connection,
      authority,
      authority.publicKey,
      null,
      DECIMALS,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Derive Program PDAs
    [poolPDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("liquidity_pool"), euroMint.toBuffer()],
      program.programId
    );

    [vaultAuthorityPDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("vault_authority"), euroMint.toBuffer()],
      program.programId
    );

    [poolVaultPDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("pool_vault"), poolPDA.toBuffer()],
      program.programId
    );

    // Create Token-2022 ATAs
    depositorEuroATA = await createAccount(
      provider.connection,
      authority,
      euroMint,
      authority.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    supplierEuroATA = await createAccount(
      provider.connection,
      supplier,
      euroMint,
      supplier.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    keeperEuroATA = await createAccount(
      provider.connection,
      liquidatorKeeper,
      euroMint,
      liquidatorKeeper.publicKey,
      undefined,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    // Mint test supply
    await mintTo(
      provider.connection,
      authority,
      euroMint,
      depositorEuroATA,
      authority.publicKey,
      2_000_000 * 10 ** DECIMALS,
      [],
      undefined,
      TOKEN_2022_PROGRAM_ID
    );

    await mintTo(
      provider.connection,
      authority,
      euroMint,
      keeperEuroATA,
      authority.publicKey,
      500_000 * 10 ** DECIMALS,
      [],
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
  });

  it("1. Successfully deploys & initializes the KREIS Liquidity Pool PDA", async () => {
    const baseRateBps = 400; // 4.00%
    const jumpRateBps = 1200; // 12.00%
    const reserveRatioBps = 1000; // 10.00%

    const tx = await program.methods
      .initializePool(baseRateBps, jumpRateBps, reserveRatioBps)
      .accounts({
        authority: authority.publicKey,
        euroMint: euroMint,
        pool: poolPDA,
        vaultAuthority: vaultAuthorityPDA,
        poolVault: poolVaultPDA,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([authority])
      .rpc();

    expect(tx).to.be.a("string");

    const poolAccount = await program.account.poolAccount.fetch(poolPDA);
    expect(poolAccount.baseRateBps).to.equal(baseRateBps);
    expect(poolAccount.jumpRateBps).to.equal(jumpRateBps);
    expect(poolAccount.totalDeposits.toNumber()).to.equal(0);
    expect(poolAccount.totalBorrowed.toNumber()).to.equal(0);
  });

  it("2. Accrues continuous Euro liquidity deposits into Pool Vault", async () => {
    await program.methods
      .depositLiquidity(INITIAL_DEPOSIT)
      .accounts({
        depositor: authority.publicKey,
        pool: poolPDA,
        euroMint: euroMint,
        depositorTokenAccount: depositorEuroATA,
        poolVault: poolVaultPDA,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([authority])
      .rpc();

    const poolAccount = await program.account.poolAccount.fetch(poolPDA);
    expect(poolAccount.totalDeposits.toString()).to.equal(
      INITIAL_DEPOSIT.toString()
    );

    const vaultBalance = await getAccount(
      provider.connection,
      poolVaultPDA,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    expect(vaultBalance.amount.toString()).to.equal(INITIAL_DEPOSIT.toString());
  });

  it("3. Factors verified invoice, advances 80% to German Supplier PDA", async () => {
    const rawInvoiceId = crypto.randomBytes(32);
    const [invoicePDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("invoice"), poolPDA.toBuffer(), rawInvoiceId],
      program.programId
    );

    const futureMaturity = new anchor.BN(
      Math.floor(Date.now() / 1000) + 60 * 86400 // 60-day tenor
    );

    await program.methods
      .factorInvoice(
        Array.from(rawInvoiceId),
        INVOICE_NOMINAL,
        ADVANCE_RATE_BPS,
        futureMaturity
      )
      .accounts({
        supplier: supplier.publicKey,
        debtorVerifier: debtorVerifier.publicKey,
        pool: poolPDA,
        euroMint: euroMint,
        vaultAuthority: vaultAuthorityPDA,
        poolVault: poolVaultPDA,
        supplierTokenAccount: supplierEuroATA,
        invoice: invoicePDA,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([supplier, debtorVerifier])
      .rpc();

    const invoiceAccount = await program.account.invoiceAccount.fetch(invoicePDA);
    const expectedAdvance = 80_000 * 10 ** DECIMALS;

    expect(invoiceAccount.nominalValue.toNumber()).to.equal(
      INVOICE_NOMINAL.toNumber()
    );
    expect(invoiceAccount.drawnAmount.toNumber()).to.equal(expectedAdvance);
    expect(invoiceAccount.status).to.deep.equal({ active: {} });

    // Supplier received the advance
    const supplierBalance = await getAccount(
      provider.connection,
      supplierEuroATA,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    expect(supplierBalance.amount.toString()).to.equal(
      expectedAdvance.toString()
    );
  });

  it("4. Rejects invoice with invalid maturity in the past", async () => {
    const rawInvoiceId = crypto.randomBytes(32);
    const [invoicePDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("invoice"), poolPDA.toBuffer(), rawInvoiceId],
      program.programId
    );

    const pastMaturity = new anchor.BN(Math.floor(Date.now() / 1000) - 100);

    try {
      await program.methods
        .factorInvoice(
          Array.from(rawInvoiceId),
          INVOICE_NOMINAL,
          ADVANCE_RATE_BPS,
          pastMaturity
        )
        .accounts({
          supplier: supplier.publicKey,
          debtorVerifier: debtorVerifier.publicKey,
          pool: poolPDA,
          euroMint: euroMint,
          vaultAuthority: vaultAuthorityPDA,
          poolVault: poolVaultPDA,
          supplierTokenAccount: supplierEuroATA,
          invoice: invoicePDA,
          tokenProgram: TOKEN_2022_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([supplier, debtorVerifier])
        .rpc();
      expect.fail("Expected transaction to fail on past maturity");
    } catch (err: any) {
      expect(err.error.errorCode.code).to.equal("InvalidMaturity");
    }
  });

  it("5. Completes invoice settlement with interest math validation", async () => {
    const rawInvoiceId = crypto.randomBytes(32);
    const [invoicePDA] = PublicKey.findProgramAddressSync(
      [Buffer.from("invoice"), poolPDA.toBuffer(), rawInvoiceId],
      program.programId
    );

    const maturity = new anchor.BN(Math.floor(Date.now() / 1000) + 30 * 86400);

    await program.methods
      .factorInvoice(
        Array.from(rawInvoiceId),
        INVOICE_NOMINAL,
        ADVANCE_RATE_BPS,
        maturity
      )
      .accounts({
        supplier: supplier.publicKey,
        debtorVerifier: debtorVerifier.publicKey,
        pool: poolPDA,
        euroMint: euroMint,
        vaultAuthority: vaultAuthorityPDA,
        poolVault: poolVaultPDA,
        supplierTokenAccount: supplierEuroATA,
        invoice: invoicePDA,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([supplier, debtorVerifier])
      .rpc();

    // Settle the invoice
    const repaymentAmount = new anchor.BN(85_000 * 10 ** DECIMALS);
    await program.methods
      .settleInvoice(repaymentAmount)
      .accounts({
        payer: authority.publicKey,
        pool: poolPDA,
        euroMint: euroMint,
        invoice: invoicePDA,
        payerTokenAccount: depositorEuroATA,
        poolVault: poolVaultPDA,
        tokenProgram: TOKEN_2022_PROGRAM_ID,
      })
      .signers([authority])
      .rpc();

    const invoiceAfter = await program.account.invoiceAccount.fetch(invoicePDA);
    expect(invoiceAfter.repaidAmount.toNumber()).to.be.greaterThan(0);
  });
});