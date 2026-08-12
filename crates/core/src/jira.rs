//! Jira Cloud client — read-only for now.
//!
//! Framework-independent by the same rule as the rest of `core`: this module
//! knows about HTTP and JSON and nothing about how the result is drawn. The
//! dashboard is produced as **markdown** ([`to_markdown`]) precisely so the
//! frontend needs no new rendering code — v2 feeds it to the read-mode renderer
//! it already has. See `docs/jira-integration.md`.
//!
//! **Blocking on purpose.** A fetch is request/response, not a stream, so it
//! runs on a thread pool via the frontend's `Task::perform` rather than through
//! the channel-and-subscription apparatus `pty` and `watch` need. That also
//! keeps `tokio` out of the dependency tree.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// Non-secret Jira settings, from `[jira]` in `config.toml`.
///
/// The token is deliberately absent — it comes from the Keychain via
/// [`crate::secret`], because `config.toml` is plaintext and shared with v1.
#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct JiraConfig {
    /// Site host or bare name: `acme`, `acme.atlassian.net`, or
    /// `https://acme.atlassian.net` all work. See [`JiraConfig::base_url`].
    pub site: String,
    /// Atlassian account email — the username half of the Basic auth pair.
    pub email: String,
    /// The dashboard's query. Any valid JQL.
    pub query: String,
    /// Cap on issues fetched in one request. Jira also caps this server-side, so
    /// asking for more does not guarantee more.
    pub max_results: usize,
    /// Which working tree a ticket's code lives in, keyed by **project key** —
    /// the part of an issue key before the dash:
    ///
    /// ```toml
    /// [jira.repos]
    /// TFE = "~/code/truefire"
    /// ```
    ///
    /// A map rather than one directory because a dashboard built from
    /// `assignee = currentUser()` spans whatever projects you're on, and a run
    /// started in the wrong working tree is the expensive mistake here. A project
    /// with no entry simply offers no run — see [`JiraConfig::repo_for`].
    pub repos: BTreeMap<String, String>,
    /// Statuses a ticket must be in before the dashboard will offer to run it.
    ///
    /// **Configurable rather than fixed, because status names are per-project.**
    /// The three-bucket `statusCategory` that [`JiraConfig::default`]'s query uses
    /// is the same on every instance, but it is far too coarse for this: "To Do"
    /// covers a ticket nobody has specified yet as well as one that's ready to
    /// build, and the whole premise of an unattended run is a description good
    /// enough to work from. Only the workflow's own names can say that, and two
    /// projects on the same instance can disagree.
    ///
    /// An empty list offers no runs at all. To widen it, name the statuses — there
    /// is deliberately no "any status" setting, because that's the one value that
    /// turns the check off entirely and it should have to be spelled out.
    pub run_statuses: Vec<String>,
}

impl Default for JiraConfig {
    fn default() -> Self {
        Self {
            site: String::new(),
            email: String::new(),
            // `statusCategory != Done`, and specifically *not*
            // `resolution = Unresolved`.
            //
            // Unresolved is the intuitive choice and is wrong on any workflow
            // whose terminal statuses don't set a resolution — which is common,
            // because setting one is a per-transition configuration nobody
            // remembers. Measured on a real instance: it matched 50 issues of
            // which 35 were `Released` or `Rejected`, i.e. finished work.
            //
            // `statusCategory` is the fixed three-bucket grouping (To Do /
            // In Progress / Done) that every Jira has underneath whatever custom
            // status names a project invented, so it means "not finished"
            // regardless of workflow. Naming statuses directly would be worse
            // still: they're per-project.
            query: "assignee = currentUser() AND statusCategory != Done \
                    ORDER BY updated DESC"
                .to_string(),
            max_results: 50,
            repos: BTreeMap::new(),
            // The two statuses that mean "written down well enough to build".
            // Anything earlier isn't specified yet, and anything later is already
            // being worked on by someone.
            run_statuses: vec!["Specified".to_string(), "New".to_string()],
        }
    }
}

impl JiraConfig {
    /// Is there enough here to try a request?
    ///
    /// The token isn't checked — it lives elsewhere and is fetched separately, so
    /// "configured" here means the parts that live in `config.toml`.
    pub fn is_configured(&self) -> bool {
        !self.site.trim().is_empty() && !self.email.trim().is_empty()
    }

    /// The API root, accepting whichever form of the site the user pasted.
    ///
    /// People copy this from a browser bar, a colleague, or another tool's
    /// config, so all three spellings arrive in practice. Normalizing here means
    /// no call site has to care, and a trailing slash can't produce a `//` in the
    /// path — which some proxies answer with a redirect the client won't follow.
    pub fn base_url(&self) -> String {
        let site = self.site.trim().trim_end_matches('/');
        let site = site
            .strip_prefix("https://")
            .or_else(|| site.strip_prefix("http://"))
            .unwrap_or(site);
        if site.contains('.') {
            format!("https://{site}")
        } else {
            format!("https://{site}.atlassian.net")
        }
    }

