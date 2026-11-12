# Releasing hinagata

Family versioning, tagging, and release policy:
[tenkai ADR 0040](https://github.com/Sannrox/tenkai/blob/main/docs/decisions/0040-family-versioning-and-release-policy.md).

This page is hinagata-local: how a maintainer prepares and tags **this**
repository.

## Compatibility of configuration, CLI and file formats

- **Configuration.** The server is configured by `HINAGATA_*` environment
  variables. A variable is renamed or removed only after one minor release
  that still honours it and logs a deprecation warning naming its
  replacement; a setting never changes meaning under the same name.
- **Versioned files.** The trusted keys file may declare `"version": 1`;
  omitted means the current version and any other value is refused. Export
  bundles carry `bundle.json` `{"format": "hinagata.bundle/v1"}` and an
  unknown format is refused. A change older readers cannot handle gets a
  new version or format id.
- **CLI.** The `verify`, `export` and `import` subcommands and their flags
  are removed only after one minor release that still accepts them and
  prints a deprecation warning naming the replacement. Their output is
  human-readable text, not a stable API; a JSON mode, if added, is
  versioned like an API.

## Who cuts a tag

Agents never push `v*` tags. After the version-bump pull request merges, the
maintainer creates an annotated, signed `vX.Y.Z` tag with the registered
release-signing key. The tag must equal the crate version in `Cargo.toml`.
The workflow refuses unsigned or lightweight tags.

## Prepare

1. Choose the target version. Do not invent one in a feature pull request.
2. Draft notes with `scripts/changelog-draft.sh`. Pull requests never edit
   `CHANGELOG.md`; the release step writes the section from merged work.
3. Land the version-bump pull request. The crate version and CHANGELOG
   heading move together.

## Tag

Create an annotated, signed tag on the merged release commit on `main`:

```text
vX.Y.Z
```

A tag is never moved, deleted, or reused. The tagged commit must be on
`main`; the workflow refuses any other commit, so there are no hotfix tags
on release branches.

## Release candidates

Cut `vX.Y.Z-rc.N` (or `vX.Y.Z-beta.N`) before the final tag:

1. Land a version-bump pull request that sets the crate version to
   `X.Y.Z-rc.N` and adds a `## [X.Y.Z-rc.N]` section to `CHANGELOG.md`.
   The final `vX.Y.Z` needs its own section; it never reuses the rc notes.
2. Tag `vX.Y.Z-rc.N` on that commit.
3. The release publishes as a GitHub pre-release and is never marked latest.

A failed release candidate costs only an rc number. A final tag on a commit
whose crate version is still `X.Y.Z-rc.N` is refused. Gate flags and
`scripts/changelog-draft.sh` count from the previous final tag, so a final
release still covers what landed before its candidates.

## Start, retry, and dry-run

A `v*` tag push runs `.github/workflows/release-dispatch.yml`. It only runs
`gh workflow run release.yml --ref main -f tag=<tag>`, so the release always
runs **main's** `release.yml`, never the copy at the tagged commit.

`release.yml` refuses a tagged run that does not start from `main`. It then:

- checks out the tag and fetches the annotated tag object
- verifies the signed tag with main's `scripts/verify-release-tag.sh` and
  main's allowed keys against the tagged commit's `Cargo.toml`
- requires the tagged commit to be on `main`
- runs the release scripts check, builds, tests, packages the binary and the
  version-only image (no `latest` tag), and writes the manifest `git_sha`,
  all from that commit
- writes an SBOM, attests provenance, drafts the GitHub Release with
  `gh release create <tag> --verify-tag` from the matching CHANGELOG section,
  attaches the signed family release manifest (component, version, git SHA,
  binary and image digests, attestation URLs, gate flags), and publishes last

Deploy the image by digest.

To retry a failed release, fix the workflow on `main` and run it again for the
same tag:

```sh
gh workflow run release.yml --ref main -f tag=vX.Y.Z
```

A retry picks up fixes to `release.yml` and to the tag verifier and keys.
Everything else (the release scripts, `CHANGELOG.md`, the `Dockerfile`, and
the sources) comes from the tagged commit, so a bug there needs a new rc or
patch version.

A re-run resumes the draft release: it deletes the draft's assets and uploads
the rebuilt ones. Every job that attests, uploads, or publishes refuses a
release that is already published, so neither a new run nor "Re-run failed
jobs" on an old one can change it; cut a new version instead. One run per tag
executes at a time.

Pull requests and `workflow_dispatch` without a tag dry-run the same
packaging path on that ref. Dry runs never build images, mint attestations,
or touch a GitHub Release.

## Tenkai product

A tagged run pushes the image as `ghcr.io/sannrox/hinagata:<version>` (never
`latest`) and attaches `hinagata-<version>-tenkai-kubernetes.tar.gz`: the
[Kubernetes product](deploy/kubernetes/README.md) rendered with
`scripts/render-tenkai-product.sh`, with the image and the profile's
component pin set to the pushed digest. The family manifest lists the pushed
image and the archive with their digests, and the archive is attested with
the other assets. A re-run of a draft rebuilds and re-pushes the version tag;
the attached product pins whatever that run pushed.

The product ships no `imagePullSecrets`. The first tagged run creates the
GHCR package; make that package public so environments can pull the digest
without registry credentials. Later tags reuse the same package.

The product provides one contract, `hinagata.registry`: the `hinagata.v1.Hinagata`
gRPC service and its HTTP/JSON surface. Its version is separate from the crate
version and lives in `deploy/kubernetes/tenkai.toml`. In the pull request that
changes the surface, bump:

- **major** when a caller written against the previous version breaks
  (a removed method, field, or route, or a stricter requirement on requests);
- **minor** for an addition a caller can ignore;
- **patch** for a fix that leaves the surface as it was.

Dependents declare `requires` on the contract at the minimum they are
tested against; Tenkai holds their upgrade until a compatible hinagata is
installed.

## Provenance

Attestations record the workflow that built the artifacts, which is main's
`release.yml` at `refs/heads/main`, not the tag. The source is pinned by the
signed tag and by the manifest `git_sha`, which is the tagged commit.

Allowed tag-signing keys live in `security/release-tag-fingerprints` with the
public key in `security/release-tag-key.asc`. A tagged run uses the copies on
`main`.
