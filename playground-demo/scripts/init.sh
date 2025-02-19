#!/usr/bin/env bash

DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"


setup () {
  echo "Cleaning up from previous runs..."
  docker compose down --volumes

  echo
  echo "Starting the stack..."
  docker compose up -d
}

bitcoind() {
  $DIR/../bin/bitcoin-cli $@
}

lnd1() {
  $DIR/../bin/lncli lnd1 $@
}

eclair1() {
  $DIR/../bin/eclair-cli eclair1 $@
}

eclair2() {
  $DIR/../bin/eclair-cli eclair2 $@
}

waitFor() {
  counter=0
  until $@ || [ $counter -eq 60 ]; do
    >&2 echo "$@ unavailable - waiting..."
    sleep 1
    ((counter++))
  done

  if [ $counter -eq 60 ]; then
    >&2 echo "Waited for 60 seconds, but $@ is still unavailable. Exiting."
    exit 1
  fi
}

print_section() {
  echo -e "\033[1;34m\n==================== $1 ====================\033[0m"
}

createBitcoindWallet() {
  $DIR/../bin/bitcoin-cli createwallet default || $DIR/../bin/bitcoin-cli loadwallet default || true
}

mineBlocks() {
  ADDRESS=$1
  AMOUNT=${2:-1}
  echo Mining $AMOUNT blocks to $ADDRESS...
  bitcoind generatetoaddress $AMOUNT $ADDRESS
  sleep 0.5 # waiting for blocks to be propagated
}

initBitcoinChain() {
  # Mine 103 blocks to initliase a bitcoind node.
  mineBlocks $BITCOIN_ADDRESS 103
}

generateBitcoinAddress() {
  BITCOIN_ADDRESS=$(bitcoind getnewaddress)
  echo BITCOIN_ADDRESS: $BITCOIN_ADDRESS
}

generateNodeAddresses() {
  LND1_ADDRESS=$(lnd1 newaddress p2wkh | jq -r .address)
  echo LND1_ADDRESS: $LND1_ADDRESS

  ECLAIR1_ADDRESS=$(eclair1 getnewaddress)
  echo ECLAIR1_ADDRESS: $ECLAIR1_ADDRESS

  ECLAIR2_ADDRESS=$(eclair2 getnewaddress)
  echo ECLAIR2_ADDRESS: $ECLAIR2_ADDRESS
}

getNodeInfo() {
  LND1_NODE_INFO=$(lnd1 getinfo)
  LND1_NODE_URI=$(echo ${LND1_NODE_INFO} | jq -r .uris[0])
  LND1_PUBKEY=$(echo ${LND1_NODE_INFO} | jq -r .identity_pubkey)
  echo LND1_PUBKEY: $LND1_PUBKEY
  echo LND1_NODE_URI: $LND1_NODE_URI

  ECLAIR1_NODE_INFO=$(eclair1 getinfo)
  ECLAIR1_PUBKEY=$(echo ${ECLAIR1_NODE_INFO} | jq -r .nodeId)
  ECLAIR1_PUBLIC_ADDRESS=$(echo ${ECLAIR1_NODE_INFO} | jq -r '.publicAddresses[0]')
  ECLAIR1_NODE_URI="${ECLAIR1_PUBKEY}@${ECLAIR1_PUBLIC_ADDRESS}"
  echo ECLAIR1_PUBKEY: $ECLAIR1_PUBKEY
  echo ECLAIR1_NODE_URI: $ECLAIR1_NODE_URI

  ECLAIR2_NODE_INFO=$(eclair2 getinfo)
  ECLAIR2_PUBKEY=$(echo ${ECLAIR2_NODE_INFO} | jq -r .nodeId)
  ECLAIR2_PUBLIC_ADDRESS=$(echo ${ECLAIR2_NODE_INFO} | jq -r '.publicAddresses[0]')
  ECLAIR2_NODE_URI="${ECLAIR2_PUBKEY}@${ECLAIR2_PUBLIC_ADDRESS}"
  echo ECLAIR2_PUBKEY: $ECLAIR2_PUBKEY
  echo ECLAIR2_NODE_URI: $ECLAIR2_NODE_URI
}

sendFundingTransaction() {
  echo creating raw tx...
  local addresses=($LND1_ADDRESS $ECLAIR1_ADDRESS $ECLAIR2_ADDRESS)
  local outputs=$(jq -nc --arg amount 1 '$ARGS.positional | reduce .[] as $address ({}; . + {($address) : ($amount | tonumber)})' --args "${addresses[@]}")
  RAW_TX=$(bitcoind createrawtransaction "[]" $outputs)
  echo RAW_TX: $RAW_TX

  echo funding raw tx $RAW_TX...
  FUNDED_RAW_TX=$(bitcoind fundrawtransaction "$RAW_TX" | jq -r .hex)
  echo FUNDED_RAW_TX: $FUNDED_RAW_TX

  echo signing funded tx $FUNDED_RAW_TX...
  SIGNED_TX_HEX=$(bitcoind signrawtransactionwithwallet "$FUNDED_RAW_TX" | jq -r .hex)
  echo SIGNED_TX_HEX: $SIGNED_TX_HEX

  echo sending signed tx $SIGNED_TX_HEX...
  bitcoind sendrawtransaction "$SIGNED_TX_HEX"
}

