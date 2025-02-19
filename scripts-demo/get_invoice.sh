export MACAROON_PATH=<PATH>/.macaroons/playground/admin.macaroon
export CERT_PATH=<PATH>/.lndk/data/tls-cert.pem

cargo run --bin lndk-cli -- -m $MACAROON_PATH --cert-path $CERT_PATH get-invoice $1 10
