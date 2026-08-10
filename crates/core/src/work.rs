//! The repository side of a ticket run: check before, verify after.
//!
//! Deliberately not part of [`crate::git`], which is the *editor's* git — change
//! bars and diff text for a file on screen. This module knows about working trees,
//! branches and pull requests, and exists to answer two questions:
//!
//! - **Is it safe to start?** ([`preflight`]) — because the run that follows pushes
//!   a branch and opens a pull request, and none of that is reversible by the app.
//! - **What actually happened?** ([`verify`]) — because the agent's own account of
//!   its work is not evidence. A run that says "done" having committed nothing is
//!   indistinguishable, from the app's side, from one that succeeded — unless the
//!   app looks.
//!
//! ## `gh` is not necessarily on our `PATH`
//!
//! This is the same trap `gui::pty` documents, and it bites here for the same
//! reason. A Dock-launched app inherits launchd's environment, whose `PATH` is
//! `/usr/bin:/bin:/usr/sbin:/sbin` — so `gh` (Homebrew, `/opt/homebrew/bin`) and
//! `claude` (usually `~/.local/bin`) are simply not findable, while the same binary
//! run from a terminal finds both. It is invisible in development and total in
//! production.
//!
//! So anything outside the base system runs through a **login shell**, which reads
//! `/etc/zprofile` (and therefore `path_helper`) and `~/.zprofile` (and therefore
//! `brew shellenv`) — exactly what the PTY does. `git` is called directly, as
//! [`crate::git`] already does: `/usr/bin/git` ships with the Xcode command line
//! tools and is on the minimal `PATH` regardless.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// What a finished run left behind, read from the repository rather than from
/// anything the agent said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub branch: String,
    /// The branch exists locally.
    pub branch_exists: bool,
    /// Commits on the branch that aren't on the base.
    pub commits: usize,
    /// A matching branch exists on the remote.
    pub pushed: bool,
    pub pr: Option<PullRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub url: String,
    pub number: u64,
    pub is_draft: bool,
}

impl Outcome {
    /// Did the run get all the way to a pull request?
    pub fn succeeded(&self) -> bool {
        self.pr.is_some()
    }

    /// One paragraph for the alert, describing the state that was actually found.
    ///
    /// Every failure case names **how far it got**, because that decides what the
    /// user does next: a branch with commits that wasn't pushed needs a `git push`,
    /// and a branch that was never created needs the whole thing running again.
    /// "The run failed" would leave them opening a terminal to find out which.
    pub fn describe(&self) -> String {
        let Some(pr) = &self.pr else {
            return match (self.branch_exists, self.commits, self.pushed) {
                (false, _, _) => format!(
                    "No pull request was opened, and the branch `{}` was never \
                     created. Nothing was changed.",
                    self.branch
                ),
                (true, 0, _) => format!(
                    "No pull request was opened. The branch `{}` exists but nothing \
                     was committed to it.",
                    self.branch
                ),
                (true, n, false) => format!(
                    "No pull request was opened. `{}` has {} and was never pushed.",
                    self.branch,
                    commits(n)
                ),
                (true, n, true) => format!(
                    "No pull request was opened, but `{}` has {} and was pushed — \
                     so `gh pr create` is all that's missing.",
                    self.branch,
                    commits(n)
                ),
            };
        };
        format!(
            "{} on `{}`, pushed.\n\n{} pull request #{}:\n{}",
            commits(self.commits),
            self.branch,
            if pr.is_draft { "Draft" } else { "Open" },
            pr.number,
            pr.url
        )
    }
}

fn commits(n: usize) -> String {
    if n == 1 {
        "1 commit".to_string()
    } else {
        format!("{n} commits")
    }
}

