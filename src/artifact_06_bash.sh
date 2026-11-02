# 1. Clone the repository and install npm packages
cd solkredit
yarn install

# 2. Configure Solana CLI for Localnet
solana config set --url localhost

# 3. Generate Localnet Authority Keypair (if not present)
solana-keygen new --no-bip39-passphrase -o ~/.config/solana/id.json --force

# 4. Update Anchor Program ID
# Sync Anchor.toml and programs/sol_kredit/src/lib.rs with the generated program key
anchor keys sync