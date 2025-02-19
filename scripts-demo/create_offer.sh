DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
PLAYGROUND_PATH="$(cd "$DIR/../.configs/playground" >/dev/null 2>&1 && pwd)"
export MACAROON_PATH=$PLAYGROUND_PATH/lnd-secrets/admin.macaroon
export CERT_PATH=$PLAYGROUND_PATH/lndk-data/tls-cert.pem

cargo run --bin lndk-cli -- -m $MACAROON_PATH --cert-path $CERT_PATH create-offer --amount 10000
