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
        format!("{base}/browse/{}", self.key)
    }
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
