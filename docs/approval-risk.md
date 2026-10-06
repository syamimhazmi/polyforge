# TypeSafe approval risk

When a DIFF approval opens, polyforge asks TypeSafe (Jev) three judgments
in one call: risk Score, secrets Noul, destructive Noul. Code composes a
LOW/MED/HIGH band and shows it on the modal. y/n/a/q stay under user
control.

Key load order:

1. `TYPESAFE_API_KEY` environment variable
2. `~/.config/typesafe/env` (or `$XDG_CONFIG_HOME/typesafe/env`)

Missing key: approvals work unscored.