    /// The working tree an issue's code lives in, or `None` when its project has
    /// no entry in `[jira.repos]`.
    ///
    /// `None` is a normal answer, not a failure: it means "this app has no idea
    /// where that project's code is", and the caller's job is then to offer no run
    /// at all. Guessing — the first configured repo, or the process cwd — would put
    /// an agent to work in the wrong tree, which is the one mistake here that costs
    /// real time to unpick.
    ///
    /// The directory is **not** checked for existence. That's `work::preflight`'s
    /// job, which reports *why* a configured repo can't be used; answering `None`
    /// for a typo'd path would silently hide the row's button instead.
    pub fn repo_for(&self, issue_key: &str) -> Option<PathBuf> {
        let dir = self.repos.get(project_key(issue_key))?;
        Some(expand_tilde(dir))
    }

    /// Is a ticket in this status ready to be handed to an agent?
    ///
    /// Compared case-insensitively and trimmed, because this is matched against a
    /// display string typed into a config file by hand — `specified` and
    /// `Specified` are the same intent, and failing on the difference would
    /// present as the button never appearing, with nothing on screen to explain
    /// why. Case folding is ASCII-only: a status name outside it will have been
    /// copied from Jira verbatim, where an exact match already works.
    pub fn runnable(&self, status: &str) -> bool {
        let status = status.trim();
        self.run_statuses
            .iter()
            .any(|allowed| allowed.trim().eq_ignore_ascii_case(status))
    }
}

/// The project half of an issue key: `TFE-954` is project `TFE`.
///
/// A key with no dash is returned whole rather than rejected — an instance using
/// unconventional keys should still be able to configure a repo for it.
pub fn project_key(issue_key: &str) -> &str {
    issue_key
        .split_once('-')
        .map(|(project, _)| project)
        .unwrap_or(issue_key)
}

/// Expand a leading `~`, which is what people write in a hand-edited config and
/// what nothing in `std` expands. Anything else is taken as-is.
fn expand_tilde(dir: &str) -> PathBuf {
    let dir = dir.trim();
    let Some(rest) = dir.strip_prefix('~') else {
        return PathBuf::from(dir);
    };
    // Only `~` and `~/…` — `~someone/…` is another user's home, which needs the
    // password database and which nobody writes in a config for their own repo.
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    match crate::paths::home_dir() {
        Some(home) if !rest.starts_with('~') => home.join(rest),
        _ => PathBuf::from(dir),
    }
}

/// Longest a generated branch name may be.
///
/// Git itself has no limit worth hitting, but a branch name is typed, pasted into
/// PR descriptions and shown in tab labels, and a 120-character one is unusable in
/// all three. Long enough to keep a real summary readable after the key.
const MAX_BRANCH_LEN: usize = 60;

/// The branch a run works on: the issue key, then a kebab of the summary.
///
/// **The app dictates this rather than letting the agent choose**, which is what
/// makes the run verifiable: the branch name is known before anything starts, so
/// `work::preflight` can refuse when it already exists and `work::verify` can find
/// the PR afterwards without trusting a word the agent said.
///
/// The output is always a valid ref: the key is kept verbatim (Jira keys are
/// alphanumeric and a dash), everything else collapses to single dashes, and
/// leading and trailing dashes are trimmed — so no `..`, no trailing `-`, and
/// never an empty name. A summary that contributes nothing (punctuation only, or
/// absent) leaves the key alone, which is still a usable branch.
pub fn branch_name(issue_key: &str, summary: &str) -> String {
    let key = issue_key.trim();
    let slug = kebab(summary);
    if slug.is_empty() {
        return key.to_string();
    }
    let room = MAX_BRANCH_LEN.saturating_sub(key.len() + 1);
    let mut name = format!("{key}-{}", truncate_on_dash(&slug, room));
    // Truncation can leave the name ending on the dash it cut at.
    while name.ends_with('-') {
        name.pop();
    }
    name
}

/// Lowercase, alphanumerics kept, everything else a single dash.
///
/// Deliberately ASCII-only in the output: a non-ASCII branch name is legal in git
/// and miserable everywhere else — it has to survive a shell, a URL and whatever
/// tools the repo's CI uses. A summary written entirely in another script therefore
/// contributes nothing and the branch is just the key, which is still correct.
fn kebab(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Cut to at most `max` characters, preferring the last whole word.
///
/// Cutting mid-word produces a branch like `…-authenti`, which reads as a typo
/// rather than as an abbreviation. Falls back to a hard cut when the first word is
/// itself longer than the budget.
fn truncate_on_dash(slug: &str, max: usize) -> &str {
    if slug.len() <= max || max == 0 {
        return &slug[..slug.len().min(max)];
    }
    match slug[..max].rfind('-') {
        Some(at) if at > 0 => &slug[..at],
        _ => &slug[..max],
    }
}

/// One issue, flattened out of Jira's nested `fields` object.
///
/// Only what the dashboard shows. Deliberately not a faithful mirror of the API
/// response — the wire types below are private for exactly that reason, so a
/// change in what Jira returns is absorbed in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub key: String,
    pub summary: String,
    pub status: String,
    pub kind: String,
    pub priority: Option<String>,
    pub assignee: Option<String>,
    /// Jira's timestamp, trimmed to the date. The time of day is noise on a
    /// dashboard and its format varies by instance locale.
    pub updated: Option<String>,
}

