# 1. Install TypeScript dependencies
yarn install

# 2. Build the Anchor program binary & generate IDL
anchor build

# 3. Synchronize program keys if re-generating keys
anchor keys sync

# 4. Execute the integration test matrix with local test validator
anchor test