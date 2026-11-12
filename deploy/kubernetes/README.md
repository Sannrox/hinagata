# Tenkai product on Kubernetes

This directory is the template for hinagata's Tenkai product. Each release
renders it with `scripts/render-tenkai-product.sh` and attaches the result
to the GitHub Release as `hinagata-<version>-tenkai-kubernetes.tar.gz`. Install
the rendered product. The template itself has placeholders and is not
installable.

## What it installs

- **StatefulSet `hinagata`**, one replica, image pinned by digest. Readiness
  and liveness are `/healthz`, which is ready only while the registry database
  and the artifact store can do work.
- **Volume `data`** at `/data` (1 Gi, `ReadWriteOnce`). The claim outlives the
  pod, restarts, upgrades and an uninstall. The SQLite file and local artifacts
  live here (`HINAGATA_DB_PATH=/data/hinagata.db`,
  `HINAGATA_ARTIFACT_DIR=/data/artifacts`).
- **Service `hinagata`** on port 7700.
- **NetworkPolicy `hinagata`**: only pods in the same namespace labeled
  `hinagata/caller: "true"` reach port 7700. Add that label to each caller.

The product declares `executor = "kubernetes-inprocess"`, so Tenkai applies
`manifests/` with server-side apply into the environment's namespace. The
image is the digest the tagged release pushed; the product has no pull
secret, so that GHCR package must be public (see RELEASING.md).

## Contract

The compatibility profile (version 2) provides `hinagata.registry` at `1.0.0`:
the `hinagata.v1.Hinagata` gRPC service and its HTTP/JSON surface. A dependent
declares, for example:

```toml
[compatibility.components.platform.requires]
"hinagata.registry" = { minimum = "1.0.0", optional = true }
```

The component pin is the image digest, so owner evidence for a release
names that digest. [RELEASING.md](../../RELEASING.md#tenkai-product) says
when the contract version moves.

## Configuration

The product ships no credentials. Before the first apply, create Secret
`hinagata-config` in the environment namespace. It is mounted read-only at
`/etc/hinagata`. The default product reads a static token:

- key `token.txt`: the bearer token (the file `HINAGATA_STATIC_TOKEN_FILE` names)
- key `namespace`: the namespace that token acts in (`HINAGATA_STATIC_NAMESPACE`)

```sh
kubectl -n <environment> create secret generic hinagata-config \
  --from-file=token.txt --from-literal=namespace=prod
```

`/healthz` stays unready until the process can open the database and the
artifact directory. The server still refuses to start without one of the
documented auth hooks. To verify JWTs instead of a static token, put `jwks.json`
in the same Secret and replace the static-token env vars on the StatefulSet
with `HINAGATA_JWKS_FILE` and `HINAGATA_JWT_AUDIENCE` (optionally issuer and
tenant claim). Put a signing seed on the volume or in the Secret if the
registry should sign manifests; the product does not ship one.

S3-compatible artifact storage stays operator config (`HINAGATA_S3_BUCKET`).
The product's default is local disk on the volume.

## Availability

One replica on the embedded store. A lost pod is restarted on its volume, and
calls fail until `/healthz` is ready again.

## Acceptance check

`scripts/kind-product-check.sh` runs the product in a throwaway kind cluster
with a local registry and an embedded Tenkai store. It needs docker, kind
v0.24 or later (its default network enforces NetworkPolicy), kubectl, gh and
`TENKAICTL` pointing at a tenkaictl that understands profile v2. It checks
that a consumer requiring `hinagata.registry` is refused with no provider and
admitted once hinagata is installed, that only labeled callers get through,
and that state survives a pod restart and an upgrade from the latest release's
image to one built from the checkout.