impl Issue {
    /// The human-facing URL for this issue on the given site.
    pub fn url(&self, base: &str) -> String {
        issue_url(base, &self.key)
    }
}

/// The human-facing URL for an issue key on the given site.
///
/// Split out from [`Issue::url`] because opening one in a browser starts from a
/// key alone — the frontend's "open this ticket" message carries the key rather
/// than a whole issue, since the row it came from may have been replaced by a
/// refresh before the message lands.
///
/// The key only ever reaches the *path*, so the scheme is whatever `base` had —
/// and [`JiraConfig::base_url`] only ever produces `https`.
pub fn issue_url(base: &str, key: &str) -> String {
    format!("{base}/browse/{key}")
}

/// A page of results, plus whether Jira had more to give.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub issues: Vec<Issue>,
    /// True when Jira reported further pages. Surfaced rather than silently
    /// dropped, because a dashboard that shows 50 of 120 issues without saying
    /// so is actively misleading.
    pub more: bool,
}

/// Fields requested from the API. Kept to what the dashboard renders — asking
/// for everything makes the response an order of magnitude larger for nothing,
/// and `description` in particular arrives as an ADF document tree.
const FIELDS: &str = "summary,status,issuetype,priority,assignee,updated";

/// Jira's current search endpoint.
///
/// The older `/rest/api/{2,3}/search` is deprecated in favour of this one. Held
/// as a constant because it is the single most likely thing to need changing
/// against an instance that disagrees, and because [`fetch`] reports the server's
/// own status and body on failure — so a mismatch says so instead of looking
/// like an auth problem.
const SEARCH_PATH: &str = "/rest/api/3/search/jql";

/// How long to wait on Jira before giving up.
///
/// Bounded so a hung or unreachable instance surfaces as an error the user can
/// act on rather than a dashboard that never arrives. Generous enough for a cold
/// instance and a slow JQL query.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Run the configured query.
///
/// Errors are strings because every one of them is going to a human in an alert;
/// there is no branch a caller would take on the variant. They carry the HTTP
/// status and the response body, which is what makes a misconfigured instance
/// diagnosable — Jira explains a bad JQL clause or a missing permission in the
/// body, and swallowing that leaves nothing to go on.
pub fn fetch(config: &JiraConfig, token: &str) -> Result<Page, String> {
    if !config.is_configured() {
        return Err("Jira is not configured: set `site` and `email` under [jira] \
                    in config.toml."
            .to_string());
    }
    let url = format!("{}{SEARCH_PATH}", config.base_url());
    let max = config.max_results.clamp(1, 100).to_string();

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();

    let mut response = agent
        .get(&url)
        .query("jql", config.query.trim())
        .query("fields", FIELDS)
        .query("maxResults", &max)
        .header("Authorization", &basic_auth(&config.email, token))
        .header("Accept", "application/json")
        .call()
        .map_err(describe)?;

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Could not read Jira's response: {e}"))?;

    parse(&body)
}

/// One issue in full, for handing to an agent as context.
///
/// Separate from [`Issue`] rather than a field on it: the dashboard fetches fifty
/// issues and needs none of this, and `description` is the single largest thing
/// the API will hand back. One ticket's body is fetched when a run is about to
/// start, and not before.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IssueDetail {
    pub key: String,
    pub summary: String,
    /// Wiki markup, or empty when the ticket has no description. Empty is worth
    /// carrying rather than refusing: plenty of real tickets say everything in the
    /// summary, and the confirm dialog is where a human decides whether that's
    /// enough to work from.
    pub description: String,
}

/// Where one issue's body is read from — **API v2, not v3**.
///
/// This is the gotcha that costs an afternoon if rediscovered. On v3 a description
/// arrives as an Atlassian Document Format tree — nested JSON that has to be walked
/// and rendered before it's readable. v2 returns the same content as wiki markup in
/// a plain string, which is exactly what an agent's prompt wants. The search
/// endpoint stays on v3 (see [`SEARCH_PATH`]) because it asks for no bodies.
const ISSUE_PATH_V2: &str = "/rest/api/2/issue";

/// Fetch one issue's summary and description.
pub fn fetch_issue(config: &JiraConfig, token: &str, key: &str) -> Result<IssueDetail, String> {
    if !config.is_configured() {
        return Err("Jira is not configured: set `site` and `email` under [jira] \
                    in config.toml."
            .to_string());
    }
    let url = format!("{}{ISSUE_PATH_V2}/{key}", config.base_url());

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();

    let mut response = agent
        .get(&url)
        .query("fields", "summary,description")
        .header("Authorization", &basic_auth(&config.email, token))
        .header("Accept", "application/json")
        .call()
        .map_err(describe)?;

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Could not read Jira's response: {e}"))?;

    parse_issue(&body)
}

