#!/bin/sh
# Renders the explorer's runtime config and nginx site from environment
# variables. The nginx image runs it from /docker-entrypoint.d/ before nginx
# starts; it can also run by hand (the paths below are overridable).
#
# Defaults come from the committed public/config.json, copied into the image
# as $DEFAULTS: a variable that is unset keeps the default, one set to the
# empty string clears an optional field.
#
#   NETWORK_NAME        display name
#   CHAIN_ID            expected chain id (checked against the RPC at boot)
#   RPC_URL             JSON-RPC endpoints, comma-separated, in failover order
#   BEACON_URL          beacon API for blob sidecars
#   REGISTRY_ADDRESS    ProcessRegistry address
#   START_BLOCK         registry deployment block
#   SEQUENCER_URLS      sequencer node APIs, comma-separated
#   BLOCK_EXPLORER_URL  block explorer for tx/address links
#   DKG_EXPLORER_URL    davinci-dkg explorer for epoch links
#   BEACON_PROXY        true: serve the beacon at /proxy/beacon/ (default false)
#   SEQUENCER_PROXY     true: serve sequencer n at /proxy/sequencer/<n>/ (default true;
#                       sequencer nodes send no CORS headers)
#   PORT                listen port (default 8080)
set -eu

DEFAULTS=${DEFAULTS:-/etc/davinci-explorer/config.defaults.json}
HTML_DIR=${HTML_DIR:-/usr/share/nginx/html}
NGINX_CONF=${NGINX_CONF:-/etc/nginx/conf.d/default.conf}
PORT=${PORT:-8080}

log() { echo "[davinci-explorer] $*"; }
die() {
	echo "[davinci-explorer] error: $*" >&2
	exit 1
}

[ -f "$DEFAULTS" ] || die "missing defaults file $DEFAULTS"

is_true() {
	case $(printf '%s' "$1" | tr '[:upper:]' '[:lower:]') in
	1 | true | yes | on) return 0 ;;
	*) return 1 ;;
	esac
}

# A comma list as a JSON array of trimmed, non-empty strings without a trailing slash.
list_json() {
	printf '%s' "$1" | jq -R -c 'split(",") | map(gsub("^\\s+|\\s+$"; "") | sub("/+$"; "")) | map(select(length > 0))'
}

check_url() {
	case $1 in
	http://* | https://*) ;;
	*) die "$2 must be an http(s) URL, got '$1'" ;;
	esac
	if printf '%s' "$1" | grep -q '[[:space:]"$;{}\\]'; then die "$2 has characters nginx cannot take: '$1'"; fi
}

cfg=$(cat "$DEFAULTS")
set_json() { cfg=$(printf '%s' "$cfg" | jq -c --argjson v "$2" ".$1 = \$v"); }
set_str() { cfg=$(printf '%s' "$cfg" | jq -c --arg v "$2" ".$1 = \$v"); }
del_key() { cfg=$(printf '%s' "$cfg" | jq -c "del(.$1)"); }

if [ -n "${NETWORK_NAME:-}" ]; then set_str networkName "$NETWORK_NAME"; fi

if [ -n "${CHAIN_ID+x}" ]; then
	case $CHAIN_ID in '' | *[!0-9]*) die "CHAIN_ID must be a positive integer, got '$CHAIN_ID'" ;; esac
	set_json chainId "$CHAIN_ID"
fi

if [ -n "${RPC_URL+x}" ]; then
	rpcs=$(list_json "$RPC_URL")
	[ "$(printf '%s' "$rpcs" | jq 'length')" -gt 0 ] || die "RPC_URL needs at least one URL"
	for u in $(printf '%s' "$rpcs" | jq -r '.[]'); do check_url "$u" RPC_URL; done
	set_json rpcUrls "$rpcs"
fi

if [ -n "${REGISTRY_ADDRESS+x}" ]; then
	printf '%s' "$REGISTRY_ADDRESS" | grep -Eq '^0x[0-9a-fA-F]{40}$' || die "REGISTRY_ADDRESS is not an address: '$REGISTRY_ADDRESS'"
	set_str registryAddress "$REGISTRY_ADDRESS"
fi

if [ -n "${START_BLOCK+x}" ]; then
	case $START_BLOCK in '' | *[!0-9]*) die "START_BLOCK must be a block number, got '$START_BLOCK'" ;; esac
	set_json startBlock "$START_BLOCK"
fi

