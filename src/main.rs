use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use lpcli::{
    auth,
    client::{Collection, LaunchpadClient},
    error::LpError,
    git::{self, MergeProposal},
};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Merge a Launchpad proposal with an lpshipit-style message"
)]
struct Args {
    /// Local or remote-tracking branch to merge.
    source: String,

    /// Launchpad repository path or project name (defaults to the remote URL).
    #[arg(long)]
    repo: Option<String>,

    /// Git remote whose URL identifies the target repository.
    #[arg(long, default_value = "origin")]
    remote: String,

    /// Launchpad source branch name, if different from the local branch name.
    #[arg(long)]
    source_branch: Option<String>,

    /// Select a proposal by its numeric ID if multiple proposals match.
    #[arg(long)]
    proposal: Option<u64>,

    /// Use public Launchpad data without loading lpcli credentials.
    #[arg(long)]
    public: bool,

    /// Print the message without merging.
    #[arg(long)]
    message_only: bool,

    /// Open Git's editor to review the generated message.
    #[arg(long)]
    edit: bool,
}

#[derive(Debug, Deserialize)]
struct Vote {
    is_pending: bool,
    reviewer_link: Option<String>,
    comment_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Comment {
    vote: Option<String>,
}

#[derive(Debug)]
struct Candidate {
    proposal: MergeProposal,
    raw: Value,
}

fn git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("running git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn repository_path(input: &str) -> Result<String> {
    let path = if let Some(path) = input.strip_prefix("lp:") {
        path.to_owned()
    } else if input.contains("://") {
        let url = Url::parse(input).context("invalid repository URL")?;
        ensure!(
            matches!(
                url.host_str(),
                Some("git.launchpad.net" | "code.launchpad.net" | "launchpad.net")
            ),
            "not a Launchpad repository URL: {input}"
        );
        url.path().trim_start_matches('/').to_owned()
    } else if let Some((host, path)) = input.split_once(':') {
        let host = host.rsplit('@').next().unwrap_or(host);
        ensure!(
            host == "git.launchpad.net",
            "not a Launchpad repository URL: {input}"
        );
        path.trim_start_matches('/').to_owned()
    } else {
        input.trim_start_matches('/').to_owned()
    };
    let path = path.trim_end_matches('/');
    ensure!(
        !path.is_empty()
            && !path.contains('?')
            && !path.contains('#')
            && !path.split('/').any(|part| part == ".." || part == "."),
        "invalid Launchpad repository path"
    );
    Ok(path.to_owned())
}

fn source_branch(reference: &str) -> Result<&str> {
    if let Some(branch) = reference.strip_prefix("refs/heads/") {
        Ok(branch)
    } else if let Some(remote_ref) = reference.strip_prefix("refs/remotes/") {
        let (_, branch) = remote_ref
            .split_once('/')
            .context("invalid remote-tracking branch")?;
        Ok(branch)
    } else {
        bail!("source must resolve to a local or remote-tracking branch")
    }
}

fn api_link(link: &str) -> Result<&str> {
    let url = Url::parse(link).context("invalid Launchpad API link")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("api.launchpad.net")
            && url.port().is_none()
            && url.path().starts_with("/devel/"),
        "unexpected Launchpad API link: {link}"
    );
    Ok(link)
}

fn person_name(link: &str) -> Result<&str> {
    api_link(link)?;
    let name = link
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_start_matches('~');
    ensure!(!name.is_empty(), "missing Launchpad person name");
    Ok(name)
}

