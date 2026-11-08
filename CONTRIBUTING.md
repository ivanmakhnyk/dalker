# Contributing to Dalker

Contributions are welcome: bug reports, fixes, features, documentation and translations.

## Before you start

1. Open an issue to discuss larger changes before you start. Every pull request refers to an
   issue, so open one for smaller changes too.
2. Fork, create a branch from `develop` named as below, and keep commits focused.
3. Run `cargo fmt --all`, `cargo clippy --workspace --all-targets` and `cargo test --workspace`
   before opening a pull request.
4. Describe what changed and how you tested it (which host OS and session type).

## Branches

`develop` is the default branch: open every pull request into it. `main` only receives `develop`
at a release (see [RELEASING.md](RELEASING.md)).

Name the branch `{type}/{issue#}-{short-description}`:

| Type | For | Label it gets |
|---|---|---|
| `feature` | new functionality | `enhancement` |
| `bug` | a fix for a bug | `bug` |
| `task` | everything else: docs, build, CI, refactoring, dependencies | `task` |
| `hotfix` | an urgent fix branched from `main` and merged into `main` (maintainer only) | `bug` |

`{issue#}` is the number of the issue it addresses, without `#`. `{short-description}` is two to
five lowercase words joined by hyphens, not the whole issue title. Examples:
`bug/10-pointer-screen-edge`, `task/43-automatic-versioning`. Use one branch per issue; GitHub's
"Create a branch" link on an issue gives a name without the type, so rename it there.

## Commit messages

Write the subject in the imperative, in sentence case, with no period at the end and at most 72
characters: `Tell the viewer why the session ended`. If it helps, add a blank line and a body that
says why, wrapped at 72 characters. Pull requests are squashed when they are merged, so the commits
on your branch don't need to be polished; the pull request title is what lands on `develop`.

## Pull requests

- **Title:** the change in plain English, in the same style as a commit subject, for example
  `Let the pointer reach the screen edge on GNOME`. Not the branch name: the title becomes the
  commit on `develop` and the line in the release notes.
- **Description:** fill in the template. Start with `Fixes #N` (or `Closes #N`; `Refs #N` for
  partial work), say what changed, and how you tested it, including what you could not test.
- **Version:** don't change it. It is set automatically when the pull request is merged.
- **Checks:** a pull request can be merged when these pass:

  | Check | What it runs |
  |---|---|
  | `fmt` | `cargo fmt --all --check` |
  | `licenses` | `cargo deny check licenses`: every dependency's license must be allowed in `deny.toml` |
  | `Linux`, `Windows` | the version check, `cargo build`, `cargo clippy` and `cargo test` for the workspace |
  | `pr-rules` | the branch name, title, `Fixes #N` line and Contributor Agreement box; it also adds the label |

- **Merging:** pull requests into `develop` are squash-merged by the maintainer. A release
  (`develop` into `main`) is merged with a merge commit.

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
