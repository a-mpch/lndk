DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"

eclair() {
  $DIR/../bin/eclair-cli eclair${1} ${@:2}
}

eclair payoffer --offer=$1 --amountMsat=200000