fn select_candidate(
    candidates: Vec<Candidate>,
    target_repo: &str,
    source: &str,
    target: &str,
    proposal_id: Option<u64>,
) -> Result<Candidate> {
    let matches = |item: &Candidate| {
        let p = &item.proposal;
        p.target_git_repository_link.as_deref() == Some(target_repo)
            && p.source_git_path
                .as_deref()
                .and_then(|p| p.strip_prefix("refs/heads/"))
                == Some(source)
            && p.target_git_path
                .as_deref()
                .and_then(|p| p.strip_prefix("refs/heads/"))
                == Some(target)
            && proposal_id
                .is_none_or(|id| p.self_link.as_deref().and_then(git::extract_mp_id) == Some(id))
    };
    let matching: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, item)| matches(item).then_some(index))
        .collect();
    if matching.is_empty() {
        let close_matches = candidates
            .iter()
            .filter(|item| {
                let p = &item.proposal;
                let source_matches =
                    p.source_git_path.as_deref() == Some(format!("refs/heads/{source}").as_str());
                let target_matches =
                    p.target_git_path.as_deref() == Some(format!("refs/heads/{target}").as_str());
                p.target_git_repository_link.as_deref() == Some(target_repo)
                    && (source_matches || target_matches)
            })
            .take(5)
            .map(|item| {
                let p = &item.proposal;
                format!(
                    "\n  {} -> {} ({})",
                    p.source_git_path.as_deref().unwrap_or("?"),
                    p.target_git_path.as_deref().unwrap_or("?"),
                    p.web_link.as_deref().unwrap_or("URL unavailable")
                )
            })
            .collect::<String>();
        let close_matches = if close_matches.is_empty() {
            "\n  none found".to_owned()
        } else {
            close_matches
        };
        bail!(
            "no matching Needs review or Approved proposal for {source} into {target}\n\
             Searched repository: {target_repo}\n\
             Proposals returned: {}\n\
             Close matches (up to 5):{close_matches}\n\
             Check the target repository (--repo or --remote), Launchpad source branch (--source-branch), proposal ID, and credentials.",
            candidates.len()
        );
    }
    if matching.len() > 1 {
        let choices = matching
            .iter()
            .map(|index| {
                candidates[*index]
                    .proposal
                    .web_link
                    .as_deref()
                    .unwrap_or("(missing proposal URL)")
            })
            .collect::<Vec<_>>()
            .join("\n  ");
        bail!("multiple proposals match; select one with --proposal ID:\n  {choices}");
    }
    Ok(candidates
        .into_iter()
        .nth(matching[0])
        .expect("matching index"))
}

fn build_message(
    source: &str,
    target: &str,
    author: &str,
    reviewers: &[String],
    body: &str,
    web: &str,
) -> String {
    format!(
        "Merge {source} into {target} [a={author}] [r={}]\n\n{body}\n\nMP: {web}",
        reviewers.join(",")
    )
}

fn merge(
    source_ref: &str,
    message: &str,
    edit: bool,
    directory: Option<&std::path::Path>,
) -> Result<i32> {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    command.args(["merge", "--no-ff", "-m", message]);
    if edit {
        command.arg("--edit");
    }
    let status = command
        .arg("--")
        .arg(source_ref)
        .status()
        .context("running git merge")?;
    Ok(status.code().unwrap_or(1))
}

async fn reviewers(client: &LaunchpadClient, candidate: &Candidate) -> Result<Vec<String>> {
    let votes_url = candidate.raw["votes_collection_link"]
        .as_str()
        .context("proposal is missing its votes collection")?;
    let votes: Vec<Vote> = Collection::fetch_all(client, api_link(votes_url)?).await?;
    let mut reviewers = Vec::new();
    for vote in votes {
        if vote.is_pending {
            continue;
        }
        let comment_url = vote
            .comment_link
            .as_deref()
            .context("completed review has no comment")?;
        let comment: Comment = client.get_url(api_link(comment_url)?).await?;
        if comment.vote.as_deref() == Some("Approve") {
            let reviewer = vote
                .reviewer_link
                .as_deref()
                .context("approved review has no reviewer")?;
            reviewers.push(person_name(reviewer)?.to_owned());
        }
    }
    reviewers.sort();
    Ok(reviewers)
}

async fn verify_source(
    client: &LaunchpadClient,
    proposal: &MergeProposal,
    local_commit: &str,
) -> Result<()> {
    let repository = proposal
        .source_git_repository_link
        .as_deref()
        .context("proposal has no source repository")?;
    let path = proposal
        .source_git_path
        .as_deref()
        .context("proposal has no source branch")?;
    let mut url = Url::parse(api_link(repository)?)?;
    url.query_pairs_mut()
        .append_pair("ws.op", "getRefByPath")
        .append_pair("path", path);
    let remote_ref: git::GitRef = client
        .get_url(url.as_str())
        .await
        .context("looking up the proposal's source branch")?;
    let remote_commit = remote_ref
        .commit_sha1
        .as_deref()
        .context("proposal source branch has no commit")?;
    ensure!(
        local_commit == remote_commit,
        "local source commit {local_commit} does not match the proposal's Launchpad branch ({remote_commit}); fetch the correct branch before merging"
    );
    Ok(())
}