/// Parse a single-issue response. Split out from [`fetch_issue`] for the same
/// reason [`parse`] is: it's the half that can be tested without a live instance.
pub fn parse_issue(body: &str) -> Result<IssueDetail, String> {
    // Checked first, for the reason given in `parse`: every wire field is
    // `#[serde(default)]`, so an error body deserializes happily into an empty
    // issue and the real message would be thrown away.
    if let Some(message) = error_messages(body) {
        return Err(message);
    }
    let issue: WireIssueDetail = serde_json::from_str(body)
        .map_err(|e| format!("Could not understand Jira's response: {e}"))?;
    Ok(IssueDetail {
        key: issue.key,
        summary: issue.fields.summary.unwrap_or_default().trim().to_string(),
        description: issue
            .fields
            .description
            .unwrap_or_default()
            .trim()
            .to_string(),
    })
}

/// Instructions for an unattended agent run on one ticket.
///
/// Built here rather than in the frontend because it's text derived from Jira data
/// with no UI in it, and because it's the part most worth testing: the prompt is
/// the entire specification handed to something that will push code.
///
/// Four properties it has to keep:
///
/// - **The branch and base are stated, not left to judgement.** The app has already
///   committed to them — `preflight` refused if the branch existed and `verify`
///   will look for a PR on exactly this branch. An agent that picks its own name
///   leaves the app unable to check its work.
/// - **The branch already exists, and the prompt says so.** The run starts in a git
///   worktree the app created ([`crate::work::add_worktree`]), already checked out
///   on `branch`. This used to instruct a `git switch -c`, which now fails —
///   telling the agent to do something impossible costs it a confused detour before
///   it works out where it is.
/// - **The PR is a draft.** A run nobody watched must not land in a colleague's
///   review queue on its own.
/// - **It never asks a question.** The run is unattended; a prompt waiting for an
///   answer nobody is there to give reads as a hang.
pub fn work_prompt(detail: &IssueDetail, branch: &str, base: &str, ticket_url: &str) -> String {
    let description = if detail.description.is_empty() {
        "(This ticket has no description. The summary above is all there is — if \
         that isn't enough to implement with confidence, make the smallest \
         defensible change and say so in the PR body.)"
            .to_string()
    } else {
        detail.description.clone()
    };
    format!(
        "Implement Jira ticket {key} from start to finish.\n\
         \n\
         # {key}: {summary}\n\
         \n\
         {ticket_url}\n\
         \n\
         ## Ticket description\n\
         \n\
         {description}\n\
         \n\
         ## What to do\n\
         \n\
         1. You are **already on the branch `{branch}`**, in a git worktree of its \
            own that was created for this run from an up-to-date `{base}`. Do not \
            create or switch branches, and do not run `git worktree` yourself.\n\
         2. This worktree is a fresh checkout: dependencies and build artifacts are \
            absent even where the main checkout has them. Install what the \
            repository needs before you run its tests.\n\
         3. Read this repository's own conventions first — its CLAUDE.md, its \
            existing code — and follow them over any habit of your own.\n\
         4. Do the work the ticket asks for, and only that. Leave unrelated \
            problems you notice alone; mention them in the PR body instead.\n\
         5. Run the repository's tests and linters, and get them passing.\n\
         6. Commit with a message that explains why, not just what. Reference \
            {key}.\n\
         7. Push: `git push -u origin {branch}`\n\
         8. Open a **draft** pull request:\n\
            `gh pr create --draft --base {base} --title \"{key} {summary}\" --body \"...\"`\n\
            The body should say what changed, how you verified it, and link \
            {ticket_url}.\n\
         \n\
         ## Rules\n\
         \n\
         - This run is unattended. Nobody will answer a question, so do not ask \
           one — make the best defensible call and record it in the PR body.\n\
         - Do not force-push, do not rewrite existing history, and do not merge \
           anything.\n\
         - Do not touch any branch other than `{branch}`.\n\
         - This worktree shares the repository's history, remote and configuration, \
           so `git`, `gh` and pushing all behave normally. Someone else is working \
           in the main checkout — stay in this directory and leave theirs alone.\n\
         - The pull request must be a draft. It is reviewed by a person before it \
           goes anywhere.\n\
         - If the ticket turns out to be unworkable, stop and leave a clear \
           explanation as your final message rather than committing a guess.\n",
        key = detail.key,
        summary = detail.summary,
    )
}

