#!/bin/bash

# hold: solana-ledger-tool create-snapshot --ledger custom-ledger --accounts temp/HWy1j ROOT
# https://solana.stackexchange.com/questions/7429/how-to-get-accounts-data-from-solana-snapshot-file

# Display help menu
usage() {
    echo ""
    echo "populate-local-validator.sh - help"
    echo "==============================================================================="
    echo ""
    echo "General script to setup solana local validator."
    echo ""
    echo "-------------------------------------------------------------------------------"
    echo "Options"
    echo "-------------------------------------------------------------------------------"
    echo "--help                               Displays help menu"
    echo ""
    echo ""
    echo "--populate=<option>                  Specify the accounts to populate."
    echo "                                     all"
    echo "                                     orca"
    echo "                                     raydium-clmm"
    echo "                                     raydium-cpmm"
    echo ""
    echo "--fetch=<number>                     Specify the number of accounts to fetch."
    echo ""
    echo "-------------------------------------------------------------------------------"
    echo ""

    exit
}

# Parse input arguments
FETCH_LIMIT=0
for i in "$@"
do
case $i in
    --help)
    usage
    shift
    ;;
    --populate=*)
    POPULATE="${i#*=}"
    shift
    ;;
    --fetch=*)
    FETCH_LIMIT="${i#*=}"
    shift
    ;;
    *)
    echo "Unknown option: $i"
    usage
    shift
    ;;
esac
done

# Validate input arguments
if [[ "$POPULATE" != "all"  && \
      "$POPULATE" != "orca"  && \
      "$POPULATE" != "raydium-clmm"   && \
      "$POPULATE" != "raydium-cpmm" ]]; then
    echo "Invalid --populate: $POPULATE"
    usage
fi

if ! [[ "$FETCH_LIMIT" =~ ^[0-9]+$ ]]; then
    echo "Invalid --fetch: $FETCH_LIMIT"
    usage
fi

# Define the public keys to clone
# Openbook AMM (Raydium)
# Standard AMM (CPMM)
# Concentrated Liquidity AMM (CLMM)
PUBLIC_KEYS=(
    "HWy1jotHpo6UqeQxx49dpYYdQB8wj9Qk9MdxwjLvDHB8"
    "CPMDWBwJDtYax9qW7AyRuVC19Cc4L4Vcy4n2BHAbHkCW"
    "devi51mZmdwUJGU9hjN27vEz64Gps7uUefqxg27EAtH"
)

echo "Killing any running solana-test-validator processes..."
pkill -f solana-test-validator

echo "Starting solana-test-validator in the background..."
nohup solana-test-validator --reset --ledger custom-ledger --url https://api.devnet.solana.com \
    $(for PUBLIC_KEY in "${PUBLIC_KEYS[@]}"; do echo --clone-upgradeable-program $PUBLIC_KEY; done) > validator.log 2>&1 &

echo "Waiting for the validator to start..."
sleep 10

# Create a temporary folder with a static name and change into it
TEMP_DIR="temp"
rm -rf $TEMP_DIR
mkdir -p $TEMP_DIR
cd $TEMP_DIR

# Fetch program accounts from local validator
for PUBLIC_KEY in "${PUBLIC_KEYS[@]}"; do
    # Create a folder based on the first 5 characters of the PUBLIC_KEY and change into it
    PUBLIC_KEY_DIR="${PUBLIC_KEY:0:5}"
    mkdir -p $PUBLIC_KEY_DIR
    cd $PUBLIC_KEY_DIR

    echo "Fetching program account $PUBLIC_KEY from local validator..."
    solana account $PUBLIC_KEY --url http://127.0.0.1:8899

    echo "Fetching all accounts for program $PUBLIC_KEY..."
    curl https://api.devnet.solana.com -X POST -H "Content-Type: application/json" -d "{
        \"jsonrpc\": \"2.0\",
        \"id\": 1,
        \"method\": \"getProgramAccounts\",
        \"params\": [
            \"$PUBLIC_KEY\",
            {
                \"encoding\": \"jsonParsed\"
            }
        ]
    }" > ${PUBLIC_KEY}_accounts.json

    ACCOUNT_COUNT=$(jq '.result | length' ${PUBLIC_KEY}_accounts.json)
    echo "Total accounts fetched for program $PUBLIC_KEY: $ACCOUNT_COUNT"

    FETCHED_COUNT=0
    for account in $(jq -r '.result[].pubkey' ${PUBLIC_KEY}_accounts.json); do
        FETCHED_COUNT=$((FETCHED_COUNT + 1))
        if (( FETCH_LIMIT > 0 && FETCHED_COUNT > FETCH_LIMIT )); then
            break
        fi
        if (( FETCHED_COUNT % 100 == 0 )); then
            echo "Fetching account $FETCHED_COUNT of $ACCOUNT_COUNT: $account (Total: $ACCOUNT_COUNT)"
        fi
        solana account $account --output json --url https://api.devnet.solana.com > "$account.json"
    done
    echo "Total accounts fetched and processed for program $PUBLIC_KEY: $FETCHED_COUNT"

    echo "Creating snapshots for accounts..."
    CREATION_COUNT=0
    for account_file in *.json; do
        # Use the keyword ROOT or a valid slot number instead of custom-ledger
        solana-ledger-tool create-snapshot ROOT custom-ledger $account_file
        CREATION_COUNT=$((CREATION_COUNT + 1))
        if (( CREATION_COUNT % 100 == 0 )); then
            echo "Created snapshot for $CREATION_COUNT accounts (Total: $ACCOUNT_COUNT)"
        fi
    done

    echo "Showing program $PUBLIC_KEY..."
    solana program show $PUBLIC_KEY --url http://127.0.0.1:8899

    # Change back to the parent directory
    cd ..
done

# Clean up temporary folder
cd ..
rm -rf $TEMP_DIR

echo "Script execution completed."






