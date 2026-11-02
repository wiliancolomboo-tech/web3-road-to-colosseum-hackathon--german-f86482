# 1. Switch CLI to Devnet
solana config set --url https://api.devnet.solana.com

# 2. Fund Deployer Keypair
solana airdrop 2

# 3. Synchronize program keys
anchor keys sync

# 4. Build and Deploy
anchor build
anchor deploy --provider.cluster devnet

# 5. Verify Program Account on-chain
solana program show <PROGRAM_ID>