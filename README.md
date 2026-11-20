# git-lp-merge

Merge a Launchpad Git merge proposal with the same commit-message convention as
`lpshipit`, using [lpcli as a Rust library](https://github.com/canonical/lpcli#using-lpcli-as-a-rust-library).

## Install

Install Rust with rustup, then run:

```sh
cargo install --locked --path .
```

Ensure Cargo's bin directory is on `PATH`. Git can then find `git-lp-merge` as
`git lp-merge`. The project pins the Rust toolchain required by lpcli.

## Usage

Check out the target branch, fetch the proposal's source branch, then run:

```sh
git lp-merge feature-branch
```

The target Launchpad repository is inferred from the `origin` remote. Use
`--remote upstream` for another remote, or `--repo '~team/project/+git/repo'`
to specify the Launchpad repository explicitly. A project name selects its
default Git repository.

The command finds a **Needs review** or **Approved** proposal matching the
target repository and both branch names. If several match, it lists them and
requires `--proposal ID`. If your local source branch has a different name
from the Launchpad branch, provide `--source-branch NAME`. The local source
commit must match the proposal's current Launchpad branch tip; fetch it first
if it is out of date.

Use `--message-only` to print the message without merging or `--edit` to review
it in Git's editor. The actual merge uses `git merge --no-ff`; if it conflicts,
resolve the conflicts and run `git merge --continue` as usual. This command
does not push changes.

The author is the proposal registrant; reviewers are sorted Launchpad usernames
with current, non-pending **Approve** votes. The body is the proposal's commit
message, falling back to its description. A missing approval is not treated as
an error, matching lpshipit's behavior.

By default, the tool uses credentials stored by `lpcli login`, or public data
if no credentials are available. Private proposals may be omitted in public
mode. Use `--public` to skip loading credentials explicitly. See
`git lp-merge --help` for all options.
