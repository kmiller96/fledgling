# Fledgling

A Infrastructure-as-Code (IaC) approach for managing database schemas.

## Motivation

I have been using [sqldef](https://github.com/sqldef/sqldef) and (goose)[https://github.com/pressly/goose] to manage my database for a project I'm working on. It was annoying me that sqldef could very easily get stuck and unable to apply migrations, so I decided to make it that I generate migrations regularly and then bring in Goose to manage these database migrations on my behalf. This seemed like a hacky approach, so I've instead opted for my own tool to manage this process.

## Quickstart

```sh
cargo install fledgling
```

## Releases

> [!NOTE]
> This section is AI-generated. It will require a review in the future.

CI checks pull requests and the default branch. Each passing push to the default
branch also produces a Linux binary in that run's artifacts. CI does not publish
anything.

In **Actions → Release → Run workflow**, leave the default branch selected and
choose `patch`, `minor`, or `major`. The workflow updates `Cargo.toml` and
`Cargo.lock`, validates the release commit, pushes it and its `vX.Y.Z` tag
together, then publishes the crate to crates.io. The release runs the same
checks as CI itself; GitHub does not start another push workflow for the bot's
commit. crates.io hosts the source package, not a prebuilt binary.

One-time setup:

- Create a GitHub environment named `release`. Restrict deployments to the
  default branch so other branches cannot use the publishing identity.
- In [crates.io Trusted Publishing](https://crates.io/docs/trusted-publishing),
  configure `fledgling` for `kmiller96/fledgling`, workflow `release.yml`, and
  environment `release`. No API token secret is needed.
- The workflow needs permission to fast-forward the default branch and create
  release tags. Rules requiring a PR or separate status checks for every push
  will block it unless they explicitly allow the release automation. The
  workflow does not bypass protection rules.

If publishing fails after the commit and tag were pushed, re-run the original
**Release** run. It reuses the same version and tag; don't start a new release
to retry. If the branch advanced before the release commit was pushed, start a
fresh run instead.
