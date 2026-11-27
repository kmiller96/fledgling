# Releases

CI checks pull requests and the default branch. Each passing push to the
default branch also produces a Linux binary in that run's artifacts; CI does
not publish anything.

To release, open **Actions → Release → Run workflow**, leave the default branch
selected, and choose `patch`, `minor`, or `major`. The workflow updates
`Cargo.toml` and `Cargo.lock`, validates the release commit, pushes it with its
`vX.Y.Z` tag, then publishes the crate to crates.io. It also builds Linux
x86_64, macOS Intel and Apple Silicon, and Windows x86_64 binaries, attaching
archives to a GitHub Release for the new tag. The release runs the same checks
as CI. GitHub does not start another push workflow for the automation's commit.

## One-time setup

- Create a GitHub environment named `release`, restricted to the default branch.
- Configure [crates.io Trusted Publishing](https://crates.io/docs/trusted-publishing)
  for crate `fledgling`, repository `kmiller96/fledgling`, workflow
  `release.yml`, and environment `release`. No API token secret is needed.
- Allow the workflow to fast-forward the default branch and create release tags.
  Branch protection requiring a PR or separate status checks for every push
  must explicitly allow the release automation; the workflow does not bypass
  protection rules.

If publishing fails after the commit and tag are pushed, rerun the original
**Release** run to reuse the same version and tag. If the branch advanced before
the release commit was pushed, start a fresh run instead.
