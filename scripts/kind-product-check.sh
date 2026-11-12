#!/usr/bin/env bash
# Kind acceptance for the Tenkai Kubernetes product (deploy/kubernetes).
# Local only: it needs docker, kind v0.24 or later (its default network
# enforces NetworkPolicy), kubectl, gh, and TENKAICTL naming a tenkaictl that
# understands compatibility profile v2.
#
# In a throwaway kind cluster with a local registry and an embedded Tenkai
# store, it checks that:
#   1. a consumer that requires `hinagata.registry` is refused while no hinagata is installed;
#   2. the previous release's image installs and passes readiness, and then
#      the consumer is admitted and becomes ready by calling it;
#   3. only pods labeled `hinagata/caller: "true"` reach hinagata;
#   4. a model written through the API survives a pod restart and an upgrade
#      to an image built from this checkout.
#
# Usage: TENKAICTL=/path/to/tenkaictl bash scripts/kind-product-check.sh
#   PREVIOUS_TAG=v0.1.0   release whose image tar is the upgrade start (default: latest release)
#   KEEP=1                keep the cluster, registry and work directory for inspection
set -euo pipefail

ROOT_PATH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
RUN="hinagata-kind-$$-${RANDOM}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/${RUN}.XXXXXX")"
REGISTRY="${RUN}-registry"
CRANE="gcr.io/go-containerregistry/crane:v0.22.1"
ENV_NAME="local"
TOKEN="kind-token"

say() { printf '\n== %s\n' "$*"; }
fail() { printf 'kind-product-check: %s\n' "$*" >&2; exit 1; }

for tool in docker kind kubectl gh python3; do
  command -v "$tool" >/dev/null || fail "missing ${tool}"
done
[[ -x "${TENKAICTL:-}" ]] || fail "set TENKAICTL to a tenkaictl binary with compatibility profile v2"

