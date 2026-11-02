# Set Solana CLI target to Devnet
solana config set --url https://api.devnet.solana.com

# Ensure deployer wallet is funded with Devnet SOL
solana airdrop 2

# Build for release
anchor build --verifiable

# Deploy to Devnet
anchor deploy --provider.cluster devnet