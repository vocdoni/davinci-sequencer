#!/usr/bin/env bash
# Runs an e2e benchmark in the e2e/Dockerfile.bench image: the throughput
# benchmark by default, the batch-size one with DAVINCI_E2E_BENCH=sizes.
# Every DAVINCI_E2E_* variable set here is passed through, e.g.
#
#   DAVINCI_E2E_BENCH_PROVERS=http://127.0.0.1:8080,http://10.200.0.27:8080 e2e/bench.sh
#
# With DAVINCI_E2E_DEMO=prepare, check or run it runs that phase of the demo
# driver (tests/demo.rs) instead, against the live nodes for run;
# DAVINCI_DEMO_* variables are passed through too:
#
#   DAVINCI_E2E_DEMO=prepare e2e/bench.sh
#   DAVINCI_E2E_DEMO=check e2e/bench.sh    # offline: every planned ballot proves
#   DAVINCI_E2E_DEMO=run \
#     DAVINCI_DEMO_BASE_URL=https://raw.githubusercontent.com/vocdoni/davinci-sequencer/<commit>/e2e/demo \
#     e2e/bench.sh
#
# prepare writes e2e/demo (the only writable part of the checkout). Every
# phase keeps its secrets in DAVINCI_DEMO_DIR (default
# ~/.davinci-gnosis/demo, created mode 0700); run also mounts the organizer
# key file DAVINCI_DEMO_ORGANIZER_KEY (default ~/gnosis-chain-privkey.txt)
# read-only.
#
# With DAVINCI_E2E=1 it runs the acceptance test (tests/e2e.rs) instead, on
# the released node image (NODE_IMAGE default ...:latest). Live, the key
# files named by DAVINCI_E2E_ORGANIZER_KEY and DAVINCI_E2E_SEQUENCER_KEYS
# are mounted read-only and passed on under their container paths:
#
#   DAVINCI_E2E=1 DAVINCI_E2E_LIVE=1 DAVINCI_E2E_DKG=1 DAVINCI_E2E_NEGATIVE=1 \
#     DAVINCI_E2E_ORGANIZER_KEY=/path/org.key \
#     DAVINCI_E2E_SEQUENCER_KEYS=/path/seq1.key,/path/seq2.key,/path/seq3.key \
#     DAVINCI_ZKVM_URL=http://127.0.0.1:8080 e2e/bench.sh
#
# Host paths (defaults: siblings of this checkout): DAVINCI_ZKVM_DIR,
# DAVINCI_CONTRACTS_DIR (branch zkvm, with submodules; forge writes its
# out/ and cache there), DAVINCI_CENSUS_CONTRACT_DIR, CIRCOM_ARTIFACTS.
# BENCH_RUNS (default ~/.cache/davinci-bench) gets the report, the log and,
# on failure, the node logs and datadirs; BENCH_LOG overrides the log path.
# BENCH_MEMORY caps the container (default 24g); the provers run elsewhere.
# NODE_IMAGE picks the node build (default ghcr.io/vocdoni/davinci-sequencer:main,
# :latest for the acceptance test);
# BENCH_PULL=0 skips refreshing it and the base images.
set -euo pipefail

seq_dir=$(cd "$(dirname "$0")/.." && pwd)
parent=$(dirname "$seq_dir")
zkvm=${DAVINCI_ZKVM_DIR:-$parent/davinci-zkvm}
contracts=${DAVINCI_CONTRACTS_DIR:-$parent/davinci-contracts}
census=${DAVINCI_CENSUS_CONTRACT_DIR:-$parent/davinci-onchain-census-contract}
circom=${CIRCOM_ARTIFACTS:-$parent/davinci-circom/artifacts}
runs=${BENCH_RUNS:-$HOME/.cache/davinci-bench}
image=${BENCH_IMAGE:-davinci-sequencer-bench}
node_image=${NODE_IMAGE:-ghcr.io/vocdoni/davinci-sequencer:main}
demo=${DAVINCI_E2E_DEMO:-}

need_dir() {
    [[ -d $1 ]] || { echo "missing $1" >&2; exit 1; }
}
need_zkey() {
    [[ -f $circom/ballot_proof_pkey.zkey ]] || { echo "no ballot zkey in $circom" >&2; exit 1; }
}

need_dir "$zkvm/rust-sdk"
mounts=(-v "$seq_dir:/work/davinci-sequencer:ro" -v "$zkvm:/work/davinci-zkvm:ro")
if [[ -n $demo ]]; then
    private=${DAVINCI_DEMO_DIR:-$HOME/.davinci-gnosis/demo}
    mkdir -p "$private" "$seq_dir/e2e/demo"
    chmod 700 "$private"
    mounts+=(-v "$private:/demo-private" -e DAVINCI_DEMO_DIR=/demo-private)
    case $demo in
        prepare)
            mounts+=(-v "$seq_dir/e2e/demo:/work/davinci-sequencer/e2e/demo")
            ;;
        check)
            need_zkey
            mounts+=(-v "$circom:/work/davinci-circom/artifacts:ro")
            ;;
        run)
            key=${DAVINCI_DEMO_ORGANIZER_KEY:-$HOME/gnosis-chain-privkey.txt}
            [[ -f $key ]] || { echo "no organizer key file $key" >&2; exit 1; }
            [[ -f $census/out/OwnedCensus.sol/OwnedCensus.json ]] \
                || { echo "no OwnedCensus build in $census/out (forge build)" >&2; exit 1; }
            need_zkey
            mounts+=(
                -v "$key:/demo-keys/organizer.key:ro"
                -e DAVINCI_DEMO_ORGANIZER_KEY=/demo-keys/organizer.key
                -v "$census:$census:ro" -e "DAVINCI_CENSUS_CONTRACT_DIR=$census"
                -v "$circom:/work/davinci-circom/artifacts:ro"
            )
            ;;
        *) echo "DAVINCI_E2E_DEMO: prepare, check or run" >&2; exit 1 ;;
    esac
    test=demo
    kind=demo-$demo
