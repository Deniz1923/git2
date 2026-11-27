# Experimental v0.1 Release-Readiness Report (`0.1.0-alpha.1`)

> [!CAUTION]
> `git2` (`0.1.0-alpha.1`) is **experimental software and is NOT safe for important repositories**. It is ready only for local evaluation in disposable clones. Both workspace crates keep `publish = false`; creating a release tag or publishing binaries requires separate maintainer authorization.

## 1. Delivered Scope (Milestones 0–9)

- **Repository model & private recovery storage:** format `3\n` owner-private CAS under `.git/git2/` (`cas`, `views`, `operations`, `pending`, `policy`, `HEAD`), zlib-compressed SHA-256 content addressing, advisory writer lock, immutable operation log (`adopt`, `external-git`, `checkpoint`, `restore`, `reset`, `undo`, `add`, `commit`), and private Git object-closure retention.
- **Safety & crash recovery:** 4-phase pending journal (`Prepared`, `Applying`, `Finalizing`, `Committed`), descriptor-relative no-follow worktree & metadata path ops, foreign Git lock detection, `git2 recover [--status | --rollback]`, and child-process `abort()` crash matrices across restore, reset, undo, add, and commit.
- **User commands (Linux SHA-1 single-worktree non-bare repositories):** `adopt`, `checkpoint`, `add [--all] [--] [<path>...]`, `commit -m <message>`, `log [-n|--max-count <n>]`, `reset [--soft | --mixed | --hard] [<commit>]`, `undo [--dry-run]`, `restore --view <id> [--dry-run]`, `recover [--status | --rollback]`, `status [-z|--nul]`, `oplog`, and read-only `debug hash-object` / `debug cat-object`.
- **Differential & privacy verification:** byte-for-byte parity tests against ordinary `git` (`write-tree`, `cat-file`, `ls-files --stage -z`, `status --porcelain=v1 --untracked-files=all -z`), `git fsck --full --strict` verification on all mutating paths, `git push --mirror` privacy proof, and `PATH=""` offline binary execution smoke test.

## 2. Dependency and License Review

Verified via `cargo metadata --format-version 1` (`163` resolved packages total, `unsafe_code = "forbid"` in both workspace crates):

- **Workspace crates:** `git2-core` (`0.1.0-alpha.1`, `MIT`) and `git2-cli` (`0.1.0-alpha.1`, `MIT`).
- **Direct normal dependencies (`git2-core`):** `bstr`, `flate2`, `gix` (narrow feature set: `attributes`, `basic`, `sha1`, `status`, `worktree-mutation`), `gix-pack`, `rustix` (`fs`, `process`), `sha1`, `sha2`, `thiserror`.
- **Direct normal dependencies (`git2-cli`):** `git2-core`, `anyhow`, `clap`.
- **Transitive license audit:** all 163 resolved packages carry permissive / dual-permissive licenses (`MIT`, `Apache-2.0`, `BSD-3-Clause`, `0BSD`, `Unlicense`, `CC0-1.0`, `Zlib`, `Unicode-3.0`, plus one `MIT OR Apache-2.0 OR LGPL-2.1-or-later` crate usable under `MIT OR Apache-2.0`). No copyleft-only or unknown licenses are present.

## 3. Baseline Storage & Performance Measurements

Measured on a disposable Linux fixture using the compiled `--release` binary (`target/release/git2`) over 200 tracked files (~1 KiB each across 10 directories):

| Operation | Measured elapsed time |
| --- | --- |
| `git2 adopt` (200 files) | ~117.2 ms |
| `git2 status -z` (clean, after adopt) | ~33.4 ms |
| `git2 add dir_0/file_000.txt` (1 modified file) | ~160.4 ms |
| `git2 commit -m "edit one"` | ~134.8 ms |
| `git2 log -n 10` | ~2.6 ms |
| `git2 reset --hard HEAD~1` | ~154.4 ms |
| `git2 undo` | ~201.7 ms |

Sidecar footprint on disk after the full sequence (`adopt` → `add` → `commit` → `reset --hard` → `undo`): **161,611 bytes** under `.git/git2/` (`16,075` bytes in `.git/objects/`), deduplicated with zlib-compressed CAS records. Note that `git2` reads eligible files into memory during capture and never prunes views silently; see the [runbook](runbook.md) for monitoring disk growth.

## 4. Reproducible Packaging & Release Gate

- Reproducible demo: `bash scripts/demo.sh`
- Local archive & checksum packaging: `bash scripts/package-release.sh` (emits `target/release-dist/git2-<version>-linux-<arch>.tar.gz` and `.sha256`, verified via `sha256sum -c`)
- **Publication gate:** `Cargo.toml` keeps `publish = false`, CI does not upload public artifacts or tags automatically, and any v0.1 release tag remains gated on separate maintainer approval.
