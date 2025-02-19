export MACAROON_PATH=<PATH>/lndk/.macaroons/playground/admin.macaroon
export CERT_PATH=<PATH>/.lndk/data/tls-cert.pem
cargo run --bin lndk-cli -- -m $MACAROON_PATH pay-offer --cert-path $CERT_PATH $1 100000
