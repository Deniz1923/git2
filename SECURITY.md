# Security policy

## Project status

git2 is pre-release software and is not safe for important repositories. While `restore --view`, `recover`, `reset`, `undo`, and `add` are implemented, do not treat `.git/git2/` as a backup, and never test the tool first on the only copy of valuable data.

No version currently receives a production security-support guarantee.

## Sensitive issue classes

Please treat these as security-sensitive:

- writing outside the selected worktree;
- following a malicious symlink during capture or future restore;
- overwriting refs/index/worktree without a durable pre-operation view;
- accepting corrupt recovery content as valid;
- exposing `.git/git2/` through ordinary Git refs or networking;
- command or filter injection;
- unsafe handling of crafted repository paths or object data;
- unbounded allocation from recovery records;
- silent loss/pruning of reachable recovery history.

## Reporting

This checkout does not declare a dedicated private reporting address. If the project is hosted on a platform with private security advisories, use that facility. Otherwise contact the maintainer through a private channel before filing a public issue.

Do not include real credentials, private repository contents, or identifying paths in a report. A minimal disposable repository or byte fixture is preferred.

If no private channel is available and public disclosure is necessary, provide a redacted description first and withhold exploit details until maintainers can respond.

## Safe evaluation

- Use a disposable temporary repository.
- Keep a separate backup outside the repository.
- Review the [compatibility matrix](docs/compatibility.md).
- Do not raise file-size limits casually.
- Do not manually edit `.git/git2/` unless investigating format behavior on a copy.
- Remember that ignored files and Git configuration are not captured.

## Dependency and Git behavior

git2 parses repository-controlled data and may use `gix` behavior influenced by trusted Git configuration and attributes. Future subprocess calls must use argument vectors and must not interpolate shell input. Any feature that can execute configured Git filters requires explicit threat-model and differential tests.