/// Turn a transport or status error into something worth showing a person.
///
/// The status codes are spelled out because each one has a different fix, and
/// "401" alone doesn't tell the user whether to check the token or the email.
fn describe(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(401) => {
            "Jira rejected the credentials (401). Check `email` under [jira] and \
             the stored API token."
                .to_string()
        }
        ureq::Error::StatusCode(403) => {
            "Jira refused the request (403). The account may lack permission for \
             this query."
                .to_string()
        }
        ureq::Error::StatusCode(404) => format!(
            "Jira returned 404 for {SEARCH_PATH}. Check `site` under [jira]; this \
             instance may also want the older /rest/api/2/search endpoint."
        ),
        ureq::Error::StatusCode(code) => format!("Jira returned HTTP {code}."),
        other => format!("Could not reach Jira: {other}"),
    }
}

/// `Basic base64(email:token)`, hand-rolled rather than pulling a base64 crate
/// for one 30-line function used in one place.
fn basic_auth(email: &str, token: &str) -> String {
    format!("Basic {}", base64(format!("{}:{}", email.trim(), token).as_bytes()))
}

/// Standard base64 with padding. Only ever fed ASCII credentials.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        // Pack the chunk into 24 bits, zero-filling a short tail, then take four
        // 6-bit groups. `pad` is how many of those groups are tail padding.
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let packed = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let pad = 3 - chunk.len();
        for group in 0..4 {
            if group + pad > 3 {
                out.push('=');
            } else {
                let index = (packed >> (18 - group * 6)) & 0x3f;
                out.push(ALPHABET[index as usize] as char);
            }
        }
    }
    out
}

/// The shape of the search response. Private: [`Issue`] is the type callers see.
///
/// `camelCase` throughout because that's Jira's wire convention; naming the
/// fields Rust-style and renaming here keeps the convention boundary in one spot.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSearch {
    #[serde(default)]
    issues: Vec<WireIssue>,
    /// Present when more pages exist. `/search/jql` paginates by token and
    /// reports no total, so this is the only signal that results were cut off.
    #[serde(default)]
    next_page_token: Option<String>,
    /// Some instances send this instead of, or alongside, the token.
    #[serde(default)]
    is_last: Option<bool>,
}

#[derive(Deserialize)]
struct WireIssue {
    #[serde(default)]
    key: String,
    #[serde(default)]
    fields: WireFields,
}

#[derive(Deserialize, Default)]
struct WireFields {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    status: Option<WireNamed>,
    #[serde(default)]
    issuetype: Option<WireNamed>,
    #[serde(default)]
    priority: Option<WireNamed>,
    #[serde(default)]
    assignee: Option<WireUser>,
    #[serde(default)]
    updated: Option<String>,
}

/// The single-issue response. Separate from [`WireIssue`] because it reads a field
/// the search never asks for, and because on **v2** `description` is a plain
/// string — on v3 the same field is an ADF object and this would fail to
/// deserialize. See [`ISSUE_PATH_V2`].
#[derive(Deserialize)]
struct WireIssueDetail {
    #[serde(default)]
    key: String,
    #[serde(default)]
    fields: WireDetailFields,
}

