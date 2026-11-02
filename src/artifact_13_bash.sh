# 1. Switch CLI to Solana Devnet
solana config set --url https://api.devnet.solana.com

# 2. Airdrop devnet SOL
solana airdrop 2

# 3. Build & Deploy
anchor build
anchor deploy --provider.cluster devnet

# 4. Verify deployment
solana program show $PROGRAM_ID