# Contributing to Dalker

Contributions are welcome: bug reports, fixes, features, documentation and translations.

## Before you start

1. Open an issue to discuss larger changes before you start. Every pull request refers to an
   issue, so open one for smaller changes too.
2. Fork, create a branch from `develop` named as below, and keep commits focused.
3. Run `cargo fmt --all`, `cargo clippy --workspace` and `cargo test --workspace` before opening a
   pull request.
4. Describe what changed and how you tested it (which host OS and session type).

## Branches and pull requests

`develop` is the default branch: open every pull request into it. `main` only receives `develop`
at a release (see [RELEASING.md](RELEASING.md)).

Name the branch `{type}/{issue#}-{short-description}`, and give the pull request the same text as
its title:

| Type | For |
|---|---|
| `feature` | new functionality |
| `bug` | a fix for a bug |
| `task` | everything else: docs, build, CI, refactoring, dependencies |
| `hotfix` | an urgent fix branched from `main` and merged into `main` (maintainer only) |

`{issue#}` is the number of the issue it addresses, without `#`. `{short-description}` is a few
lowercase words joined by hyphens. Examples: `bug/1-windows-set-password-input`,
`task/43-automatic-versioning`.

Put `Fixes #N` (or `Closes #N`) in the pull request description. Don't change the version in a
pull request: it is set automatically when it is merged.

Build instructions, the architecture and the wire protocol are in
[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## License of contributions

Dalker is licensed under the [GNU Affero General Public License v3](LICENSE) (`AGPL-3.0-only`).
The copyright holder also sells [commercial licenses](COMMERCIAL.md) and may publish paid
editions of Dalker. That income is what keeps the project maintained.

To make this possible, every contribution must be covered by the
**[Dalker Contributor Agreement](CLA.md)**, which assigns copyright to the maintainer. In short:

- you transfer the copyright in your contribution to the maintainer, who can then distribute it
  under the AGPL **and** under any other terms, including commercial licenses, without asking you;
- you may still use your own contribution under the AGPL;
- you grant a patent license for your contribution;
- you confirm that the work is yours to give (check with your employer if needed);
- the maintainer promises that your contribution stays available under the AGPL in every public
  release that includes it.

You accept the agreement by ticking the box in the pull request template. Pull requests without
it cannot be merged. If you do not want to assign your copyright, you are still free to fork
Dalker and publish your changes under the AGPL.

## Third-party code

Do not copy code from other projects unless its license is compatible with both the AGPL-3.0 and
proprietary redistribution (for example MIT, BSD, Apache-2.0, ISC or zlib). Say in the pull
request where the code comes from. New dependencies must use such licenses too; GPL, LGPL, MPL
and similar copyleft dependencies need the maintainer's approval first.
