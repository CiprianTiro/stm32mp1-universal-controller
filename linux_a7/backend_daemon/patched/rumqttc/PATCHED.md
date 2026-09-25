# rumqttc 0.25.1 — patched copy

This is the published [rumqttc 0.25.1](https://crates.io/crates/rumqttc/0.25.1)
crate (Apache-2.0, © Bytebeam; unchanged source from crates.io) with
**one line** changed in `Cargo.toml`:

```diff
 [dependencies.rustls-webpki]
-version = "0.102.8"
+version = "0.103.13"
```

## Why (issue #37)

`rustls-webpki` 0.102 has known vulnerabilities (RUSTSEC-2026-0049, -0098,
-0099, -0104), fixed only in 0.103.12/0.103.13. Upstream rumqttc (latest
release 0.25.1, and its `main` branch as of 2026-09-25) still asks for 0.102.
rumqttc only uses that crate for one error type (`webpki::Error` in
`src/tls.rs`), which exists unchanged in 0.103, so nothing else needs to
change. With this, only the patched 0.103 is in the hub's build, and
`cargo audit` reports no vulnerabilities.

Used through `[patch.crates-io]` in `../../Cargo.toml`.

## What was left out

Only what the hub's build compiles is kept: `src/`, `Cargo.toml`, the
upstream `README.md`, and this file. Removed from the published crate:
`examples/`, `tests/`, `certs/` (test certificates), `CHANGELOG.md`,
`design.md` and `Cargo.toml.orig`. `Cargo.toml` still lists the examples
and tests (its `[[example]]`/`[[test]]` entries are left as published, so
the version above stays the only change); Cargo never builds those for a
dependency, so their missing files don't matter.

## When to remove it

As soon as a rumqttc release depends on `rustls-webpki` >= 0.103.13: delete
this folder and the `[patch.crates-io]` entry, bump `rumqttc` in
`Cargo.toml`, and run `make audit`.
