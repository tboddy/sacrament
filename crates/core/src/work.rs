//! The repository side of a ticket run: check before, verify after.
//!
//! Deliberately not part of [`crate::git`], which is the *editor's* git — change
//! bars and diff text for a file on screen. This module knows about working trees,
//! branches and pull requests, and exists to answer two questions:
//!
//! - **Is it safe to start?** ([`preflight`]) — because the run that follows pushes
//!   a branch and opens a pull request, and none of that is reversible by the app.
//! - **Where does it work?** ([`add_worktree`] / [`remove_worktree`]) — a run gets a
//!   git worktree of its own, so it is independent of the checkout the user is
//!   working in.
//! - **What actually happened?** ([`verify`]) — because the agent's own account of
//!   its work is not evidence. A run that says "done" having committed nothing is
//!   indistinguishable, from the app's side, from one that succeeded — unless the
//!   app looks.
//!
//! ## The agent never works in the user's checkout
//!
//! An unattended agent and a person editing the same working tree cannot both have
//! it. The first version of this ran the agent *in* the configured repository, and
//! so [`preflight`] had to refuse whenever that tree was dirty — which is most of
//! the time, because the whole appeal of the button is starting a ticket as an
//! aside from whatever you are already doing. A run you have to stash your own work
//! for is a run you don't press.
//!
//! So each run gets its own worktree ([`add_worktree`]), branched from an
//! up-to-date base, and the agent's shell opens there. `git worktree` shares the
//! repository's object store and refs, which is what makes this cheap and what
//! keeps the rest of this module unchanged: [`verify`] still asks the *main*
//! repository about the branch, because there is only one set of refs.
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
//! So anything outside the base system runs through a **login, interactive** shell —
//! exactly what a PTY gets, which is the standard to match, because the run itself
//! happens in a shell tab. Login gets `/etc/zprofile` (and therefore `path_helper`)
//! and `~/.zprofile` (and therefore `brew shellenv`); interactive gets `~/.zshrc`,
//! which is where a great many people actually keep their `PATH` — see
//! [`login_shell`], where leaving `-i` out made the pre-flight report `claude` as
//! missing while `claude` ran fine in the pane beside it.
//!
//! `git` is called directly, as [`crate::git`] already does: `/usr/bin/git` ships
//! with the Xcode command line tools and is on the minimal `PATH` regardless.

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
/// - **An existing branch** means a previous run — finished or abandoned. Running
///   again would either fail at `git worktree add -b` or, worse, build on a state
///   nobody remembers.
/// - **A leftover worktree** is that same previous run's checkout, kept because it
///   still held uncommitted work. Reusing it would put a second agent in it.
/// - **An unresolvable base** means the branch would be cut from nothing. Better to
///   say `main` can't be found than to fail inside the agent's shell.
/// - **`gh` unauthenticated** fails at the *last* step, after the work is done and
///   pushed. Ten minutes of agent time to discover a login prompt.
///
/// **There is deliberately no dirty-tree check.** There was, and dropping it is the
/// reason the worktree exists: the agent works in a checkout of its own, so
/// whatever is uncommitted in the user's tree is untouched and irrelevant. See the
/// module docs.
pub fn preflight(repo: &Path, branch: &str, worktree: &Path, base: &str) -> Result<(), String> {
    if !repo.is_dir() {
        return Err(format!(
            "{} isn't a directory. Check the path in [jira.repos].",
            repo.display()
        ));
    }
    if git(repo, &["rev-parse", "--is-inside-work-tree"]).is_none() {
        return Err(format!("{} isn't a git repository.", repo.display()));
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
    if worktree.exists() {
        return Err(format!(
            "A previous run's worktree is still at {}. It was kept because it had \
             uncommitted changes in it. Look at it, then remove it with:\n\n\
             git -C {} worktree remove {}",
            worktree.display(),
            repo.display(),
            worktree.display()
        ));
    }
    if base_ref(repo, base).is_none() {
        return Err(format!(
            "Can't find `{base}` or `origin/{base}` in {}. A run branches from it, \
             so there's nothing to start from.",
            repo.display()
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
        // Both of these name the check, because the fix is nearly always to the
        // `PATH` rather than to the installation — and a shell tab is right there to
        // run it in.
        "no-claude" => Err("`claude` isn't on the PATH your shell sees. Check it in a \
                            shell tab with `command -v claude`; if that finds it, the \
                            line that sets the PATH is somewhere this app's shell \
                            doesn't read."
            .to_string()),
        "no-gh" => Err("`gh` isn't on the PATH your shell sees, and the run needs it \
                        to open the pull request. Check it in a shell tab with \
                        `command -v gh`."
            .to_string()),
        "no-auth" => {
            Err("`gh` isn't authenticated. Run `gh auth login` first, or the run \
                 will do all the work and then fail to open the pull request."
                .to_string())
        }
        _ => Err("Couldn't run a login shell to check for `claude` and `gh`.".to_string()),
    }
}

/// Create the run's own checkout: a git worktree at `worktree`, on a new `branch`
/// cut from an up-to-date `base`.
///
/// **Called after the confirm dialog, never before it.** A worktree is a branch and
/// a directory, so making one during [`preflight`] would leave both behind every
/// time the user pressed Cancel.
///
/// Blocking, and slow enough to matter: a fetch plus a checkout of a large
/// repository is seconds, so callers run it off the UI thread.
///
/// The fetch is only attempted when the base *is* a remote-tracking ref — a
/// repository with no `origin` would otherwise fail on `git fetch` and never get to
/// the part that would have worked. Where there is a remote the fetch is fatal on
/// purpose: branching from a stale `origin/main` produces a pull request full of
/// conflicts, which is a worse outcome than a run that refuses to start.
pub fn add_worktree(
    repo: &Path,
    branch: &str,
    worktree: &Path,
    base: &str,
) -> Result<(), String> {
    let base = base_ref(repo, base).ok_or_else(|| {
        format!("Can't find `{base}` or `origin/{base}` in {}.", repo.display())
    })?;
    if base.starts_with("origin/") {
        git_checked(repo, &["fetch", "origin"])
            .map_err(|e| format!("Couldn't fetch from origin, so `{base}` may be stale:\n\n{e}"))?;
    }
    // Clears records for worktrees whose directories someone deleted by hand. Those
    // stay registered otherwise, and `worktree add` refuses a path that is still
    // registered even when nothing is there. Best-effort: a repository with none to
    // prune reports nothing, and a failure here is not a reason to refuse the run.
    let _ = git(repo, &["worktree", "prune"]);
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    git_checked(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            &worktree.to_string_lossy(),
            &base,
        ],
    )
    .map_err(|e| format!("Couldn't create a worktree at {}:\n\n{e}", worktree.display()))?;
    Ok(())
}

/// Tidy a finished run's worktree away, and report whether it's gone.
///
/// **Never `--force`, and that single choice is the whole policy.** Plain
/// `git worktree remove` deletes the directory only when there is nothing in it to
/// lose, and refuses when the tree holds modified or untracked files — which is
/// exactly the split we want. A run that committed and pushed everything leaves a
/// clean tree and is swept up; a run that died holding uncommitted work keeps its
/// checkout, and the caller says where it is.
///
/// Verified rather than assumed: ignored files do **not** block it, so the `target/`
/// or `node_modules/` the agent's own test run built doesn't strand every worktree.
///
/// Removing the worktree never loses the commits — those are on the branch, in the
/// repository's shared object store, and the branch is left in place.
pub fn remove_worktree(repo: &Path, worktree: &Path) -> bool {
    if !worktree.exists() {
        return true;
    }
    let _ = git(repo, &["worktree", "remove", &worktree.to_string_lossy()]);
    let _ = git(repo, &["worktree", "prune"]);
    !worktree.exists()
}

/// The ref a run's branch is cut from: `origin/<base>` when the remote has it,
/// otherwise the local `<base>`.
///
/// Preferring the remote is what stops a run inheriting however far behind the
/// user's local `main` happens to be. The local fallback is for a repository with
/// no remote at all, where refusing would be pedantic.
fn base_ref(repo: &Path, base: &str) -> Option<String> {
    let remote = format!("origin/{base}");
    if git(repo, &["rev-parse", "--verify", "--quiet", &remote]).is_some() {
        return Some(remote);
    }
    git(repo, &["rev-parse", "--verify", "--quiet", base]).map(|_| base.to_string())
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

/// Run a git command for its effect, keeping git's own words when it fails.
///
/// The counterpart to [`git`], which answers questions and treats every failure as
/// "no". These commands *do* something, and when one doesn't the reason is the whole
/// message the user gets — "couldn't create a worktree" alone is not actionable,
/// while git's `fatal:` line usually is.
fn git_checked(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| format!("couldn't run git: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("git {} failed", args.join(" "))
    } else {
        stderr
    })
}

/// Printed by [`login_shell`] before the script it was given, so a `.zshrc` that
/// writes to stdout can't be read as the script's answer.
///
/// Needed because the shell is now interactive (see below) and therefore sources a
/// file most people have put something chatty in. Without it, one `echo` in a
/// startup file makes every check here report "couldn't run a login shell", and one
/// in front of `gh`'s JSON makes a real pull request invisible.
const OUTPUT_MARKER: &str = "--- sacrament ---";

/// Run a script through a **login, interactive** shell, so it sees the `PATH` the
/// run's own shell will.
///
/// See the module docs for why it's a login shell. **Interactive is the other half,
/// and it is not optional**: `zsh -l -c` sources `/etc/zprofile`, `~/.zprofile` and
/// `~/.zlogin` — but *not* `~/.zshrc`, which zsh reads only for interactive shells.
/// Plenty of people, including this app's author, keep their whole `PATH` in
/// `.zshrc`, so a login-but-not-interactive shell finds neither `claude`
/// (`~/.local/bin`) nor `gh` (`/opt/homebrew/bin`), and the pre-flight refuses to
/// start a run that would have worked perfectly.
///
/// That failure is worth recognising, because it accuses the wrong thing: it says
/// `claude` isn't installed while `claude` runs fine in the shell pane beside it —
/// and that pane is the proof, because a PTY shell is login *and* interactive. The
/// check was stricter than the thing it was checking. Reproduce it without a Dock
/// launch by emptying the environment:
///
/// ```text
/// env -i HOME=$HOME PATH=/usr/bin:/bin /bin/zsh -l    -c 'command -v claude'  # nothing
/// env -i HOME=$HOME PATH=/usr/bin:/bin /bin/zsh -l -i -c 'command -v claude'  # found
/// ```
///
/// Measured at ~0.3s on this machine, with no tty and no `TERM` — which is what a
/// Dock-launched app has. Every caller is already on a background thread.
fn login_shell(repo: &Path, script: &str) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let output = Command::new(shell)
        .args([
            "-l",
            "-i",
            "-c",
            &format!("printf '%s\\n' '{OUTPUT_MARKER}'; {script}"),
        ])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let out = String::from_utf8_lossy(&output.stdout);
    after_marker(&out).map(str::to_string)
}

/// The part of a shell's stdout that belongs to the script rather than to whatever
/// its startup files printed.
///
/// Split out to be testable: the interesting case is noise *before* the marker, and
/// arranging for a real shell to produce some would mean writing dotfiles into a
/// fake `HOME` and setting `SHELL` process-wide, which is both fiddly and flaky
/// under a threaded test runner.
///
/// `rsplit_once`, so the later occurrence wins if a startup file somehow prints the
/// marker itself.
fn after_marker(stdout: &str) -> Option<&str> {
    Some(stdout.rsplit_once(OUTPUT_MARKER)?.1)
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
    fn a_chatty_startup_file_is_not_mistaken_for_the_answer() {
        // The shell is interactive, so it sources `.zshrc` — and a `.zshrc` that
        // prints something is ordinary. Without the marker that `echo` *is* the
        // output: the tool check would read "nvm loaded" and report that a login
        // shell couldn't be run, and `gh`'s JSON would fail to parse, hiding a real
        // pull request behind "no PR was opened".
        let noisy = format!("nvm loaded\nplugins ready\n{OUTPUT_MARKER}\nok\n");
        assert_eq!(after_marker(&noisy).map(str::trim), Some("ok"));

        // Multi-line output survives whole — `gh --json` is one line today, but the
        // marker must not turn into a line-picking rule that breaks if it stops being.
        let json = format!("{OUTPUT_MARKER}\n[\n  {{\"number\": 7}}\n]\n");
        assert_eq!(after_marker(&json).map(str::trim), Some("[\n  {\"number\": 7}\n]"));

        // No marker means the shell died before our script ran, which is not the
        // same as an empty answer — and must not be reported as one.
        assert_eq!(after_marker("command not found\n"), None);
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
        let tree = Path::new("/no/such/worktree");
        let error =
            preflight(Path::new("/"), "TFE-1-x", tree, "main").expect_err("/ is not a repo");
        assert!(error.contains("git repository"), "got: {error}");

        let error = preflight(Path::new("/no/such/place"), "TFE-1-x", tree, "main")
            .expect_err("a missing directory is not a repo");
        assert!(error.contains("isn't a directory"), "got: {error}");
    }

    #[test]
    fn preflight_refuses_a_branch_that_already_exists() {
        let repo = TempRepo::new("branch-exists");
        // `main` stands in for a previous run's branch — the check is the same.
        let error = preflight(repo.path(), "main", &repo.worktree("main"), "main")
            .expect_err("main already exists");
        assert!(error.contains("already exists"), "got: {error}");
    }

    #[test]
    fn preflight_allows_a_dirty_tree() {
        // The check that used to be here is gone, and its absence is the feature:
        // the agent works in a worktree of its own, so uncommitted work in the
        // user's checkout is untouched — and having to stash before starting a
        // ticket as an aside is what made the button not worth pressing.
        let repo = TempRepo::new("dirty-is-fine");
        std::fs::write(repo.path().join("scratch.txt"), "work in progress").unwrap();
        let tree = repo.worktree("TFE-1-new");
        // Everything before the tool check has to pass; whether `claude` and `gh`
        // are installed is not this test's business, so a failure here must not be
        // about the tree.
        if let Err(error) = preflight(repo.path(), "TFE-1-new", &tree, "main") {
            assert!(
                !error.contains("uncommitted"),
                "a dirty tree must not refuse a run: {error}"
            );
        }
    }

    #[test]
    fn preflight_refuses_a_leftover_worktree_and_says_how_to_remove_it() {
        // Reachable whenever a previous run kept its checkout because it held
        // uncommitted work, and then someone deleted the branch by hand. The
        // message has to carry the path and the command — a refusal naming neither
        // reads as the button being broken.
        let repo = TempRepo::new("leftover");
        let tree = repo.worktree("TFE-1-again");
        std::fs::create_dir_all(&tree).unwrap();
        let error = preflight(repo.path(), "TFE-1-again", &tree, "main")
            .expect_err("the worktree is still there");
        assert!(error.contains("worktree remove"), "got: {error}");
        assert!(error.contains(&tree.display().to_string()), "got: {error}");
    }

    #[test]
    fn preflight_refuses_a_base_it_cannot_find() {
        let repo = TempRepo::new("no-base");
        let error = preflight(
            repo.path(),
            "TFE-1-x",
            &repo.worktree("TFE-1-x"),
            "no-such-base",
        )
        .expect_err("there is no such base");
        assert!(error.contains("no-such-base"), "got: {error}");
    }

    #[test]
    fn a_worktree_is_independent_of_a_dirty_main_checkout() {
        // The whole point of the change, asserted end to end against real git: the
        // run gets its own checkout on its own branch, and the edit in progress in
        // the user's tree is neither swept up nor disturbed.
        let repo = TempRepo::new("independent");
        std::fs::write(repo.path().join("README"), "hello\nedited by the user\n").unwrap();

        let tree = repo.worktree("TFE-1-aside");
        add_worktree(repo.path(), "TFE-1-aside", &tree, "main").expect("worktree is created");

        assert!(tree.join("README").is_file(), "the worktree is a real checkout");
        assert_eq!(
            std::fs::read_to_string(tree.join("README")).unwrap(),
            "hello",
            "the worktree holds the committed content, not the user's edit"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("README")).unwrap(),
            "hello\nedited by the user\n",
            "the user's uncommitted edit survives untouched"
        );
        // Two checkouts, one repository — and the user is still on `main`.
        assert_eq!(repo.head_branch(repo.path()), "main");
        assert_eq!(repo.head_branch(&tree), "TFE-1-aside");
    }

    #[test]
    fn a_clean_worktree_is_swept_up_and_a_dirty_one_keeps_itself() {
        // One rule, expressed by never passing `--force`: git refuses to remove a
        // tree with anything in it to lose. A run that pushed everything leaves
        // nothing behind; a run that died holding work keeps its checkout so
        // someone can look at it.
        let repo = TempRepo::new("sweep");

        let clean = repo.worktree("TFE-1-clean");
        add_worktree(repo.path(), "TFE-1-clean", &clean, "main").expect("created");
        assert!(remove_worktree(repo.path(), &clean), "a clean tree is removed");
        assert!(!clean.exists());
        // The commits live on the branch, so removing the checkout never loses work.
        assert!(
            git(repo.path(), &["rev-parse", "--verify", "--quiet", "TFE-1-clean"]).is_some(),
            "the branch outlives its worktree"
        );

        let dirty = repo.worktree("TFE-2-dirty");
        add_worktree(repo.path(), "TFE-2-dirty", &dirty, "main").expect("created");
        std::fs::write(dirty.join("half-done.txt"), "the agent was mid-thought").unwrap();
        assert!(!remove_worktree(repo.path(), &dirty), "a dirty tree is kept");
        assert!(dirty.join("half-done.txt").is_file(), "and keeps its contents");

        // Ignored build output must not count as work worth keeping, or a Rust
        // repository strands a worktree on every single run.
        let built = repo.worktree("TFE-3-built");
        add_worktree(repo.path(), "TFE-3-built", &built, "main").expect("created");
        std::fs::create_dir_all(built.join("target")).unwrap();
        std::fs::write(built.join("target/artifact"), "compiled").unwrap();
        assert!(
            remove_worktree(repo.path(), &built),
            "ignored files are not uncommitted work"
        );

        // Removing something already gone is a success, not an error — the caller
        // asks for the tree to be absent, not for a removal to have happened.
        assert!(remove_worktree(repo.path(), &built));
    }

    #[test]
    fn a_worktree_wiped_by_hand_does_not_block_the_next_run() {
        // Deleting the directory leaves git's record of it behind, and git refuses
        // to add at a path that is "a missing but already registered worktree" —
        // its own message names `prune` as the fix. `add_worktree` prunes first for
        // exactly this, and without it a hand-deleted worktree poisons that path
        // for good.
        let repo = TempRepo::new("pruned");
        let tree = repo.worktree("TFE-1-x");
        add_worktree(repo.path(), "TFE-1-x", &tree, "main").expect("created");
        std::fs::remove_dir_all(&tree).unwrap();

        // A second attempt at the same ticket is a different branch at the same
        // path, which is the shape this actually has in the app.
        add_worktree(repo.path(), "TFE-1-x-again", &tree, "main")
            .expect("the stale record was pruned");
        assert!(tree.join("README").is_file());
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

    /// A throwaway git repository with one commit on `main`, plus somewhere beside
    /// it to put worktrees.
    ///
    /// Built rather than pointing the tests at this repository, which was the first
    /// attempt and is wrong: a test that creates worktrees and branches must not do
    /// it in the tree it's being run from, and the old dirty-tree test passed or
    /// failed depending on whether the person running it had edits open. A test
    /// whose result depends on the working tree it runs in isn't testing what it
    /// claims to.
    ///
    /// The repository is a *subdirectory* of the temp dir, with worktrees as its
    /// siblings, so dropping this removes both — a worktree left outside would
    /// survive the repository that registered it and litter `/tmp`.
    struct TempRepo {
        base: std::path::PathBuf,
        repo: std::path::PathBuf,
    }

    impl TempRepo {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "sacrament-work-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            let repo = base.join("repo");
            std::fs::create_dir_all(&repo).expect("temp dir");
            let repo = Self { base, repo };
            repo.git(&["init", "-b", "main"]);
            std::fs::write(repo.path().join("README"), "hello").unwrap();
            // So the "ignored build output doesn't strand a worktree" case has
            // something to ignore.
            std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
            repo.git(&["add", "."]);
            repo.commit("first");
            repo
        }

        fn path(&self) -> &Path {
            &self.repo
        }

        /// Where a run's worktree goes: outside the repository, one directory per
        /// branch, mirroring `paths::worktree_dir`.
        fn worktree(&self, branch: &str) -> std::path::PathBuf {
            self.base.join("worktrees").join(branch)
        }

        fn head_branch(&self, at: &Path) -> String {
            git(at, &["rev-parse", "--abbrev-ref", "HEAD"])
                .expect("HEAD resolves")
                .trim()
                .to_string()
        }

        fn git(&self, args: &[&str]) {
            let ok = Command::new("git")
                .args(args)
                .current_dir(&self.repo)
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
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }
}