for pair in BLOCK_EXPLORER_URL:blockExplorerUrl DKG_EXPLORER_URL:dkgExplorerUrl; do
	var=${pair%%:*}
	key=${pair#*:}
	eval "isset=\${$var+x}; value=\${$var-}"
	# shellcheck disable=SC2154
	if [ -n "$isset" ]; then
		if [ -z "$value" ]; then del_key "$key"; else
			check_url "$value" "$var"
			set_str "$key" "${value%/}"
		fi
	fi
done

# Beacon: the URL itself, or the same-origin proxy in front of it.
beacon=$(printf '%s' "$cfg" | jq -r '.beaconUpstream // .beaconUrl // ""')
if [ -n "${BEACON_URL+x}" ]; then beacon=${BEACON_URL%/}; fi
beacon_proxy=
del_key beaconUpstream
if [ -z "$beacon" ]; then
	del_key beaconUrl
else
	check_url "$beacon" BEACON_URL
	if is_true "${BEACON_PROXY:-false}"; then
		beacon_proxy=$beacon
		set_str beaconUrl /proxy/beacon
		set_str beaconUpstream "$beacon"
	else
		set_str beaconUrl "$beacon"
	fi
fi

# Sequencers: direct, or each behind /proxy/sequencer/<n>.
if [ -n "${SEQUENCER_URLS+x}" ]; then
	sequencers=$(list_json "$SEQUENCER_URLS")
else
	sequencers=$(printf '%s' "$cfg" | jq -c '[.sequencers[]? | if type == "string" then . else (.upstream // .url) end]')
fi
for u in $(printf '%s' "$sequencers" | jq -r '.[]'); do check_url "$u" SEQUENCER_URLS; done
sequencer_proxy=false
if is_true "${SEQUENCER_PROXY:-true}"; then
	sequencer_proxy=true
	set_json sequencers "$(printf '%s' "$sequencers" | jq -c 'to_entries | map({url: "/proxy/sequencer/\(.key)", upstream: .value})')"
else
	set_json sequencers "$(printf '%s' "$sequencers" | jq -c 'map({url: .})')"
fi

mkdir -p "$HTML_DIR"
printf '%s\n' "$cfg" | jq . >"$HTML_DIR/config.json"
log "wrote $HTML_DIR/config.json"
jq -c . "$HTML_DIR/config.json"

# ── nginx ────────────────────────────────────────────────────────────────────

resolver=$(awk '/^nameserver/ { print $2; exit }' /etc/resolv.conf 2>/dev/null || true)
case $resolver in *:*) resolver="[$resolver]" ;; esac
[ -n "$resolver" ] || resolver=127.0.0.11

# proxy_block NAME PREFIX UPSTREAM: a GET-only reverse proxy. The upstream is
# held in a variable so nginx resolves it per request and starts even when the
# host does not resolve yet.
proxy_block() {
	name=$1
	prefix=$2
	upstream=$3
	origin=$(printf '%s' "$upstream" | sed -E 's#^(https?://[^/]+).*#\1#')
	base=$(printf '%s' "$upstream" | sed -E 's#^https?://[^/]+##; s#/+$##')
	host=$(printf '%s' "$origin" | sed -E 's#^https?://##')
	cat <<EOF

    location $prefix/ {
        limit_except GET HEAD { deny all; }
        set \$$name "$origin";
        rewrite ^$prefix/(.*)\$ $base/\$1 break;
        proxy_pass \$$name;
        proxy_set_header Host "$host";
        proxy_set_header User-Agent "davinci-explorer";
        proxy_ssl_server_name on;
        proxy_ssl_name "$host";
        proxy_hide_header Access-Control-Allow-Origin;
        proxy_read_timeout 60s;
    }
EOF
}

proxies=""
if [ -n "$beacon_proxy" ]; then proxies="$proxies$(proxy_block davinci_beacon /proxy/beacon "$beacon_proxy")"; fi
if [ "$sequencer_proxy" = true ]; then
	i=0
	for u in $(printf '%s' "$sequencers" | jq -r '.[]'); do
		proxies="$proxies$(proxy_block "davinci_sequencer_$i" "/proxy/sequencer/$i" "$u")"
		i=$((i + 1))
	done
fi

headers='add_header X-Content-Type-Options nosniff always;
        add_header Referrer-Policy strict-origin-when-cross-origin always;
        add_header X-Frame-Options DENY always;'

cat >"$NGINX_CONF" <<EOF
# Rendered by /docker-entrypoint.d/40-davinci-explorer.sh; edits are overwritten.
server {
    listen $PORT;
    server_name _;
    root $HTML_DIR;
    resolver $resolver valid=60s ipv6=off;

    gzip on;
    gzip_types text/css application/javascript application/json image/svg+xml;
    $headers

    location = /healthz {
        access_log off;
        default_type text/plain;
        return 200 "ok\n";
    }

    location = /config.json {
        add_header Cache-Control "no-store" always;
        $headers
    }

    location /assets/ {
        add_header Cache-Control "public, max-age=31536000, immutable" always;
        $headers
        try_files \$uri =404;
    }

    location / {
        add_header Cache-Control "no-cache" always;
        $headers
        try_files \$uri /index.html;
    }
$proxies
}
EOF
log "wrote $NGINX_CONF (port $PORT$([ -n "$proxies" ] && echo ', with proxies'))"
