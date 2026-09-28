#!/usr/bin/env bash
# Runs an e2e benchmark in the e2e/Dockerfile.bench image: the throughput
# benchmark by default, the batch-size one with DAVINCI_E2E_BENCH=sizes.
# Every DAVINCI_E2E_* variable set here is passed through, e.g.
#
#   DAVINCI_E2E_BENCH_PROVERS=http://127.0.0.1:8080,http://10.200.0.27:8080 e2e/bench.sh
#
# Host paths (defaults: siblings of this checkout): DAVINCI_ZKVM_DIR,
# DAVINCI_CONTRACTS_DIR (branch zkvm, with submodules; forge writes its
# out/ and cache there), DAVINCI_CENSUS_CONTRACT_DIR, CIRCOM_ARTIFACTS.
# BENCH_RUNS (default ~/.cache/davinci-bench) gets the report, the log and,
# on failure, the node logs and datadirs; BENCH_LOG overrides the log path.
# BENCH_MEMORY caps the container (default 24g); the provers run elsewhere.
# NODE_IMAGE picks the node build (default ghcr.io/vocdoni/davinci-sequencer:main);
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

for d in "$zkvm/rust-sdk" "$contracts/src" "$census/src" "$circom"; do
    [[ -d $d ]] || { echo "missing $d" >&2; exit 1; }
done
[[ -f $circom/ballot_proof_pkey.zkey ]] || { echo "no ballot zkey in $circom" >&2; exit 1; }

case ${DAVINCI_E2E_BENCH:=throughput} in
    throughput) test=throughput ;;
    sizes) test=bench ;;
    *) echo "DAVINCI_E2E_BENCH: throughput or sizes" >&2; exit 1 ;;
esac
export DAVINCI_E2E_BENCH

mkdir -p "$runs"
stamp=$(date -u +%Y%m%d-%H%M%S)
log=${BENCH_LOG:-$runs/$DAVINCI_E2E_BENCH-$stamp.log}
export DAVINCI_E2E_BENCH_OUT=/runs/$DAVINCI_E2E_BENCH-$stamp.md

pull=()
if [[ ${BENCH_PULL:-1} != 0 ]]; then
    docker pull -q "$node_image" >/dev/null
    pull=(--pull)
fi
docker build "${pull[@]}" -q --build-arg "NODE_IMAGE=$node_image" -t "$image" - \
    < "$seq_dir/e2e/Dockerfile.bench" >/dev/null

env_args=()
while IFS= read -r v; do
    env_args+=(-e "$v")
done < <(compgen -e | grep -E '^DAVINCI_E2E_|^DAVINCI_ZKVM_URL$' || true)

{
    echo "node image: $(docker image inspect --format '{{index .RepoDigests 0}}' \
        "$node_image" 2>/dev/null || echo "$node_image")"
    echo "report: $runs/$DAVINCI_E2E_BENCH-$stamp.md"
} | tee "$log"

# Host network: provers on loopback and on the private network are
# reachable, and the nodes and anvil bind 127.0.0.1 as they do natively.
# The container is the scope: stopping it kills anvil and the nodes. The
# forge projects keep their host paths, which forge's cache records.
docker run --rm --init --name "${BENCH_NAME:-davinci-bench}" \
    --network host \
    --memory "${BENCH_MEMORY:-24g}" \
    --user "$(id -u):$(id -g)" \
    -v davinci-bench-cache:/cache \
    -v "$runs:/runs" \
    -v "$seq_dir:/work/davinci-sequencer:ro" \
    -v "$zkvm:/work/davinci-zkvm:ro" \
    -v "$contracts:$contracts" -e "DAVINCI_CONTRACTS_DIR=$contracts" \
    -v "$census:$census:ro" -e "DAVINCI_CENSUS_CONTRACT_DIR=$census" \
    -v "$circom:/work/davinci-circom/artifacts:ro" \
    "${env_args[@]}" \
    "$image" \
    cargo test --locked -p davinci-e2e --test "$test" -- --nocapture \
    2>&1 | tee -a "$log"
