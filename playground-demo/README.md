# Demo of running LNDK

1. run scripts/init.sh
1. List peers `./playground-demo/bin/lncli lnd1 listpeers`
1. Check payments ` ./playground-demo/bin/lncli lnd1 listpayments`
1. Setup lndk.conf
1. Modify your scripts-demo bash files
1. Run `cargo run --bin=lndk` to start server
1. Run ./playground-demo/bin/eclair-cli eclair1 tipjarshowoffer
1. Copy offer
1. Run ./scripts-demo/get_invoice.sh (gets invoice)
1. Run ./scripts-demo/pay_offer.sh (gets invoice and pays an offer)
1. Check payments ` ./playground-demo/bin/lncli lnd1 listpayments`
1. Yaaaay 🎉
1. Now you can hack around, find bugs or help us out! :D
