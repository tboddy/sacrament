# Jira integration — build plan

The Jira section is the first thing in the editor pane that isn't the editor. It
exists to answer two questions without leaving the app: *what am I meant to be
working on*, and *can the work start itself*.

This document is the reference for building it. It records decisions and the
reasons for them, so a later session doesn't re-litigate settled ground or
rediscover a constraint the hard way. Update the status table at the bottom as
steps land.

## The load-bearing decision: we already own the agent runtime

**The app does not embed an agent loop.** It orchestrates and observes one that
already exists.

What is already in the app, and what each piece contributes:

| Existing feature | What it gives the Jira flow |
|---|---|
| Two shell panes, real PTYs | Somewhere to run `claude` and watch it, with scrollback and a working `Ctrl+C` |
| The `--review` hook (`.claude/settings.json`) | Files the agent edits appear as `◇` unreviewed tabs *while it works* |
| The IPC socket (`core::protocol`, `gui::ipc`) | A process in the shell pane can push files into the editor pane |
| `Buffer::unreviewed` + `mark_active_reviewed` | Viewing a tab clears the mark — the review ledger, already built |
| `claude` and an authenticated `gh` on `PATH` | Branch, commit, push, and PR creation without writing any of it |

So the "one button does the ticket" flow is, at bottom, `claude -p "<ticket
context>"` spawned into a shell pane with a well-built prompt. Claude Code
already does branch → edit → test → commit → push → `gh pr create`, and its
hook already surfaces each edited file in our own editor.