cleanup() {
  if [[ "${KEEP:-0}" == 1 ]]; then
    printf 'kept cluster %s, registry %s, work dir %s\n' "$RUN" "$REGISTRY" "$WORK"
    return
  fi
  kind delete cluster --name "$RUN" >/dev/null 2>&1 || true
  docker rm -f "$REGISTRY" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

export TENKAI_DATABASE="${WORK}/tenkai.db"
# The embedded store's development token, as in Tenkai's local dogfood.
export TENKAI_MANAGEMENT_TOKEN="${TENKAI_MANAGEMENT_TOKEN:-tenkai-local-management}"
KUBECONFIG_FILE="${WORK}/kubeconfig"
# Force the throwaway store even if the operator's shell is set to a remote target.
tenkai() { "$TENKAICTL" --target embedded --database "$TENKAI_DATABASE" "$@"; }
k() { kubectl --kubeconfig "$KUBECONFIG_FILE" -n "$ENV_NAME" "$@"; }

say "registry and cluster"
# The registry is reachable only on the kind network, by container name.
# Images are pushed from a container on that network, not by the docker
# daemon, whose localhost may sit behind a proxy.
docker run -d --name "$REGISTRY" registry:2 >/dev/null
cat >"${WORK}/kind.yaml" <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".registry]
      config_path = "/etc/containerd/certs.d"
EOF
kind create cluster --name "$RUN" --kubeconfig "$KUBECONFIG_FILE" --config "${WORK}/kind.yaml" --wait 120s
docker network connect kind "$REGISTRY"
for node in $(kind get nodes --name "$RUN"); do
  docker exec "$node" mkdir -p "/etc/containerd/certs.d/${REGISTRY}:5000"
  printf 'server = "http://%s:5000"\n' "$REGISTRY" | docker exec -i "$node" cp /dev/stdin "/etc/containerd/certs.d/${REGISTRY}:5000/hosts.toml"
done

# Pushes an image tarball (docker save format) and prints its reference by digest.
push() {
  local tar="$1" name="${REGISTRY}:5000/hinagata:$2" ref
  ref="$(docker run --rm --network kind -v "$(dirname "$tar"):/images:ro" "$CRANE" \
    push --insecure "/images/$(basename "$tar")" "$name")"
  [[ "$ref" =~ ^${REGISTRY}:5000/hinagata@sha256:[0-9a-f]{64}$ ]] || fail "no registry digest for ${name}: ${ref}"
  printf '%s\n' "$ref"
}

say "images"
PREVIOUS_TAG="${PREVIOUS_TAG:-$(gh release view --repo Sannrox/hinagata --json tagName --jq .tagName)}"
gh release download "$PREVIOUS_TAG" --repo Sannrox/hinagata -p "hinagata-${PREVIOUS_TAG#v}-image.tar" -D "$WORK"
docker build -q -f "${ROOT_PATH}/Dockerfile" -t "${RUN}-current:local" "$ROOT_PATH" >/dev/null
docker save -o "${WORK}/current-image.tar" "${RUN}-current:local"
docker rmi "${RUN}-current:local" >/dev/null
PREVIOUS_IMAGE="$(push "${WORK}/hinagata-${PREVIOUS_TAG#v}-image.tar" previous)"
CURRENT_IMAGE="$(push "${WORK}/current-image.tar" current)"
# Product versions are local labels; the images are what is being tested.
PREVIOUS_VERSION="0.0.0-kind.1"
CURRENT_VERSION="0.0.0-kind.2"
bash "${ROOT_PATH}/scripts/render-tenkai-product.sh" --image "$PREVIOUS_IMAGE" --version "$PREVIOUS_VERSION" --out "${WORK}/previous"
bash "${ROOT_PATH}/scripts/render-tenkai-product.sh" --image "$CURRENT_IMAGE" --version "$CURRENT_VERSION" --out "${WORK}/current"

say "consumer product"
CONSUMER_PIN="sha256:$(printf 'hinagata-kind-consumer' | shasum -a 256 | cut -d' ' -f1)"
mkdir -p "${WORK}/consumer/manifests"
cat >"${WORK}/consumer/tenkai.toml" <<EOF
[product]
name = "hinagata-consumer"
version = "0.1.0"
description = "Declares a requirement on hinagata.registry and calls it"
kind = "software"

[deploy]
workdir = "."
executor = "kubernetes-inprocess"
inputs = ["manifests"]

[compatibility]
version = 2
max_evidence_age_ms = 86400000

[compatibility.components.hinagata-consumer.pin]
kind = "digest"
value = "${CONSUMER_PIN}"

[compatibility.components.hinagata-consumer.requires]
"hinagata.registry" = "1.0.0"
EOF
# Ready only once it reaches hinagata, so the apply proves the policy admits callers.
cat >"${WORK}/consumer/manifests/10-deployment.yaml" <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: hinagata-consumer
spec:
  replicas: 1
  selector:
    matchLabels:
      app: hinagata-consumer
  template:
    metadata:
      labels:
        app: hinagata-consumer
        hinagata/caller: "true"
    spec:
      containers:
        - name: caller
          image: busybox:1.37
          command: ["sleep", "infinity"]
          readinessProbe:
            exec:
              command: ["wget", "-q", "-T", "2", "-O", "/dev/null", "http://hinagata:7700/healthz"]
            periodSeconds: 3
EOF

say "tenkai store"
tenkai init >/dev/null
tenkai env cluster-config set "$ENV_NAME" "$KUBECONFIG_FILE"
kubectl --kubeconfig "$KUBECONFIG_FILE" create namespace "$ENV_NAME"
printf '%s' "$TOKEN" >"${WORK}/token.txt"
k create secret generic hinagata-config --from-file=token.txt="${WORK}/token.txt" --from-literal=namespace=kind
for product in previous current consumer; do
  tenkai publish "${WORK}/${product}/tenkai.toml" --allow-unsigned-development
done

# Records owner evidence for one release: its component pin and contracts.
observe() {
  local release="$1" component="$2" pin="$3" requires="$4" provides="$5"
  local digest
  digest="$(tenkai release inspect "$release" | python3 -c 'import json,sys; print(json.load(sys.stdin)["manifest_digest"])')"
  python3 - "$ENV_NAME" "$digest" "$component" "$pin" "$requires" "$provides" >"${WORK}/evidence.json" <<'PY'
import json, sys, time
env, digest, component, pin, requires, provides = sys.argv[1:]
print(json.dumps({
    "version": 1,
    "environment": env,
    "release_digest": digest,
    "observed_at_ms": int(time.time() * 1000),
    "components": {component: {
        "pin": {"kind": "digest", "value": pin},
        "requires": json.loads(requires),
        "provides": json.loads(provides),
    }},
    "contracts": {},
    "capabilities": [],
    "migration": "stable",
}))
PY
  tenkai env compatibility record "$ENV_NAME" "${WORK}/evidence.json"
}
digest_of() { printf '%s' "${1##*@}"; }
observe "hinagata-consumer@0.1.0" hinagata-consumer "$CONSUMER_PIN" '{"hinagata.registry": "1.0.0"}' '{}'
observe "hinagata@${PREVIOUS_VERSION}" hinagata "$(digest_of "$PREVIOUS_IMAGE")" '{}' '{"hinagata.registry": "1.0.0"}'
observe "hinagata@${CURRENT_VERSION}" hinagata "$(digest_of "$CURRENT_IMAGE")" '{}' '{"hinagata.registry": "1.0.0"}'

# Plans the environment and applies the plan; prints the plan output.
plan_apply() {
  local out id
  out="$(tenkai plan --env "$ENV_NAME")"
  printf '%s\n' "$out"
  id="$(printf '%s\n' "$out" | sed -n 's/^plan id: //p' | tail -1)"
  [[ -n "$id" ]] || fail "no plan id in plan output"
  tenkai apply "$id" --allow-unapproved-development --development-reason "kind product check"
}

# An explicit plan is satisfied only by a provider already deployed, so
# hinagata installs first and the consumer is planned after it.
say "1. consumer without hinagata is refused"
if refusal="$(tenkai env compatibility check "$ENV_NAME" "hinagata-consumer@0.1.0" 2>&1)"; then
  fail "the consumer was admitted without a hinagata.registry provider: ${refusal}"
fi
if ! grep -q 'missing_contract' <<<"$refusal" || ! grep -q 'hinagata.registry' <<<"$refusal"; then
  fail "refusal does not name the missing hinagata.registry contract: ${refusal}"
fi
echo "refused as expected: missing_contract hinagata.registry"

say "2. install ${PREVIOUS_TAG} image, then the consumer"
tenkai promote "hinagata@${PREVIOUS_VERSION}" stable
tenkai env subscribe "$ENV_NAME" hinagata=stable
plan_apply
k wait --for=condition=Ready pod/hinagata-0 --timeout=120s
k get pod hinagata-0 -o jsonpath='{.spec.containers[0].image}{"\n"}' | grep -qxF "$PREVIOUS_IMAGE" || fail "hinagata-0 does not run the previous image"
tenkai env compatibility check "$ENV_NAME" "hinagata-consumer@0.1.0"
tenkai promote "hinagata-consumer@0.1.0" stable
tenkai env subscribe "$ENV_NAME" hinagata-consumer=stable
plan_apply
k rollout status deployment/hinagata-consumer --timeout=120s

say "3. only declared callers reach hinagata"
if k run "not-a-caller-$$" --image=busybox:1.37 --restart=Never --rm -i --quiet --command -- \
  wget -q -T 3 -O /dev/null http://hinagata:7700/healthz 2>/dev/null; then
  fail "a pod without the caller label reached hinagata"
fi
k run "caller-$$" --image=busybox:1.37 --restart=Never --rm -i --quiet --labels='hinagata/caller=true' --command -- \
  wget -q -T 3 -O /dev/null http://hinagata:7700/healthz || fail "a labeled caller could not reach hinagata"
echo "unlabeled pod refused, labeled caller admitted"

caller() {
  k run "api-$$-$RANDOM" --image=busybox:1.37 --restart=Never --rm -i --quiet --labels='hinagata/caller=true' --command -- wget -q -O- "$@"
}

# delete --wait returns after the old pod is gone; wait until the
# StatefulSet has created a replacement before checking Ready.
wait_hinagata() {
  local old_uid="${1:-}" i new
  for i in $(seq 1 60); do
    new="$(k get pod hinagata-0 -o jsonpath='{.metadata.uid}' 2>/dev/null || true)"
    if [[ -n "$new" && ( -z "$old_uid" || "$new" != "$old_uid" ) ]]; then
      k wait --for=condition=Ready pod/hinagata-0 --timeout=120s
      return
    fi
    sleep 1
  done
  fail "hinagata-0 did not reappear"
}

say "4. state survives restart and upgrade"
created="$(caller --header="Authorization: Bearer ${TOKEN}" --header='Content-Type: application/json' \
  --post-data='{"name":"kind-check","description":"kept"}' http://hinagata:7700/v1/models)"
grep -qF '"name":"kind-check"' <<<"$created" || fail "create model failed: ${created}"
old_uid="$(k get pod hinagata-0 -o jsonpath='{.metadata.uid}')"
k delete pod hinagata-0 --wait=true
wait_hinagata "$old_uid"
kept="$(caller --header="Authorization: Bearer ${TOKEN}" http://hinagata:7700/v1/models/kind-check)"
grep -qF '"name":"kind-check"' <<<"$kept" || fail "model kind-check lost after restart: ${kept}"
echo "model kind-check kept after restart"
tenkai promote "hinagata@${CURRENT_VERSION}" stable
plan_apply
wait_hinagata
k get pod hinagata-0 -o jsonpath='{.spec.containers[0].image}{"\n"}' | grep -qxF "$CURRENT_IMAGE" || fail "hinagata-0 does not run the current image"
kept="$(caller --header="Authorization: Bearer ${TOKEN}" http://hinagata:7700/v1/models/kind-check)"
grep -qF '"name":"kind-check"' <<<"$kept" || fail "model kind-check lost after upgrade: ${kept}"
echo "model kind-check kept after upgrade"
k rollout status deployment/hinagata-consumer --timeout=60s

say "passed"