/// Refuse to start a run that can't succeed, or that would damage something.
///
/// Ordered cheapest and most-likely-wrong first, and it reports only the first
/// failure: a list of five problems is harder to act on than the one thing to fix.
///
/// Each check is here because of what happens without it:
///
/// - **A dirty tree** would be swept into the agent's commits. Someone's unrelated
///   work in progress ends up in a pull request under a ticket number, which is
///   both confusing and genuinely hard to unpick once pushed.
/// - **An existing branch** means a previous run — finished or abandoned. Running
///   again would either fail at `git switch -c` or, worse, build on a state nobody
///   remembers.
/// - **`gh` unauthenticated** fails at the *last* step, after the work is done and
///   pushed. Ten minutes of agent time to discover a login prompt.
pub fn preflight(repo: &Path, branch: &str) -> Result<(), String> {
    if !repo.is_dir() {
        return Err(format!(
            "{} isn't a directory. Check the path in [jira.repos].",
            repo.display()
        ));
    }
    if git(repo, &["rev-parse", "--is-inside-work-tree"]).is_none() {
        return Err(format!("{} isn't a git repository.", repo.display()));
    }
    match git(repo, &["status", "--porcelain"]) {
        Some(status) if !status.trim().is_empty() => {
            let count = status.lines().count();
            return Err(format!(
                "{} has {} uncommitted change{}. Commit or stash first — an agent \
                 working here would sweep them into its own commits.",
                repo.display(),
                count,
                if count == 1 { "" } else { "s" }
            ));
        }
        None => return Err(format!("Couldn't read git status in {}.", repo.display())),
        _ => {}
    }
    // `--verify --quiet` exits non-zero when the ref is absent, which is the
    // answer we want, so `is_some` here means "it already exists".
    if git(repo, &["rev-parse", "--verify", "--quiet", branch]).is_some() {
        return Err(format!(
            "The branch `{branch}` already exists in {}. A previous run made it; \
             delete or rename it first.",
            repo.display()
        ));
    }
    if git(
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("origin/{branch}")],
    )
    .is_some()
    {
        return Err(format!(
            "The branch `{branch}` already exists on the remote — a previous run \
             pushed it. Check whether its pull request is still open."
        ));
    }
    // One login shell for all three, rather than three shell startups. See the
    // module docs for why these can't be looked up on our own `PATH`.
    let tools = login_shell(
        repo,
        "command -v claude >/dev/null 2>&1 || { echo no-claude; exit 0; }; \
         command -v gh >/dev/null 2>&1 || { echo no-gh; exit 0; }; \
         gh auth status >/dev/null 2>&1 || { echo no-auth; exit 0; }; \
         echo ok",
    )
    .unwrap_or_else(|| "no-shell".to_string());
    match tools.trim() {
        "ok" => Ok(()),
        "no-claude" => Err("`claude` isn't installed, or isn't on the PATH a login \
                            shell sees."
            .to_string()),
        "no-gh" => Err("`gh` isn't installed, or isn't on the PATH a login shell \
                        sees. The run needs it to open the pull request."
            .to_string()),
        "no-auth" => {
            Err("`gh` isn't authenticated. Run `gh auth login` first, or the run \
                 will do all the work and then fail to open the pull request."
                .to_string())
        }
        _ => Err("Couldn't run a login shell to check for `claude` and `gh`.".to_string()),
    }
}

/// Read what the run left in the repository.
///
/// Never fails: every question has a defensible negative answer, and "couldn't
/// tell" and "it didn't happen" lead to the same next step for the user. An
/// `Outcome` full of `false` describes a run that did nothing, which is exactly
/// what the user needs to hear.
pub fn verify(repo: &Path, branch: &str, base: &str) -> Outcome {
    let branch_exists = git(repo, &["rev-parse", "--verify", "--quiet", branch]).is_some();
    let pushed = git(
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("origin/{branch}")],
    )
    .is_some();

    // Count against the remote base where there is one: the local `main` may be
    // days behind, which would report the whole gap as this run's work.
    let commits = if branch_exists {
        count_commits(repo, &format!("origin/{base}"), branch)
            .or_else(|| count_commits(repo, base, branch))
            .unwrap_or(0)
    } else {
        0
    };

    Outcome {
        branch: branch.to_string(),
        branch_exists,
        commits,
        pushed,
        pr: pull_request(repo, branch),
    }
}

fn count_commits(repo: &Path, base: &str, branch: &str) -> Option<usize> {
    let range = format!("{base}..{branch}");
    git(repo, &["rev-list", "--count", &range])?.trim().parse().ok()
}

/// The open pull request for this branch, if `gh` can see one.
///
/// Through a login shell, for the `PATH` reason in the module docs. The branch name
/// is interpolated into the script, which is safe because it comes from
/// [`crate::jira::branch_name`] — ASCII alphanumerics and dashes, nothing a shell
/// would look at twice.
fn pull_request(repo: &Path, branch: &str) -> Option<PullRequest> {
    let json = login_shell(
        repo,
        &format!("gh pr list --head {branch} --json url,number,isDraft 2>/dev/null"),
    )?;
    let prs: Vec<PullRequest> = serde_json::from_str(json.trim()).ok()?;
    prs.into_iter().next()
}