fundNodes() {
  # Fund with multiple transactions to that we have multiple utxos to spend on each of the lnd nodes.
  sendFundingTransaction
  sendFundingTransaction
  sendFundingTransaction
  sendFundingTransaction
  sendFundingTransaction
  sendFundingTransaction
  sendFundingTransaction
  sendFundingTransaction

  # Generate some blocks to confirm the transactions.
  mineBlocks $BITCOIN_ADDRESS 10
}

openChannel() {

  # Open a channel between lnd1 and eclair1.
  echo "Opening channel between lnd1 and eclair1"
  waitFor lnd1 connect $ECLAIR1_NODE_URI
  waitFor lnd1 openchannel $ECLAIR1_PUBKEY 10000000 5000000

  # Open a channel between lnd1 and eclair2.
  echo "Opening channel between lnd1 and eclair2"
  waitFor lnd1 connect $ECLAIR2_NODE_URI
  waitFor lnd1 openchannel $ECLAIR2_PUBKEY 10000000 5000000

  # Open a channel between eclair1 and eclair2.
  echo "Opening channel between eclair1 and eclair2"
  waitFor eclair1 connect --uri=$ECLAIR2_NODE_URI
  waitFor eclair1 open --nodeId=$ECLAIR2_PUBKEY --fundingSatoshis=10000000 --pushMsat5000000


  # Generate some blocks to confirm the channel.
  mineBlocks $BITCOIN_ADDRESS 10
}


waitBitcoind() {
  waitFor bitcoind getnetworkinfo
}

waitForNodes() {
  waitFor lnd1 getinfo
  waitFor eclair1 getinfo
  waitFor eclair2 getinfo
}

waitForGraphSync() {
  # Declare an associative array with node names as keys and public keys as values
  declare -A nodes=(
      ["ECLAIR1"]=$ECLAIR1_PUBKEY
      ["ECLAIR2"]=$ECLAIR2_PUBKEY
  )

  # Get the current time and set a timeout of 10 minutes
  start_time=$(date +%s)
  timeout=$((start_time + 600))

  echo "Initiating node graph validation. Please wait, this may take up to 10 minutes..."

  # Loop until all nodes have an address or the timeout is reached
  while true; do
    all_nodes_have_address=true

    # Loop over all nodes
    for node in "${!nodes[@]}"; do
      # If the public key for a node is not set, exit with an error
      if [ -z "${nodes[$node]}" ]; then
        echo "Error: Public key for $node is not set"
        exit 1
      fi

      # Get node info and check if the command succeeded
      node_info=$(lnd1 getnodeinfo ${nodes[$node]} 2>&1)
      if [ $? -ne 0 ]; then
        echo "Error: Failed to get node info for $node"
        echo "Details: $node_info"
        all_nodes_have_address=false
        break
      fi

      # Extract the addresses from the node info
      node_addresses=$(echo "$node_info" | jq -c '.node.addresses')
      echo "Addresses for $node: $node_addresses"

      # Count the number of addresses and check if it's zero
      address_count=$(echo "$node_addresses" | jq 'if . == [] then 0 else length end')
      if [ $address_count -eq 0 ]; then
        echo "Error: $node is missing an address"
        all_nodes_have_address=false
        break
      fi
    done

    # If all nodes have an address, break the loop
    if $all_nodes_have_address; then
      break
    fi

    # If the current time is greater than or equal to the timeout, exit with an error
    current_time=$(date +%s)
    if [ $current_time -ge $timeout ]; then
      echo "Timeout: Not all nodes have an address after 10 minutes"
      exit 1
    fi

    # Wait for 5 seconds before the next iteration
    sleep 5
  done

  # Calculate the elapsed time and print a success message
  end_time=$(date +%s)
  elapsed_time=$((end_time - start_time))
  echo "All nodes have an address after $elapsed_time seconds"
}

copyMacaroonAndTlsLocally () {
  echo "Copying macaroon and tls files locally..."
  docker cp playground-demo-lnd1-1:/root/.lnd/data/chain/bitcoin/regtest/admin.macaroon $DIR/../../.macaroons/playground/admin.macaroon
  docker cp playground-demo-lnd1-1:/root/.lnd/tls.cert $DIR/../../.macaroons/playground/tls.cert
}

main() {
  print_section "SETUP"
  setup

  print_section "WAIT FOR BITCOIND"
  waitBitcoind

  print_section "CREATE BITCOIND WALLET"
  createBitcoindWallet

  print_section "GENERATE BITCOIN ADDRESS"
  generateBitcoinAddress

  print_section "INITIALIZE BITCOIN CHAIN"
  initBitcoinChain

  print_section "WAIT FOR NODES"
  waitForNodes

  print_section "GENERATE NODE ADDRESSES"
  generateNodeAddresses

  print_section "GET NODE INFO"
  getNodeInfo

  print_section "FUND NODES"
  fundNodes

  print_section "OPEN CHANNEL"
  openChannel

  print_section "VALIDATE NODE GRAPH"
  waitForGraphSync

  print_section "NODE INFO"
  getNodeInfo

  print_section "NODE INFO"
  copyMacaroonAndTlsLocally
}

main