async fn run(args: Args) -> Result<i32> {
    ensure!(
        !args.source.starts_with('-'),
        "source cannot start with '-'"
    );
    let target = git_output(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("check out the target branch first")?;
    let source_ref = git_output(&[
        "rev-parse",
        "--symbolic-full-name",
        "--verify",
        &args.source,
    ])?;
    let source = args
        .source_branch
        .as_deref()
        .unwrap_or(source_branch(&source_ref)?);
    ensure!(!source.is_empty(), "source branch cannot be empty");
    let source_commit =
        git_output(&["rev-parse", "--verify", &format!("{source_ref}^{{commit}}")])?;
    let target_commit = git_output(&["rev-parse", "--verify", "HEAD^{commit}"])?;

    let repo_input = match &args.repo {
        Some(repo) => repo.clone(),
        None => git_output(&["remote", "get-url", &args.remote])
            .context("specify --repo or configure a Launchpad remote")?,
    };
    let repo_path = repository_path(&repo_input)?;
    let credentials = if args.public {
        None
    } else {
        match auth::load_credentials() {
            Ok(creds) => Some(creds),
            Err(LpError::NotAuthenticated) => {
                eprintln!(
                    "No lpcli credentials found; using public Launchpad data (private proposals may be omitted)."
                );
                None
            }
            Err(error) => return Err(error.into()),
        }
    };
    let client = LaunchpadClient::new(credentials);
    let repository = if repo_path.contains("/+git/") || repo_path.starts_with("+git/") {
        git::get_git_repository(&client, &repo_path).await?
    } else {
        git::get_default_git_repository(&client, &repo_path).await?
    };
    let target_repo = repository
        .self_link
        .as_deref()
        .context("repository has no API link")?;
    api_link(target_repo)?;
    let unique_name = repository
        .unique_name
        .as_deref()
        .context("repository has no unique name")?;
    let mut candidates = Vec::new();
    for status in ["Needs review", "Approved"] {
        let result = git::list_merge_proposals_raw(&client, unique_name, Some(status)).await?;
        let entries = result.raw["entries"]
            .as_array()
            .context("invalid proposal collection")?;
        ensure!(
            entries.len() == result.value.len(),
            "invalid proposal collection length"
        );
        candidates.extend(
            result
                .value
                .into_iter()
                .zip(entries.iter().cloned())
                .map(|(proposal, raw)| Candidate { proposal, raw }),
        );
    }
    let selected = select_candidate(candidates, target_repo, source, &target, args.proposal)?;
    let proposal = &selected.proposal;
    verify_source(&client, proposal, &source_commit).await?;
    let author = person_name(
        proposal
            .registrant_link
            .as_deref()
            .context("proposal has no registrant")?,
    )?;
    let web = proposal
        .web_link
        .as_deref()
        .context("proposal has no web URL")?;
    let body = proposal
        .commit_message
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(proposal.description.as_deref())
        .unwrap_or("");
    let reviewers = reviewers(&client, &selected).await?;
    let message = build_message(source, &target, author, &reviewers, body, web);

    if args.message_only {
        println!("{message}");
        return Ok(0);
    }
    ensure!(
        git_output(&["symbolic-ref", "--quiet", "--short", "HEAD"])? == target,
        "target branch changed while looking up the proposal"
    );
    ensure!(
        git_output(&["rev-parse", "--verify", "HEAD^{commit}"])? == target_commit,
        "target branch changed while looking up the proposal"
    );
    ensure!(
        git_output(&["rev-parse", "--verify", &format!("{source_ref}^{{commit}}")])?
            == source_commit,
        "source branch changed while looking up the proposal"
    );
    eprintln!("Merging proposal: {web}");
    merge(&source_ref, &message, args.edit, None)
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("git-lp-merge: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(id: u64, source: &str, target_repo: &str) -> Candidate {
        let raw = json!({
            "self_link": format!("https://api.launchpad.net/devel/~alice/proj/+git/proj/+merge/{id}"),
            "web_link": format!("https://code.launchpad.net/~alice/proj/+git/proj/+merge/{id}"),
            "source_git_path": format!("refs/heads/{source}"),
            "target_git_path": "refs/heads/main",
            "target_git_repository_link": target_repo
        });
        Candidate {
            proposal: serde_json::from_value(raw.clone()).unwrap(),
            raw,
        }
    }

    #[test]
    fn chooses_only_matching_target_and_source_and_rejects_ambiguity() {
        let repo = "https://api.launchpad.net/devel/~team/proj/+git/proj";
        let choices = vec![
            candidate(1, "feature", repo),
            candidate(2, "other", repo),
            candidate(3, "feature", "other"),
        ];
        let chosen = select_candidate(choices, repo, "feature", "main", None).unwrap();
        assert!(chosen.proposal.self_link.unwrap().ends_with("/1"));

        let choices = vec![candidate(1, "feature", repo), candidate(2, "feature", repo)];
        assert!(
            select_candidate(choices, repo, "feature", "main", None)
                .unwrap_err()
                .to_string()
                .contains("multiple proposals")
        );
        let choices = vec![candidate(1, "feature", repo), candidate(2, "feature", repo)];
        let chosen = select_candidate(choices, repo, "feature", "main", Some(2)).unwrap();
        assert!(chosen.proposal.self_link.unwrap().ends_with("/2"));
    }

    #[test]
    fn no_match_reports_searched_repository_and_nearby_proposals() {
        let repo = "https://api.launchpad.net/devel/~team/proj/+git/proj";
        let choices = vec![
            candidate(1, "launchpad-name", repo),
            candidate(2, "feature", "other"),
        ];
        let error = select_candidate(choices, repo, "local-name", "main", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains(repo));
        assert!(error.contains("Proposals returned: 2"));
        assert!(error.contains("refs/heads/launchpad-name -> refs/heads/main"));
        assert!(error.contains("/+merge/1"));
        assert!(!error.contains("/+merge/2"));
        assert!(error.contains("--source-branch"));
    }

    #[test]
    fn formats_the_lpshipit_message() {
        assert_eq!(
            build_message(
                "feature",
                "main",
                "alice",
                &["bob".into(), "zoe".into()],
                "One\n\nTwo",
                "https://code.launchpad.net/mp"
            ),
            "Merge feature into main [a=alice] [r=bob,zoe]\n\nOne\n\nTwo\n\nMP: https://code.launchpad.net/mp"
        );
    }

    #[test]
    fn understands_launchpad_remotes_and_branch_refs() {
        for remote in [
            "lp:~team/project/+git/repo",
            "https://git.launchpad.net/~team/project/+git/repo",
            "git+ssh://git.launchpad.net/~team/project/+git/repo",
            "git@git.launchpad.net:~team/project/+git/repo",
        ] {
            assert_eq!(repository_path(remote).unwrap(), "~team/project/+git/repo");
        }
        assert!(repository_path("https://example.com/repo").is_err());
        assert!(repository_path("lp:../repo").is_err());
        assert_eq!(
            source_branch("refs/remotes/origin/feat/one").unwrap(),
            "feat/one"
        );
        assert!(source_branch("refs/tags/feature").is_err());
    }

    #[test]
    fn rejects_untrusted_api_links() {
        assert!(api_link("https://api.launchpad.net/devel/~alice").is_ok());
        assert!(api_link("https://api.launchpad.net.evil.example/devel/~alice").is_err());
        assert!(api_link("http://api.launchpad.net/devel/~alice").is_err());
    }

    #[test]
    fn creates_a_merge_commit_with_the_generated_message() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.hooksPath", "/dev/null"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["switch", "-q", "-c", "feature"]);
        git(&["commit", "-q", "--allow-empty", "-m", "feature"]);
        git(&["switch", "-q", "main"]);
        let message = build_message(
            "feature",
            "main",
            "alice",
            &["bob".into()],
            "Description",
            "https://example.com/mp",
        );
        assert_eq!(
            merge("refs/heads/feature", &message, false, Some(dir.path())).unwrap(),
            0
        );
        assert_eq!(git(&["log", "-1", "--format=%B"]).trim(), message);
        assert_eq!(
            git(&["rev-list", "--parents", "-n", "1", "HEAD"])
                .split_whitespace()
                .count(),
            3
        );
    }
}
