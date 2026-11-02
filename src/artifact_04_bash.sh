# 1. Clone repository
git clone https://github.com/kreis-protocol/kreis-core.git
cd kreis-core

# 2. Install dependencies
yarn install

# 3. Build Anchor program and generate IDL
anchor build

# 4. Extract generated program ID and synchronize keys
solana address -k target/deploy/kreis-keypair.json
# Ensure this pubkey matches declare_id!() in programs/kreis/src/lib.rs