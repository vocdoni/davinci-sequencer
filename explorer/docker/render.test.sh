#!/bin/sh
# Checks docker/render.sh: defaults, overrides, clearing, proxies and
# validation. Needs jq. Run from the explorer directory.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/davinci-explorer-render.XXXXXX")
trap 'rm -rf "$work"' EXIT
fail() {
	echo "FAIL: $*" >&2
	exit 1
}

render() {
	env -i PATH="$PATH" DEFAULTS="$here/../public/config.json" HTML_DIR="$work/html" NGINX_CONF="$work/nginx.conf" "$@" \
		sh "$here/render.sh" >"$work/log" 2>&1
}
cfg() { jq -r "$1" "$work/html/config.json"; }

# Defaults: the committed config, sequencers none, no proxies.
render || fail "defaults: $(cat "$work/log")"
[ "$(cfg .chainId)" = 100 ] || fail "default chainId"
[ "$(cfg .registryAddress)" = "$(jq -r .registryAddress "$here/../public/config.json")" ] || fail "default registry"
[ "$(cfg '.rpcUrls | length')" = 3 ] || fail "default rpcUrls"
[ "$(cfg .beaconUrl)" = https://rpc-gbc.gnosischain.com ] || fail "default beacon"
[ "$(cfg '.sequencers | length')" = 0 ] || fail "default sequencers"
grep -q 'listen 8080;' "$work/nginx.conf" || fail "default port"
grep -q '/proxy/' "$work/nginx.conf" && fail "no proxy by default"

# Overrides, clearing an optional field, sequencer proxies.
render CHAIN_ID=31337 NETWORK_NAME=anvil RPC_URL='http://127.0.0.1:8545/, http://127.0.0.1:8546' \
	REGISTRY_ADDRESS=0x0000000000000000000000000000000000000abc START_BLOCK=7 BLOCK_EXPLORER_URL= \
	SEQUENCER_URLS='https://seq1.example/api/,https://seq2.example' PORT=9090 ||
	fail "overrides: $(cat "$work/log")"
[ "$(cfg .chainId)" = 31337 ] || fail chainId
[ "$(cfg .networkName)" = anvil ] || fail networkName
[ "$(cfg '.rpcUrls | join(" ")')" = 'http://127.0.0.1:8545 http://127.0.0.1:8546' ] || fail rpcUrls
[ "$(cfg .startBlock)" = 7 ] || fail startBlock
[ "$(cfg '.blockExplorerUrl // "none"')" = none ] || fail "empty BLOCK_EXPLORER_URL clears it"
[ "$(cfg '.sequencers[1].url')" = /proxy/sequencer/1 ] || fail "sequencer proxy url"
[ "$(cfg '.sequencers[0].upstream')" = https://seq1.example/api ] || fail "sequencer upstream"
grep -q 'location /proxy/sequencer/0/' "$work/nginx.conf" || fail "sequencer 0 location"
grep -q 'rewrite ^/proxy/sequencer/0/(.\*)\$ /api/\$1 break;' "$work/nginx.conf" || fail "sequencer base path"
grep -q 'limit_except GET HEAD' "$work/nginx.conf" || fail "proxies are read-only"
grep -q 'listen 9090;' "$work/nginx.conf" || fail PORT

# Beacon proxy, direct sequencers.
render BEACON_PROXY=true SEQUENCER_PROXY=false SEQUENCER_URLS=https://seq.example || fail "beacon proxy: $(cat "$work/log")"
[ "$(cfg .beaconUrl)" = /proxy/beacon ] || fail "beacon proxy url"
[ "$(cfg .beaconUpstream)" = https://rpc-gbc.gnosischain.com ] || fail "beacon upstream"
[ "$(cfg '.sequencers[0].url')" = https://seq.example ] || fail "direct sequencer"
grep -q 'location /proxy/beacon/' "$work/nginx.conf" || fail "beacon location"
grep -q '/proxy/sequencer' "$work/nginx.conf" && fail "no sequencer proxy when disabled"

# Validation.
render CHAIN_ID=abc && fail "accepted a bad CHAIN_ID"
render REGISTRY_ADDRESS=0x1234 && fail "accepted a bad REGISTRY_ADDRESS"
render RPC_URL=ftp://x && fail "accepted a non-http RPC_URL"
render RPC_URL= && fail "accepted an empty RPC_URL"
render SEQUENCER_URLS='https://x.example/a;b' && fail "accepted a URL nginx cannot take"
render START_BLOCK=-1 && fail "accepted a negative START_BLOCK"

echo "render.sh: ok"