**Why not an embedded agent loop.** There is no official Anthropic Rust SDK, so
Rust talks to the Messages API over raw HTTP. An in-app agent loop would mean
hand-writing tool dispatch, context management, and a permissions model, in a
language with no SDK, inside a single-threaded UI event loop — months of work
for a worse harness than the one already installed. Direct API calls stay in
scope only for small structured one-shots (see "Where a direct LLM call earns
its place").

## Build order

Each step is independently useful and independently shippable. Earlier steps
prove the plumbing that later ones depend on, with progressively more at stake:
step 1 cannot write anything anywhere, step 5 pushes branches.

### Step 1 — read-only dashboard

The whole step is: fetch issues by JQL, render them, show them in the Jira
section. No writes to Jira, no LLM, no git.

What it proves, which everything later needs: credential storage, the HTTP
call, JSON deserialization, the background-work plumbing, and a rendering path
for structured content.

**Superseded — see "Step 1 revisited" below.** The dashboard originally rendered
via markdown:

**Render via markdown, not a new widget.** `core::markdown` +
`gui::read::ReadSource` already render headings by colour, tables with column
alignment, links underlined, code spans — and cache the result against
`(width, revision)`. The dashboard is therefore: issues → a markdown string →
a buffer in read mode → the existing `GridView`. No new rendering code, and it
inherits wrapping, vertical and horizontal scroll, the palette, and the font
coverage check.

This also sidesteps the theme constraint. `theme_guard` allows the 16 theme
colours and no blending, so there is no "subtly tinted status card" available.
Markdown headings and `Palette::ansi_slot()` are the design vocabulary that
actually exists, and read mode already speaks it.

Known limit, accepted for this step: markdown rows are not clickable. Opening a
ticket comes in step 2 via a prompt (`Ctrl+G`'s shape), not a hit-test.

### Step 2 — ticket detail and the linked spec

- `getJiraIssue` for one ticket: description, acceptance criteria, comments.
- The Confluence page linked from the ticket, opened as a markdown tab.

This is the step that is uniquely good in an *editor* and that the Jira web UI
cannot do: the spec and the code side by side, in the same window, in the same
font.

Fetch descriptions through **API v2**, not v3 — see the ADF note below.

### Step 3 — cheap writes

Transition status, add a comment, assign to me, log work. One request each,
immediately useful, and all trivially reversible if wrong. This is where the
write path and its error reporting get exercised while the blast radius is
still a Jira field.

### Step 4 — branch and link, no LLM

Create the branch named from the ticket key and summary. Attach it, and later
the PR, to the ticket as a **remote issue link** — which is how a PR gets onto
a ticket without the GitHub-Jira app installed.

`core::git` currently has only `changed_lines` and `diff_text`; branch creation
is new surface there.

### Step 5 — the button

By this point it is mostly a state machine wrapping `claude -p` in a shell
pane. See "Hard problems in the one-button flow" before starting it.

## Atlassian API reference

### Endpoints this needs

| Capability | Operation | Used by |
|---|---|---|
| JQL search | `searchJiraIssuesUsingJql` | Step 1 |
| Issue detail | `getJiraIssue` | Step 2 |
| Confluence page / CQL | `getConfluencePage`, `searchConfluenceUsingCql` | Step 2 |
| Available transitions | `getTransitionsForJiraIssue` | Step 3 |
| Perform transition | `transitionJiraIssue` | Steps 3, 5 |
| Comment | `addCommentToJiraIssue` | Steps 3, 5 |
| Worklog | `addWorklogToJiraIssue` | Step 3 |
| Remote issue links | `getJiraIssueRemoteIssueLinks` | Step 4 |
| Issue links | `createIssueLink` | Later — link a discovered follow-up |
| Compass components | `getCompassComponents` | Later — map repo to service, filter tickets |

### Gotchas, each of which costs an afternoon if rediscovered

**Descriptions are ADF on API v3.** Atlassian Document Format is a JSON
document tree; rendering it properly is a real project. Hit
`/rest/api/2/issue/{key}` instead and the description comes back as wiki markup
in a plain string. Use v2 for anything that reads issue *bodies*.

**There are no board or sprint endpoints in the platform API.** Those live in
the separate Jira *Agile* REST API (`/rest/agile/1.0/board`, `/sprint`). If
"my current sprint" becomes the wanted dashboard, that is a second API surface,
not a second query against this one. Step 1 therefore uses JQL, which needs
only the platform API.

**Auth is Basic with an API token, not a password.** `email:api_token`,
base64-encoded, in an `Authorization: Basic` header, against
`https://<site>.atlassian.net`. OAuth 2.0 (3LO) exists and is worse for a
desktop app — it needs a redirect URI and a refresh loop for no benefit here.

**Pagination is `startAt` / `maxResults`,** and `maxResults` is capped
server-side (typically 50–100 for search). A dashboard that assumes one
response holds everything is wrong the moment someone has 60 open tickets.
The newer `/search/jql` endpoint paginates by opaque token instead and reports
no total, so "there are more" is the only signal available — which is why
`Page::more` exists.

**`resolution = Unresolved` does not mean "not finished".** It is the intuitive
JQL for an open-work dashboard and it is wrong on any workflow whose terminal
statuses don't set a resolution — which is common, because setting one is a
per-transition config nobody remembers. Measured against the truefirestudios
instance: `resolution = Unresolved` matched 50 issues of which **35 were
`Released` or `Rejected`**. Use `statusCategory != Done`, which keys off the
fixed three-bucket grouping (To Do / In Progress / Done) that exists underneath
whatever custom status names a project invented. Same query on the same
instance: 15 issues, all genuinely open. Naming statuses directly is worse
again — they're per-project.

## Where a direct LLM call earns its place

Small, structured, one-shot calls, where spawning an agent would be absurd:

- Summarize a 40-comment ticket into three lines
- Draft the branch name and PR title from the ticket
- Draft the PR body from the diff
- Classify: is this ticket actually ready to work, or missing acceptance criteria

**Structured outputs are the feature that makes this viable from Rust.**
`output_config: {format: {type: "json_schema", schema: ...}}` constrains the
response to a schema, so the reply deserializes straight into a serde struct —
no "output ONLY valid JSON" prompt-wrangling and no parse-retry loop.

Request shape (raw HTTP; there is no Rust SDK):

- `POST https://api.anthropic.com/v1/messages`
- Headers: `x-api-key`, `anthropic-version: 2023-06-01`, `content-type: application/json`
- Model: `claude-opus-5` ($5/$25 per MTok). `output_config: {effort: "low"}`
  for these small jobs — cheap and fast.
- Auth from `ANTHROPIC_API_KEY`.

### The three tiers, and which tool owns which

| Job | Tool | Why |
|---|---|---|
| Summarize, draft, classify | Direct Messages API call, structured output | One request, no loop, no tools needed |
| Do the ticket | `claude` CLI in a shell pane | It already has the tools, the context management, and the permissions model |
| Nightly backlog triage, no local repo | Managed Agents scheduled deployment | Runs in Anthropic's cloud on a mounted GitHub repo — a companion to the local flow, not a replacement |

## Plumbing patterns already in this repo

**Background work has three precedents.** `pty.rs`, `watch.rs`, and `ipc.rs`
all use *background thread → `futures::channel::mpsc::unbounded` →
`Subscription` → `Message`*. That is the pattern for anything streaming.

**An HTTP fetch is not streaming, so it does not need that.** A request/response
fetch is `Task::perform(async { blocking_call() }, Message::Loaded)` — the same
shape the `rfd` dialogs already use. iced's executor here is `thread-pool`
(a futures thread pool, not tokio), so a blocking call inside the future
occupies one pool thread for the duration of the request. Fine for a
human-paced fetch; would not be fine for a stream.

**Do not add tokio.** `reqwest`'s async stack drags it in. A blocking client on
a pool thread costs nothing here and keeps the dependency tree honest.

**`core` carries no UI dependency,** enforced by its `Cargo.toml`. The Jira
client, its types, and the markdown formatting all belong in `core::jira`;
only the section wiring belongs in `gui`.

## Credentials

`config.toml` is plaintext **and shared with v1**. No token goes in it.

- **Non-secret config** (`site`, `email`, the JQL query) goes in a `[jira]`
  table in `config.toml`. Safe to add: v1's `Config` is `#[serde(default)]`
  without `deny_unknown_fields`, so v1 ignores the new section.
- **The token** goes in the macOS Keychain, read via the `security` CLI. That
  keeps the secret out of the repo, out of backups of the config, and out of
  any future `config.toml` serialization.

This decision has to be made before the client is written; retrofitting secret
storage after the fact means touching every call site.

## Constraints that apply to everything here

- **Every colour comes from `Palette`,** and nothing synthesizes one. No
  blending, no darkening, no literals. `theme_guard.rs` fails the test suite on
  violations. Where a dimmer colour is wanted, `Palette::dim()`.
- **No emoji, ever** — not in labels, markers, messages, or docs. And no
  turning a symbol into one: a glyph that is merely emoji-*presentation* must
  come from the monochrome fallback chain. See CLAUDE.md, "Fallback is ours".
- **Glyph coverage is not guaranteed.** `text` widgets shape with
  `Shaping::Basic` and no font fallback, so a character the configured font
  lacks draws as *nothing*. Anything non-ASCII in chrome goes through
  `State::marker`'s coverage check; anything in the grid goes through
  `GridView`'s.
- **No status bar.** Everything the app has to *say* is a native alert
  (`State::alert`); state that changes continuously goes in the window title.
- **Section commands are gated on `State::editing()`.** The Jira section holds
  focus without the editor being on screen, so anything that touches the
  buffer must check. See CLAUDE.md, "Sections".

## Hard problems in the one-button flow

Recorded now, while it is cheap to design around them.

**The self-review gate must be able to fail and hold the transition.** As
originally sketched, the flow ends by moving the ticket to Code Review. If the
review finds problems and the ticket moves anyway, the app has automated
putting bad PRs into a team's queue — the single outcome that makes colleagues
hate a tool. The transition must be conditional on the review passing; a failed
review should leave the PR as a draft with the findings as a comment.

**Seven steps means six places to die.** Branch created, no commit. PR open,
ticket not moved. This needs a persisted run record (`session.toml` is the
pattern) and an idempotence check per step: does the branch exist, is a PR
already open, is the ticket already in that state. Without those, re-running
after a crash makes a mess.

**Nothing in step 5 is reversible by the app.** A pushed branch and an opened
PR are outward-facing. "Start" must be a deliberate act with the plan visible
first — not a button that can be grazed with the pointer.

## Status

| Step | State | Notes |
|---|---|---|
| 1 — read-only dashboard | **built, untested against a live instance** | See "What step 1 landed" |
| 2 — ticket detail + spec | not started | |
| 3 — cheap writes | not started | |
| 4 — branch and link | not started | |
| 5 — the button | not started | |

## What step 1 landed

| File | Contents |
|---|---|
| `core/src/jira.rs` | `JiraConfig`, `Issue`, `Page`, `fetch`, `parse`, `to_markdown`. Blocking HTTP via `ureq`; hand-rolled base64 for Basic auth rather than a crate for one function |
| `core/src/secret.rs` | `lookup(env, service, account)` — environment variable, then macOS Keychain via the `security` CLI |
| `core/src/config.rs` | `[jira]` table on `Config` |
| `gui/src/buffer.rs` | `Buffer::markdown_view` / `set_markdown` — a path-less, unsaveable buffer already in read mode |
| `gui/src/main.rs` | `JiraPane`, `jira_refresh`, `jira_section`, `read_target`, `Cmd+R`, the clickable `Refresh` heading |

Configuration:

```toml
[jira]
site = "your-company"          # or your-company.atlassian.net, or the full URL
email = "you@your-company.com"
query = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
max_results = 50
```

```
security add-generic-password -s sacrament-jira -a you@your-company.com -w '<token>'
```

`SACRAMENT_JIRA_TOKEN` overrides the Keychain, for scripting and development.

### Decisions taken while building

- **`Task::perform`, not a subscription.** One request and one response, so the
  streaming apparatus would be overhead. The blocking call holds one thread-pool
  thread for the request's duration.
- **`read_target()` is the single decision of which buffer scrolls.** The editor
  pane now hosts two read-mode surfaces (a markdown file, the dashboard). The key
  handler and the wheel handler both ask it, so they cannot disagree about which
  one moved.
- **The setup instructions are the dashboard, not an alert.** An unconfigured
  integration isn't an error, and setup text is something to read and copy from —
  which a dialog with an OK button is bad at. Fetch failures do both: an alert
  that can't be missed, plus the text left in the pane so it survives dismissal.
- **The first fetch happens when the section is first shown,** not at startup. An
  app launched to edit a file shouldn't make a network request nobody asked for.
  Gated on `attempted`, so a failure doesn't re-fire on every visit.
- **A refresh is not an edit.** `set_markdown` clears the undo history and the
  dirty flag: undoing into a previous fetch's text is meaningless, and a dirty
  flag would claim there's something to write.
- **The `Summary` and `Refresh` headings are widgets, not markdown.** A cell grid
  can't hit-test the text inside it, so a clickable heading has to be chrome. The
  colours come from `core::markdown::{h1_slot, h2_slot}` so they can't drift from
  the renderer. `Refresh` is the app's only `Interaction::Pointer` — the cursor is
  its only affordance, since it looks exactly like a heading at rest.
- **The dashboard has no footer.** Rule, count, and query echo removed as noise.
  The truncation warning stays: it's a correctness signal, not decoration.

### The one real risk

**The search endpoint is unverified against a live instance.** Atlassian
deprecated `GET /rest/api/{2,3}/search` in favour of `/rest/api/3/search/jql`,
which is what `SEARCH_PATH` uses. If an instance disagrees, the 404 message says
so explicitly and names the older path — the constant is a one-line change. The
response parsing tolerates missing fields throughout, so a partial match degrades
rather than failing.

Everything is tested against captured JSON (`parse`), never against the network.
`fetch` itself is only covered for the unconfigured case.


## Step 1 revisited — real tables

The markdown dashboard was replaced with widgets. The reason is narrow and worth
keeping: **a text table cannot be responsive.** Its columns are measured in
characters, so they fit exactly one pane width. `markdown::emit_table` scales
columns down when the natural width exceeds the pane but never truncates a cell,
so a narrow pane produced rows that each overflowed by a different amount and no
column lined up. Bounding the summary to a character budget bought alignment at
one width and lost it everywhere else.

What replaced it:

- `core::jira::group_by_status` returns structure; `to_markdown`, its cell
  escaping, and the summary budget are gone.
- `State::jira` holds a `JiraView` — `Loading`, `Ready(Page)`, or `Note(String)`
  for setup help and errors — instead of a text buffer.
- `jira_tables` / `issue_table` build the tables. Every column is a
  `FillPortion` (`JIRA_COLUMNS`): rows are separate widgets, so `Shrink` columns
  would size per row and come out ragged, while portions give every row the same
  split at any width. Summaries wrap rather than truncate.
- The **whole section** is inside one `scrollable`, headings included, rather
  than a fixed header over a scrolling list — one scroll region reads as one
  document, and pinning the title plus the refresh control would spend two rows
  of a short pane on chrome that has nothing to say while you read. Content keeps
  its natural height inside it; `Fill` there clamps to the viewport and leaves
  nothing to scroll.

Known cost: keyboard scrolling. The grid gave arrow keys and PageUp/PageDown for
free through `read_key`; a `scrollable` takes the wheel but not those keys, and
`read_target` no longer has a Jira buffer to point at.
