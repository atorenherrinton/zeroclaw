# ZeroClaw TypeSafe offline evaluation

See the [design, exact commands, schemas and limitations](../../docs/book/src/contributing/typesafe-evaluation.md).

This standalone Rust command reads an explicitly selected existing trace offline.
It never starts an agent or makes Jev calls. A successful run is not proof of a
live A/B experiment or improved response quality.
