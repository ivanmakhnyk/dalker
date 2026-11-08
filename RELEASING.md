# Releasing Dalker

Dalker uses [semantic versions](https://semver.org) the GitFlow way: `develop` builds are
pre-releases of the next minor version (`0.3.0-dev.4`), a release drops the suffix (`0.3.0`), and
a hotfix bumps PATCH (`0.3.1`). The versions are set automatically (see below); only MAJOR is
changed by hand.

## Where the version lives

One place: `version` under `[workspace.package]` in `Cargo.toml`. Everything else follows it:

| Uses the version | How |
|---|---|
| all crates | `version.workspace = true` |
| `dalkerd --version`, `/api/info` | `CARGO_PKG_VERSION` at build time |
| web client (connection details → Version) | reads `/api/info` |
| `.deb` package | `build-deb.sh` (`1.2.0-rc.1` becomes `1.2.0~rc.1`, which Debian sorts before `1.2.0`) |
| MSI `ProductVersion` | `build-msi.ps1` (numeric part only, so `1.2.0-rc.1` installs as `1.2.0`) |
| release tag | must be `v` + the version, or the release workflow stops |

`scripts/version.sh check` (run by CI) fails if a crate or `Cargo.lock` disagrees.

## Branches and automatic versions

- `develop` is the default branch. Every fix and feature is a pull request into it, from a branch
  named `{feature|bug|task}/{issue#}-{short-description}` (for example `bug/1-windows-set-password-input`).
- `main` only receives `develop` at a release (or a hotfix), and every commit on it is a released
  version.

The **Version** workflow (`.github/workflows/version.yml`) sets the version, so pull requests
never change it by hand:

| Event | Version change | Also |
|---|---|---|
| a pull request is merged into `develop` | `-dev` number + 1: `0.3.0-dev.4` → `0.3.0-dev.5` (the first one after a release: `0.2.0` → `0.3.0-dev.1`) | commit `Version …` on `develop` |
| `develop` is merged into `main` (a release) | suffix dropped: `0.3.0-dev.5` → `0.3.0` | commit `Release 0.3.0` on `main`, tag `v0.3.0`, Release workflow started for the tag, `main` merged back into `develop` (which takes `0.3.0`) |
| anything else reaches `main` (a hotfix) | PATCH + 1: `0.3.0` → `0.3.1` | the same as a release, but `develop` keeps its own `-dev` version |

It pushes with the workflow's own token. If you protect `develop` or `main` with rules that
block direct pushes, allow GitHub Actions to bypass them, or the bump commits fail.

## Cutting a release

1. Make sure `develop` is green in GitHub Actions.
2. Open a pull request from `develop` into `main` and merge it with **Create a merge commit**
   (not squash), so the two branches keep a shared history.
3. The **Version** workflow drops the `-dev` suffix, tags the release and starts the **Release** workflow,
   which builds on Ubuntu and Windows and publishes the release with notes generated from the
   merged pull requests and commits since the previous tag. Assets:
   - `Dalker-0.3.0-x64.msi` and `dalker-agent-windows-x86_64.zip`
   - `dalker_0.3.0_amd64.deb` and `dalker-agent-linux-x86_64.tar.gz`
   - `SHA256SUMS.txt`

For a MAJOR bump, run `sh scripts/version.sh set X.0.0-dev.0` in a pull request into `develop`
(on Windows, from Git Bash, or edit the version in `Cargo.toml` and run `cargo update -w`); the
merge makes it `X.0.0-dev.1`, and the next release `X.0.0`. The automatic bump never changes
MAJOR.

### Hotfixes

For a fix that can't wait for the next release, branch `hotfix/{issue#}-{short-description}` from
`main` and open the pull request into `main`. Merging it publishes the patch release and brings
the fix into `develop`.

## Trying the packaging without releasing

Actions → **Release** → **Run workflow** builds every package from the chosen branch and attaches
them to the run as artifacts; nothing is published.

## If a release goes wrong

Delete the GitHub Release and the tag, fix the problem, and tag again (or re-run the Release
workflow for the tag):

```bash
gh release delete v0.2.0 --cleanup-tag   # or delete both in the GitHub UI
```

Prefer a new patch version once anyone may have downloaded the broken one.