elif [[ ${DAVINCI_E2E:-} == 1 ]]; then
    for d in "$contracts/src" "$census/src" "$circom"; do
        need_dir "$d"
    done
    need_zkey
    node_image=${NODE_IMAGE:-ghcr.io/vocdoni/davinci-sequencer:latest}
    test=e2e
    kind=e2e
    : "${BENCH_NAME:=davinci-e2e}"
    # The scenario runs forge build in the census project, so it is writable.
    mounts+=(
        -e DAVINCI_E2E=1
        -v "$contracts:$contracts" -e "DAVINCI_CONTRACTS_DIR=$contracts"
        -v "$census:$census" -e "DAVINCI_CENSUS_CONTRACT_DIR=$census"
        -v "$circom:/work/davinci-circom/artifacts:ro"
    )
    if [[ ${DAVINCI_E2E_LIVE:-} == 1 ]]; then
        # Absolute paths: docker takes a relative one for a volume name.
        org=${DAVINCI_E2E_ORGANIZER_KEY:-}
        [[ -f $org ]] || { echo "DAVINCI_E2E_ORGANIZER_KEY: no key file '$org'" >&2; exit 1; }
        mounts+=(-v "$(realpath "$org"):/e2e-keys/organizer.key:ro"
            -e DAVINCI_E2E_ORGANIZER_KEY=/e2e-keys/organizer.key)
        IFS=, read -ra seq_keys <<< "${DAVINCI_E2E_SEQUENCER_KEYS:-}"
        paths=()
        for k in "${seq_keys[@]}"; do
            [[ -n $k ]] || continue
            [[ -f $k ]] || { echo "DAVINCI_E2E_SEQUENCER_KEYS: no key file '$k'" >&2; exit 1; }
            paths+=("/e2e-keys/seq${#paths[@]}.key")
            mounts+=(-v "$(realpath "$k"):${paths[-1]}:ro")
        done
        (( ${#paths[@]} )) || { echo "DAVINCI_E2E_SEQUENCER_KEYS: no key files" >&2; exit 1; }
        mounts+=(-e "DAVINCI_E2E_SEQUENCER_KEYS=$(IFS=,; echo "${paths[*]}")")
        # Mapped to their container paths above, not passed through.
        unset DAVINCI_E2E_ORGANIZER_KEY DAVINCI_E2E_SEQUENCER_KEYS
    elif [[ ${DAVINCI_E2E_DKG:-} == 1 ]]; then
        echo "DAVINCI_E2E_DKG on anvil builds davinci-dkg-node, which the image cannot: run it live" >&2
        exit 1
    fi
else
    for d in "$contracts/src" "$census/src" "$circom"; do
        need_dir "$d"
    done
    need_zkey
    case ${DAVINCI_E2E_BENCH:=throughput} in
        throughput) test=throughput ;;
        sizes) test=bench ;;
        *) echo "DAVINCI_E2E_BENCH: throughput or sizes" >&2; exit 1 ;;
    esac
    export DAVINCI_E2E_BENCH
    kind=$DAVINCI_E2E_BENCH
    mounts+=(
        -v "$contracts:$contracts" -e "DAVINCI_CONTRACTS_DIR=$contracts"
        -v "$census:$census:ro" -e "DAVINCI_CENSUS_CONTRACT_DIR=$census"
        -v "$circom:/work/davinci-circom/artifacts:ro"
    )
fi

mkdir -p "$runs"
stamp=$(date -u +%Y%m%d-%H%M%S)
log=${BENCH_LOG:-$runs/$kind-$stamp.log}
[[ -n $demo || $test == e2e ]] || export DAVINCI_E2E_BENCH_OUT=/runs/$kind-$stamp.md

pull=()
if [[ ${BENCH_PULL:-1} != 0 ]]; then
    docker pull -q "$node_image" >/dev/null
    pull=(--pull)
fi
docker build "${pull[@]}" -q --build-arg "NODE_IMAGE=$node_image" -t "$image" - \
    < "$seq_dir/e2e/Dockerfile.bench" >/dev/null

# The demo's host paths are mapped above, not passed through.
env_args=()
while IFS= read -r v; do
    env_args+=(-e "$v")
done < <(compgen -e | grep -E '^DAVINCI_E2E_|^DAVINCI_ZKVM_URL$|^DAVINCI_DEMO_' \
    | grep -vE '^DAVINCI_DEMO_(DIR|ORGANIZER_KEY)$' || true)

{
    echo "node image: $(docker image inspect --format '{{index .RepoDigests 0}}' \
        "$node_image" 2>/dev/null || echo "$node_image")"
    [[ -n $demo || $test == e2e ]] || echo "report: $runs/$kind-$stamp.md"
} | tee "$log"

# Host network: provers and nodes on loopback and on the private network are
# reachable, and the nodes and anvil bind 127.0.0.1 as they do natively.
# The container is the scope: stopping it kills anvil and the nodes. The
# forge projects keep their host paths, which forge's cache records.
docker run --rm --init --name "${BENCH_NAME:-davinci-${demo:+demo-}bench}" \
    --network host \
    --memory "${BENCH_MEMORY:-24g}" \
    --user "$(id -u):$(id -g)" \
    -v davinci-bench-cache:/cache \
    -v "$runs:/runs" \
    "${mounts[@]}" \
    "${env_args[@]}" \
    "$image" \
    cargo test --locked -p davinci-e2e --test "$test" -- --nocapture \
    2>&1 | tee -a "$log"