#[derive(Deserialize, Default)]
struct WireDetailFields {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Jira wraps most enumerated fields in an object with a `name`.
#[derive(Deserialize)]
struct WireNamed {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireUser {
    #[serde(default)]
    display_name: Option<String>,
}

/// Parse a search response.
///
/// Split out from [`fetch`] so it can be tested against captured JSON without a
/// live instance — which is the only way to test this at all, and the reason the
/// wire types are shaped to tolerate missing fields rather than demand them.
pub fn parse(body: &str) -> Result<Page, String> {
    // Checked *first*, not as a fallback on a parse failure. Every field of the
    // wire types is `#[serde(default)]` so that one thin issue can't lose the
    // other forty-nine — which means an error body deserializes perfectly happily
    // into a page with no issues. Left to last, this check would never run and a
    // rejected query would render as "No issues matched".
    if let Some(message) = error_messages(body) {
        return Err(message);
    }
    let search: WireSearch = serde_json::from_str(body)
        .map_err(|e| format!("Could not understand Jira's response: {e}"))?;

    let issues = search
        .issues
        .into_iter()
        .map(|issue| Issue {
            key: issue.key,
            summary: issue
                .fields
                .summary
                .unwrap_or_default()
                .trim()
                .to_string(),
            // A missing status would leave an issue in a nameless group, which
            // reads as a rendering bug. Name the gap instead.
            status: name_or(issue.fields.status, "No status"),
            kind: name_or(issue.fields.issuetype, "Issue"),
            priority: issue.fields.priority.and_then(|p| p.name),
            assignee: issue.fields.assignee.and_then(|a| a.display_name),
            updated: issue.fields.updated.map(|u| date_of(&u)),
        })
        .collect();

    // `is_last` is authoritative when present; otherwise a page token means more.
    let more = match search.is_last {
        Some(last) => !last,
        None => search.next_page_token.is_some(),
    };
    Ok(Page { issues, more })
}

fn name_or(named: Option<WireNamed>, fallback: &str) -> String {
    named
        .and_then(|n| n.name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Jira's `errorMessages` array, when the body is an error rather than results.
fn error_messages(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireError {
        #[serde(default)]
        error_messages: Vec<String>,
    }
    let error: WireError = serde_json::from_str(body).ok()?;
    (!error.error_messages.is_empty()).then(|| error.error_messages.join(" "))
}

/// `2026-08-07T14:02:11.000+0100` becomes `2026-08-07`.
///
/// Splitting on `T` rather than parsing: no date library is needed to drop the
/// time, and an unexpected format passes through unchanged instead of becoming
/// an error or an empty cell.
fn date_of(timestamp: &str) -> String {
    timestamp
        .split_once('T')
        .map(|(date, _)| date.to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

/// Group issues by status, in the order the statuses are first seen.
///
/// First-seen order rather than alphabetical, because that is the order the JQL
/// asked for — a query sorted by priority or rank keeps its meaning, and sorting
/// the groups would silently discard it.
///
/// The frontend renders these as real table widgets, so this returns structure
/// rather than text: a text table can't scale its columns to the pane, which is
/// the whole reason the markdown version was replaced.
pub fn group_by_status(issues: &[Issue]) -> Vec<(String, Vec<&Issue>)> {
    let mut groups: Vec<(String, Vec<&Issue>)> = Vec::new();
    for issue in issues {
        match groups.iter_mut().find(|(status, _)| *status == issue.status) {
            Some((_, list)) => list.push(issue),
            None => groups.push((issue.status.clone(), vec![issue])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_is_accepted_in_every_form_people_paste() {
        for site in [
            "acme",
            "acme.atlassian.net",
            "https://acme.atlassian.net",
            "https://acme.atlassian.net/",
            "http://acme.atlassian.net",
        ] {
            let c = JiraConfig {
                site: site.to_string(),
                ..JiraConfig::default()
            };
            assert_eq!(
                c.base_url(),
                "https://acme.atlassian.net",
                "site {site} normalized wrong"
            );
        }
    }

    #[test]
    fn an_issue_url_is_https_whatever_form_the_site_was_pasted_in() {
        // The frontend hands these to `open`, which launches a registered handler
        // for *any* scheme — so "the scheme is always https" has to be a property
        // of the construction rather than something the call site checks. `http://`
        // in config is the case that would otherwise slip through.
        for site in ["acme", "acme.atlassian.net", "http://acme.atlassian.net"] {
            let c = JiraConfig {
                site: site.to_string(),
                ..JiraConfig::default()
            };
            assert_eq!(
                issue_url(&c.base_url(), "TFE-954"),
                "https://acme.atlassian.net/browse/TFE-954",
                "site {site} produced the wrong issue URL"
            );
        }
    }

    #[test]
    fn a_self_hosted_host_is_not_given_the_atlassian_suffix() {
        // The bare-name shorthand only applies to something with no dots in it.
        let c = JiraConfig {
            site: "jira.internal.acme.com".to_string(),
            ..JiraConfig::default()
        };
        assert_eq!(c.base_url(), "https://jira.internal.acme.com");
    }

    #[test]
    fn basic_auth_matches_the_rfc_examples() {
        // Base64 is hand-rolled, so pin it against the canonical vectors — an
        // off-by-one in the padding produces a 401 that looks like a bad token.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(
            basic_auth("aladdin", "opensesame"),
            "Basic YWxhZGRpbjpvcGVuc2VzYW1l"
        );
    }

    #[test]
    fn a_search_response_becomes_issues() {
        let body = r#"{
          "issues": [
            {"key": "ACME-1", "fields": {
              "summary": "Fix the thing",
              "status": {"name": "In Progress"},
              "issuetype": {"name": "Bug"},
              "priority": {"name": "High"},
              "assignee": {"displayName": "Ada"},
              "updated": "2026-08-07T14:02:11.000+0100"
            }}
          ],
          "isLast": true
        }"#;
        let page = parse(body).expect("parses");
        assert_eq!(page.issues.len(), 1);
        let issue = &page.issues[0];
        assert_eq!(issue.key, "ACME-1");
        assert_eq!(issue.summary, "Fix the thing");
        assert_eq!(issue.status, "In Progress");
        assert_eq!(issue.kind, "Bug");
        assert_eq!(issue.priority.as_deref(), Some("High"));
        assert_eq!(issue.assignee.as_deref(), Some("Ada"));
        // Time of day dropped: it's noise, and its format varies by instance.
        assert_eq!(issue.updated.as_deref(), Some("2026-08-07"));
        assert!(!page.more);
        assert_eq!(issue.url("https://acme.atlassian.net"), "https://acme.atlassian.net/browse/ACME-1");
    }

    #[test]
    fn missing_fields_do_not_fail_the_whole_page() {
        // Jira omits fields the account can't see, and a field the instance has
        // renamed simply isn't there. One thin issue must not lose the other 49.
        let body = r#"{"issues": [
            {"key": "ACME-2", "fields": {}},
            {"key": "ACME-3", "fields": {"summary": "Has a summary",
                                         "status": {"name": "Done"}}}
        ]}"#;
        let page = parse(body).expect("parses");
        assert_eq!(page.issues.len(), 2);
        assert_eq!(page.issues[0].status, "No status");
        assert_eq!(page.issues[0].kind, "Issue");
        assert!(page.issues[0].priority.is_none());
        assert_eq!(page.issues[1].summary, "Has a summary");
    }

    #[test]
    fn more_pages_are_reported_from_either_signal() {
        assert!(parse(r#"{"issues": [], "nextPageToken": "abc"}"#).unwrap().more);
        assert!(parse(r#"{"issues": [], "isLast": false}"#).unwrap().more);
        // `isLast` wins when both are present — it's the authoritative field, and
        // a token alongside it would otherwise contradict.
        assert!(
            !parse(r#"{"issues": [], "isLast": true, "nextPageToken": "abc"}"#)
                .unwrap()
                .more
        );
        assert!(!parse(r#"{"issues": []}"#).unwrap().more);
    }

    #[test]
    fn a_jira_error_body_is_reported_in_its_own_words() {
        // Jira explains a bad JQL clause precisely; inventing our own message
        // there would replace the useful text with a generic one.
        let body = r#"{"errorMessages": ["Field 'nope' does not exist."],
                       "warningMessages": []}"#;
        let error = parse(body).expect_err("an error object is not a page");
        assert!(error.contains("does not exist"), "got: {error}");
    }







    #[test]
    fn a_single_issue_response_carries_its_description() {
        // API v2, so `description` is a plain string. The same field on v3 is an
        // ADF object and would not deserialize into this at all.
        let body = r#"{
          "key": "TFE-954",
          "fields": {
            "summary": "Fix the thing",
            "description": "h2. Background\n\nThe thing is broken."
          }
        }"#;
        let detail = parse_issue(body).expect("parses");
        assert_eq!(detail.key, "TFE-954");
        assert_eq!(detail.summary, "Fix the thing");
        assert!(detail.description.contains("The thing is broken"));
    }

    #[test]
    fn an_issue_with_no_description_is_not_an_error() {
        // Plenty of real tickets say everything in the summary. Refusing here
        // would make the button dead on exactly those.
        let detail = parse_issue(r#"{"key": "TFE-1", "fields": {"summary": "Do it"}}"#)
            .expect("parses");
        assert_eq!(detail.summary, "Do it");
        assert!(detail.description.is_empty());
    }

    #[test]
    fn an_error_body_is_reported_from_a_single_issue_fetch_too() {
        // Same trap as `parse`: every field is defaulted, so an error body would
        // otherwise deserialize into a blank issue and the message would be lost.
        let body = r#"{"errorMessages": ["Issue does not exist."]}"#;
        let error = parse_issue(body).expect_err("an error object is not an issue");
        assert!(error.contains("does not exist"), "got: {error}");
    }

    #[test]
    fn a_branch_name_is_the_key_plus_a_kebab_summary() {
        assert_eq!(
            branch_name("TFE-954", "Fix the broken thing"),
            "TFE-954-fix-the-broken-thing"
        );
    }

    #[test]
    fn a_branch_name_is_always_a_valid_ref() {
        // Each of these produced an invalid or absurd ref at some point while this
        // was being written: doubled separators become `..`, trailing punctuation
        // becomes a trailing dash, and a summary with nothing ASCII in it leaves
        // the slug empty.
        for (key, summary) in [
            ("TFE-1", "Fix... the thing!!"),
            ("TFE-2", "  leading and trailing  "),
            ("TFE-3", "Slash/es and \\backslashes"),
            ("TFE-4", "~^:?*[]@{} all the illegal ones"),
            ("TFE-5", ""),
            ("TFE-6", "!!!"),
            ("TFE-7", "日本語のみ"),
            ("TFE-8", "a".repeat(300).as_str()),
        ] {
            let branch = branch_name(key, summary);
            assert!(branch.starts_with(key), "{branch} lost its key");
            assert!(!branch.ends_with('-'), "{branch} ends on a dash");
            assert!(!branch.contains(".."), "{branch} contains ..");
            assert!(!branch.contains("--"), "{branch} has a doubled dash");
            assert!(branch.len() <= MAX_BRANCH_LEN, "{branch} is too long");
            assert!(
                branch
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "{branch} has a character that shouldn't survive"
            );
        }
    }

    #[test]
    fn a_long_summary_is_cut_at_a_word() {
        // Cutting mid-word reads as a typo rather than an abbreviation.
        let branch = branch_name("TFE-954", "Rewrite the authentication middleware entirely");
        assert!(branch.len() <= MAX_BRANCH_LEN);
        assert!(!branch.ends_with('-'));
        // Whatever it kept, it kept whole words.
        for word in branch.trim_start_matches("TFE-954-").split('-') {
            assert!(
                "rewrite the authentication middleware entirely".contains(word),
                "{word} is a fragment of a word"
            );
        }
    }

    #[test]
    fn a_repo_is_found_by_project_key_with_a_tilde_expanded() {
        let mut config = JiraConfig::default();
        config.repos.insert("TFE".to_string(), "~/code/truefire".to_string());

        let repo = config.repo_for("TFE-954").expect("TFE is configured");
        assert!(repo.is_absolute(), "~ should have expanded: {repo:?}");
        assert!(repo.ends_with("code/truefire"));

        // An unconfigured project answers `None`, which is what makes the row
        // offer no run rather than one in the wrong tree.
        assert!(config.repo_for("OTHER-1").is_none());
        assert!(JiraConfig::default().repo_for("TFE-954").is_none());
    }

    #[test]
    fn only_a_configured_status_is_runnable() {
        let config = JiraConfig::default();
        // The default: written down well enough to build.
        assert!(config.runnable("Specified"));
        assert!(config.runnable("New"));
        // Hand-typed config, so case and stray spaces must not decide it — the
        // symptom would be a button that never appears, with nothing saying why.
        assert!(config.runnable("specified"));
        assert!(config.runnable("  SPECIFIED  "));
        // Anything else: not specified yet, or already being worked on.
        assert!(!config.runnable("In Progress"));
        assert!(!config.runnable("Code Review"));
        assert!(!config.runnable("Released"));
        assert!(!config.runnable(""));
        // No status is a substring match — "New" must not admit "New Idea".
        assert!(!config.runnable("New Idea"));
    }

    #[test]
    fn an_empty_status_list_offers_no_runs() {
        // There is deliberately no "any status" setting: that value turns the
        // check off entirely, and it should have to be spelled out as a list.
        let config = JiraConfig {
            run_statuses: Vec::new(),
            ..JiraConfig::default()
        };
        assert!(!config.runnable("Specified"));
        assert!(!config.runnable("anything"));
    }

    #[test]
    fn a_project_key_is_the_part_before_the_dash() {
        assert_eq!(project_key("TFE-954"), "TFE");
        assert_eq!(project_key("ABC-1-2"), "ABC");
        // No dash at all: keep the whole thing rather than reject it.
        assert_eq!(project_key("WEIRD"), "WEIRD");
    }

    #[test]
    fn the_prompt_states_everything_the_app_will_check_afterwards() {
        // `verify` looks for a PR on exactly this branch, so a prompt that let the
        // agent choose its own name would leave the app unable to find its work.
        let detail = IssueDetail {
            key: "TFE-954".to_string(),
            summary: "Fix the thing".to_string(),
            description: "It is broken.".to_string(),
        };
        let prompt = work_prompt(
            &detail,
            "TFE-954-fix-the-thing",
            "main",
            "https://acme.atlassian.net/browse/TFE-954",
        );
        assert!(prompt.contains("TFE-954-fix-the-thing"));
        assert!(prompt.contains("--base main"));
        assert!(prompt.contains("--draft"));
        assert!(prompt.contains("It is broken."));
        assert!(prompt.contains("browse/TFE-954"));
        // The run is unattended, so the prompt has to say so — otherwise a
        // clarifying question just sits there looking like a hang.
        assert!(prompt.contains("unattended"));
        // The branch is already checked out in the run's own worktree, so telling
        // the agent to create it is an instruction that now fails.
        assert!(prompt.contains("already on the branch"), "got: {prompt}");
        assert!(!prompt.contains("switch -c"), "got: {prompt}");
    }

    #[test]
    fn a_ticket_with_no_description_still_produces_a_workable_prompt() {
        // The empty case must not render as a blank section that reads like the
        // fetch failed.
        let detail = IssueDetail {
            key: "TFE-1".to_string(),
            summary: "Bump the timeout".to_string(),
            description: String::new(),
        };
        let prompt = work_prompt(&detail, "TFE-1-bump-the-timeout", "main", "https://x/browse/TFE-1");
        assert!(prompt.contains("no description"));
        assert!(prompt.contains("Bump the timeout"));
    }

    #[test]
    fn an_unconfigured_fetch_fails_before_any_request() {
        let error = fetch(&JiraConfig::default(), "token").expect_err("not configured");
        assert!(error.contains("config.toml"), "got: {error}");
    }

    #[test]
    fn issues_group_by_status_in_query_order() {
        // Group order follows first appearance, so a JQL `ORDER BY` still means
        // something. Sorting alphabetically would silently discard it.
        let issues = vec![
            issue("A-1", "In Progress"),
            issue("A-2", "To Do"),
            issue("A-3", "In Progress"),
        ];
        let groups = group_by_status(&issues);
        let names: Vec<&str> = groups.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(names, ["In Progress", "To Do"]);
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].1.len(), 1);
    }

    fn issue(key: &str, status: &str) -> Issue {
        Issue {
            key: key.to_string(),
            summary: "Fix the thing".to_string(),
            status: status.to_string(),
            kind: "Bug".to_string(),
            priority: Some("High".to_string()),
            assignee: None,
            updated: Some("2026-08-07".to_string()),
        }
    }
}
