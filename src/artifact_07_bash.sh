# Set cluster target
solana config set --url https://api.devnet.solana.com

# Fund your deployer wallet
solana airdrop 2

# Deploy program binary
anchor deploy --provider.cluster devnet

# Verify the deployed program
solana program show KRFTh8rZtqV4sTcxR7Y1mJmK6e5L2o8t1V3X7Y2zPump