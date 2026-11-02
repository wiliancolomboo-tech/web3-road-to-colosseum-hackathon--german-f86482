# Switch to devnet
solana config set --url https://api.devnet.solana.com

# Ensure your deployer keypair has devnet SOL
solana airdrop 2

# Deploy
anchor deploy --provider.cluster devnet