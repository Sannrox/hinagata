# ADR 0001: Registry metadata lives in one embedded SQLite file

- Status: Accepted
- Date: 2026-10-23
- Source: [Issue #1](https://github.com/Sannrox/hinagata/issues/1)

## Context

The registry needs transactions: a version and its audit event commit
together, and later only one production version per model and target may
exist. Air-gapped installs should not need another service.

## Decision

1. **One embedded SQLite file, statically linked**, owned by the
   `registry` module and set by `HINAGATA_DB_PATH`. Artifacts stay in the
   artifact store; a version records only their digest.
2. **Every write is one transaction with its audit event.** An interrupted
   write leaves neither. Writes take the write lock first, so concurrent
   writers serialize.
3. **Immutability is enforced in the schema.** Triggers refuse any update
   to a registered version's content, any delete of a model or version,
   and any update or delete of an audit event. The stage is the only
   column that changes.
4. **Migrations are append only.** `user_version` counts the applied
   migrations. An older database opens and is upgraded in one
   transaction. A database from a newer schema is refused, never read with
   the wrong layout.
5. **Retries are safe.** A model is unique by name, and a version by
   artifact within its model. Repeating a create or register with the same
   content returns the stored record and records nothing. Different
   content is a conflict.

## Consequences

- One server owns a database file. A second backend for several
  instances is out of scope for v0.1 and is revisited when HA needs it.
- A rollback to a binary with fewer migrations refuses the newer file.
  Restore that binary's last backup, or roll forward.

## Rejected alternatives

- An external database server: it adds a service to every air-gapped
  install.
- JSON metadata in the artifact store: it has no transactions for the
  one-production-version rule.