/// Run a git command in `repo`, returning its stdout when it succeeded.
///
/// `None` covers both "git failed" and "git isn't there", which callers treat
/// identically — same shape as [`crate::git`]'s helpers.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run a script through a **login** shell, so it sees the `PATH` a terminal would.
///
/// See the module docs: without the login shell this finds neither `gh` nor
/// `claude` when the app was started from the Dock.
fn login_shell(repo: &Path, script: &str) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let output = Command::new(shell)
        .args(["-l", "-c", script])
        .current_dir(repo)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(branch_exists: bool, commits: usize, pushed: bool) -> Outcome {
        Outcome {
            branch: "TFE-954-fix-it".to_string(),
            branch_exists,
            commits,
            pushed,
            pr: None,
        }
    }

    #[test]
    fn a_successful_outcome_leads_with_the_pull_request() {
        let mut done = outcome(true, 3, true);
        done.pr = Some(PullRequest {
            url: "https://github.com/acme/app/pull/12".to_string(),
            number: 12,
            is_draft: true,
        });
        assert!(done.succeeded());
        let text = done.describe();
        assert!(text.contains("Draft pull request #12"), "got: {text}");
        assert!(text.contains("https://github.com/acme/app/pull/12"));
        assert!(text.contains("3 commits"));
    }

    #[test]
    fn every_failure_says_how_far_it_got() {
        // Which of these it is decides what the user does next, so a generic
        // "the run failed" would send them to a terminal to find out.
        let nothing = outcome(false, 0, false).describe();
        assert!(nothing.contains("never created"), "got: {nothing}");

        let empty = outcome(true, 0, false).describe();
        assert!(empty.contains("nothing was committed"), "got: {empty}");

        let unpushed = outcome(true, 2, false).describe();
        assert!(unpushed.contains("never pushed"), "got: {unpushed}");
        assert!(unpushed.contains("2 commits"));

        let pushed = outcome(true, 1, true).describe();
        assert!(pushed.contains("gh pr create"), "got: {pushed}");
        // Singular, because "1 commits" in an alert reads as a bug in the app.
        assert!(pushed.contains("1 commit "), "got: {pushed}");
    }

    #[test]
    fn a_pull_request_deserializes_from_ghs_own_json() {
        // Pinned against `gh pr list --json url,number,isDraft` output: the
        // camelCase rename is the part that silently yields an empty list if wrong,
        // which would report a successful run as having opened no PR.
        let json = r#"[{"url":"https://github.com/acme/app/pull/7",
                        "number":7,"isDraft":true}]"#;
        let prs: Vec<PullRequest> = serde_json::from_str(json).expect("parses");
        assert_eq!(prs[0].number, 7);
        assert!(prs[0].is_draft);
    }

    #[test]
    fn preflight_refuses_a_path_that_is_not_a_repository() {
        let error = preflight(Path::new("/"), "TFE-1-x").expect_err("/ is not a repo");
        assert!(error.contains("git repository"), "got: {error}");

        let error = preflight(Path::new("/no/such/place"), "TFE-1-x")
            .expect_err("a missing directory is not a repo");
        assert!(error.contains("isn't a directory"), "got: {error}");
    }

    #[test]
    fn preflight_refuses_a_branch_that_already_exists() {
        let repo = TempRepo::new("branch-exists");
        // `main` stands in for a previous run's branch — the check is the same.
        let error = preflight(repo.path(), "main").expect_err("main already exists");
        assert!(error.contains("already exists"), "got: {error}");
    }

    #[test]
    fn preflight_refuses_a_dirty_tree() {
        // The expensive mistake this prevents: someone's unrelated work in
        // progress swept into an agent's commits, under a ticket number.
        let repo = TempRepo::new("dirty");
        std::fs::write(repo.path().join("scratch.txt"), "work in progress").unwrap();
        let error = preflight(repo.path(), "TFE-1-new").expect_err("the tree is dirty");
        assert!(error.contains("uncommitted"), "got: {error}");
    }

    #[test]
    fn verify_counts_what_the_branch_actually_holds() {
        // The whole point of `verify`: read the repository rather than believe the
        // agent. A branch with commits on it reports them; a branch that was never
        // created reports nothing, rather than failing.
        let repo = TempRepo::new("verify");
        repo.git(&["switch", "-c", "TFE-1-did-the-work"]);
        std::fs::write(repo.path().join("new.txt"), "done").unwrap();
        repo.git(&["add", "."]);
        repo.commit("TFE-1 do the work");

        let done = verify(repo.path(), "TFE-1-did-the-work", "main");
        assert!(done.branch_exists);
        assert_eq!(done.commits, 1);
        // No remote in a temp repo, so this is the honest answer.
        assert!(!done.pushed);
        assert!(!done.succeeded());
        assert!(done.describe().contains("never pushed"), "{}", done.describe());

        let nothing = verify(repo.path(), "TFE-2-never-started", "main");
        assert!(!nothing.branch_exists);
        assert_eq!(nothing.commits, 0);
        assert!(nothing.describe().contains("never created"));
    }

    /// A throwaway git repository with one commit on `main`.
    ///
    /// Built rather than pointing the tests at this repository, which was the first
    /// attempt and is wrong: `preflight` reports a dirty tree *before* it looks at
    /// the branch, so the branch test passed or failed depending on whether the
    /// person running it had edits open. A test whose result depends on the working
    /// tree it's run from isn't testing what it claims to.
    struct TempRepo(std::path::PathBuf);

    impl TempRepo {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sacrament-work-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            let repo = Self(dir);
            repo.git(&["init", "-b", "main"]);
            std::fs::write(repo.path().join("README"), "hello").unwrap();
            repo.git(&["add", "."]);
            repo.commit("first");
            repo
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn git(&self, args: &[&str]) {
            let ok = Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .output()
                .expect("git runs")
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        }

        /// Identity passed per-command, so the test never depends on — or touches —
        /// whatever the machine has configured globally.
        fn commit(&self, message: &str) {
            self.git(&[
                "-c",
                "user.name=sacrament tests",
                "-c",
                "user.email=tests@example.invalid",
                "commit",
                "-m",
                message,
            ]);
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
