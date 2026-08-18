# The Jira section and the Run button

Extracted from `CLAUDE.md`, which now carries a summary and points here. The
forward-looking build plan is `docs/jira-integration.md`; this document is what
shipped and the seams it established.

#### The Jira section (`core::jira`, `core::secret`, `State::jira`)

A read-only ticket dashboard. The full build plan for the Jira work — including
the four steps after this one — is `docs/jira-integration.md`; this section covers
only what shipped and the seams it established.

**The dashboard is real widgets, not the markdown renderer** — and that was a
reversal. It began as generated markdown fed through `read::ReadSource`, which
cost no new drawing code, but at the time `markdown::emit_table` scaled its
columns down when they didn't fit and *never* made a cell honour the result, so
at a narrower width every row overflowed by a different amount and no column
lined up with the one above it. A character budget on the summary bought
alignment at one width and lost it at every other.

That renderer bug has since been fixed (see "Tables" in `CLAUDE.md`), so the original reason no
longer holds — a markdown table now stays aligned at every width. The dashboard
stays widgets anyway, for a reason the fix doesn't touch: its refresh control is
a real clickable widget, which a cell grid has no way to express.

So `core::jira` hands over structured `Issue`s and `jira_tables` lays them out.
**Every column is a `FillPortion`** (`JIRA_COLUMNS`), which is what keeps rows
aligned: each row is its own widget, so a `Shrink` column would size independently
per row and the table would come out ragged. Portions give every row the same
proportional split at whatever width the pane has, and summaries wrap instead of
being truncated.

The section still draws its two headings as chrome (see below) and still takes
its heading colours from `core::markdown`, so it matches rendered markdown
elsewhere. What it no longer does is render *through* it.

**The issue key opens the ticket in the browser** (`issue_key_cell` →
`Message::JiraOpenIssue` → `core::jira::issue_url` → `open_url`), which is the
second thing widgets buy that the grid couldn't: a cell grid reports a row and a
column, not "you clicked TFE-954". Four things about it:

- **The hot area is the text, not the column.** Every other cell is a `text`
  filling its `FillPortion`, and doing that here would arm the whole blank
  remainder of the Key column — a wide band of screen that launches an app when
  clicked. So the `mouse_area` wraps a `Shrink` text and a `container` carries the
  portion instead.
- **The message carries the key, not a URL.** `issue_url(base, key)` derives it in
  `update` from the configured site, so the URL's shape lives in `core` beside
  `Issue::url` rather than being built at a call site — and a refresh that
  reordered or dropped rows can't leave a stale URL in flight.
- **The scheme is https by construction, and that's the security boundary.**
  `JiraConfig::base_url` strips whatever form the site was pasted in and
  re-prefixes it, and the key only ever reaches the path — so `open`, which
  launches a registered handler for *any* scheme, can only ever get a web page.
  Same rule `Terminal::url_at` enforces for a shell click, pinned here by
  `an_issue_url_is_https_whatever_form_the_site_was_pasted_in`.
- **Colour is the affordance and the cursor stays an arrow**, as it is for
  "Refresh" and for shift-clicking a URL in a shell. `LINK_SLOT` / `LINK_HOVER_SLOT`
  (blue → bright_blue) are shared by both controls, so the section has one look for
  clickable text; bright_blue is also what `core::markdown` renders a link in.

`JiraPane::hovered` went from a `bool` to `Option<JiraHover>` for this — one field
for the section, since exactly one thing can be under the pointer. With more than
one hoverable control the tab strips' tree-order hazard is back, so
`Message::JiraUnhovered` carries **which** thing left and clears only that:
widgets publish in tree order, so moving from one key to the one above it emits
that key's `on_enter` before the departed key's `on_exit`.

`Buffer::markdown_view` was built for the earlier approach and removed with it. If
a future section wants generated prose in the grid, that constructor — a path-less
buffer forced into read mode — is the shape to bring back; it's in the history.

`read_target()` is **the single decision of which buffer scrolls.** The editor pane
hosts two read-mode surfaces now — a markdown file and the dashboard — and both the
key handler (`read_key`) and the wheel handler ask it. Two independent answers to
"which one moved" is the bug they would otherwise take turns having.

**Secrets do not go in `config.toml`.** That file is plaintext, hand-edited, and
*shared with v1*, so an API token in it would sit in the file users paste into
issue reports. `core::secret::lookup` reads the environment variable first (the
scriptable override) then the macOS Keychain via the `security` CLI — no crate
needed, and the allow prompt is the system's own. A Dock-launched app inherits
launchd's environment rather than a shell's, so the Keychain is the path that
matters in practice and the env var is for development. Non-secret settings
(`site`, `email`, `query`) stay in `[jira]` where they can be seen and edited.

Three smaller decisions:

- **`Task::perform`, not a subscription.** A fetch is one request and one
  response, unlike the PTY's stream, so it needs none of the channel machinery
  `pty`/`watch`/`ipc` share. The blocking `ureq` call holds one thread-pool thread
  for the request; iced's executor here is `thread-pool`, so **no tokio** — which
  `reqwest`'s async stack would have dragged in.
- **Setup instructions are the dashboard; failures are both.** An unconfigured
  integration is a normal state, not an error, and setup text is something to read
  and copy from — which a dialog with an OK button is bad at. A *fetch failure*
  raises an alert **and** leaves the text in the pane, so it survives being
  acknowledged rather than leaving a stale dashboard behind.
- **The first fetch is on first show, not at startup,** gated on
  `JiraPane::attempted` so a failure doesn't re-fire on every visit. An app
  launched to edit a file shouldn't make a network request nobody asked for.

**Untested against a live instance.** `parse` is covered against captured JSON and
tolerates missing fields throughout (Jira omits what an account can't see, so one
thin issue must not lose the other forty-nine). `SEARCH_PATH` points at
`/rest/api/3/search/jql`, which replaced the deprecated `/rest/api/{2,3}/search`;
if an instance disagrees the 404 message names the older path, and the constant is
a one-line change.

**Issue bodies are read through API v2** (`ISSUE_PATH_V2`, `fetch_issue`), and the
version split is the whole point: on v3 a description arrives as an ADF document
tree — nested JSON needing a renderer — while v2 returns the same content as wiki
markup in a plain string. The search stays on v3 because it asks for no bodies.
One ticket is fetched when a run is about to start, never fifty on the dashboard.

#### The Run button — one click does the ticket (`core::work`, `State::runs`)

A `Run` link on each dashboard row starts Claude Code on that ticket in a shell
pane: branch, do the work, commit, push, open a **draft** PR. This is step 5 of
`docs/jira-integration.md`, and that document's load-bearing decision holds — **the
app does not embed an agent loop, it orchestrates the one already installed.** The
whole feature is a prepared prompt, a shell tab, and a check afterwards.

The flow: `Run` → pre-flight and ticket fetch on a background thread → a confirm
dialog showing the verified plan → a **git worktree** created for the run → a shell
tab in it running one line → the shell exits → the app reads the repository, and
tidies the worktree away → alert, and the PR opens.

**The agent never works in the checkout you are working in** (`work::add_worktree`,
`paths::worktree_dir`). Every run gets a worktree of its own, branched from an
up-to-date base, and its shell opens there rather than in the configured repository.

This replaced a dirty-tree refusal in `preflight`, and the swap is the difference
between a button that gets pressed and one that doesn't. Starting a ticket is
supposed to be an *aside* from whatever you are already doing — which is exactly
when the tree has edits in it, so the guard fired almost every time and the fix it
demanded was to stash your own work to make room for an agent. The guard was right
about the danger and wrong about the remedy: uncommitted work in the same tree an
unattended agent is committing from would be swept into a pull request under a
ticket number, and hard to unpick once pushed. A separate checkout removes the
hazard rather than refusing in front of it.

Four things about it:

- **The refs are shared, so nothing downstream changed.** `git worktree` puts a
  second working tree on one object store, so `work::verify` still asks the *main*
  repository about the branch and `gh` still sees the same remote. That's why the
  worktree is an addition to this feature rather than a rewrite of it.
- **It's created after the dialog, never before** (`Message::JiraRunWorktree`). A
  worktree is a branch *and* a directory, so making one during the pre-flight would
  leave both behind every time someone pressed Cancel. `JiraPane::preparing` stays
  set across it, because a fetch plus a checkout is seconds of nothing on screen and
  the cell reading `Run` again would invite a second press.
- **It lives in the cache directory, not the config one** (`paths::worktree_dir` →
  `~/.cache/sacrament/sacrament2-worktrees/<branch>`), which is the one place this
  app puts anything outside `~/.config`. A worktree is *derivable* — the commits are
  on the branch, in the repository, and survive the directory — and it is large,
  since the agent's own test run builds `target/` or `node_modules/` inside it. The
  transcript stays under `~/.config` because it is the opposite on both counts: the
  only record of what an unattended agent did.
- **The prompt had to be told** (`jira::work_prompt`). It used to instruct
  `git switch -c`, which now fails: the branch is already checked out. It also says
  the checkout is fresh, so the agent installs dependencies instead of being baffled
  by a missing `node_modules` the main checkout has.

**A finished run's worktree is tidied away, unless there's something in it to lose.**
`work::remove_worktree` never passes `--force`, and that single choice is the whole
policy: plain `git worktree remove` deletes a clean tree and *refuses* one holding
modified or untracked files. So a run that committed and pushed everything leaves
nothing behind, and a run that died mid-thought keeps its checkout — with
`report_run` naming the path and the command in the alert, since a directory nothing
on screen has mentioned is one nobody will ever find. Verified rather than assumed:
ignored files don't block it, so `target/` doesn't strand every worktree.

Removing it never loses work. The commits are on the branch, in the shared object
store, and the branch is left in place.

An **aborted** run (tab closed) keeps its worktree deliberately, matching how it
keeps its branch: the agent process is still being killed as the tab goes, and a run
someone stopped by hand is the one they most want to look at. It's discovered again
by `preflight`, which refuses a leftover worktree and prints the `git worktree
remove` line for it.

**The app dictates the branch name** (`jira::branch_name` — `TFE-954-kebab-summary`),
and that is what makes the run checkable rather than merely started. Because the
name is known before anything runs, `work::preflight` can refuse when it already
exists and `work::verify` can find the pull request afterwards without believing
anything the agent said. An agent choosing its own name leaves the app unable to
tell "done" from "did nothing".

**The agent's account of its own work is not evidence.** `work::verify` asks git
whether the branch exists, how many commits are on it, and whether it was pushed,
then asks `gh` for the PR — and `Outcome::describe` names *how far it got*, because
that decides what the user does next. "Pushed but no PR" needs `gh pr create`;
"branch never created" needs the whole thing again.

**Everything outside the base system runs through a login *and interactive* shell**,
and this is the same trap `pty.rs` records. A Dock-launched app inherits launchd's
`/usr/bin:/bin:/usr/sbin:/sbin`, so `gh` (Homebrew) and `claude` (`~/.local/bin`)
are simply not findable — while the identical binary run from a terminal finds
both, which makes it invisible in development and total in production. `git` is
called directly, as `core::git` already does: `/usr/bin/git` is on the minimal
`PATH` regardless.

**`-i` is half the fix, and leaving it out fails in a way that accuses the wrong
thing.** `zsh -l -c` reads `/etc/zprofile` and `~/.zprofile` but **not `~/.zshrc`**,
which zsh sources only for interactive shells — and `.zshrc` is where a great many
people, including this repo's author, actually set `PATH`. So a login-but-not-
interactive shell found neither `claude` nor `gh`, and the pre-flight refused with
"`claude` isn't installed" while `claude` ran perfectly in the shell pane two inches
below the dialog. That pane is the proof rather than a contradiction: a PTY shell is
login *and* interactive, so the check was stricter than the thing it was checking.

Reproducing it needs no Dock launch, just an empty environment:

```text
env -i HOME=$HOME PATH=/usr/bin:/bin /bin/zsh -l    -c 'command -v claude'   # nothing
env -i HOME=$HOME PATH=/usr/bin:/bin /bin/zsh -l -i -c 'command -v claude'   # found
```

Interactive brings one cost with it: `.zshrc` files print things, and that output
would be read as the script's answer — a startup `echo` would make every tool check
report "couldn't run a login shell", and one in front of `gh`'s JSON would make a
real pull request parse as none. So `login_shell` prints `OUTPUT_MARKER` first and
keeps only what follows it. `after_marker` is split out to be tested, because
arranging for a real shell to be chatty means writing dotfiles into a fake `HOME`
and setting `SHELL` process-wide, which is flaky under a threaded test runner.
Measured at ~0.3s with no tty and no `TERM`, which is what a Dock launch has.

**The prompt is a file the shell reads, not text on the command line**
(`run_command` → `claude … "$(cat <path>)"; exit`). A ticket description is
thousands of characters, and putting it on the line means zsh echoing and
re-wrapping all of it in a tab you're watching, every shell metacharacter in the
ticket needing correct escaping, and history expansion seeing any `!` in the text.
Inside `$(cat …)` none of that is true, and the only thing needing quoting is a
path this app generated (via `shell_escaped`, already there for dropped files).

**`; exit` is the completion signal.** The shell ends when the agent does, which
fires `pty::Event::Exited` — the event that already removes a dead tab — so the run
reports itself with no polling, no timer and no new plumbing.

**Unattended is a deliberate choice, and the guard rails are what pay for it.**
`--dangerously-skip-permissions` is the point of the button: a run that stops to
ask in a tab nobody is watching reads as a hang. What makes it acceptable is
everything around it, and removing any one of these changes the trade:

| Guard | What it prevents |
|---|---|
| Status not in `run_statuses` → no control at all | An agent turned loose on a ticket nobody has specified yet |
| No repo configured for the project → no control at all | An agent working in the wrong tree |
| The agent runs in a worktree of its own (`work::add_worktree`) | Your uncommitted work swept into an agent's commits — and having to stash it to start a ticket |
| `preflight` refuses an existing branch, local or remote | Building on a previous run nobody remembers |
| `preflight` refuses a leftover worktree | A second agent in the checkout a previous run kept |
| `preflight` checks `gh auth status` | Ten minutes of work, then a login prompt at `gh pr create` |
| One run per repo (`State::runs`) | Two test suites at once against one development database — worktrees separate the *git* state, not the ports and fixtures around it |
| `JiraPane::preparing` covers pre-flight *and* the dialog | A second dialog for the same ticket, and two agents from two answers |
| Confirm dialog, with `Show prompt` | A push and a PR from a pointer graze — and the prompt is readable first |
| The PR is a draft | Unreviewed agent output in a colleague's queue |

**The transcript is saved on *both* ways a run ends** (`State::take_run`). The grid
is dropped with the tab, so that is the last moment the record exists — and losing
it when the user *closes* the tab would be backwards, since a run someone aborted
is the one they most want to read. Only a shell that exited on its own is
*reported*, though: `close_shell` takes the record first, so an abandoned run isn't
verified as if it had finished.

`Terminal::transcript` walks the grid rather than reusing `selection_to_string`,
which would mean installing a select-all over the user's own live selection.
Wrapped rows are rejoined, or every command longer than the pane comes back with a
newline through it.

Two smaller pieces this needed, both general rather than Jira-specific:

- **`Shell::on_attach`** — a command typed in once the PTY is live. `Attached`
  `take`s it, so it can't replay on a shell the user has since made their own.
  `SACRAMENT_SPIKE_CMD` now goes through it too, rather than a branch beside it.
- **`Shell::label_override`** — a run's tab reads `TFE-954` for its life.
  `refresh_cwd` leaves it alone; without it every run in a repo shares one tab
  label, the directory basename.

**Only a ticket that's ready to build gets the control** (`JiraConfig::runnable`,
`run_statuses`, defaulting to `Specified` and `New`). This is *configurable rather
than fixed, because status names are per-project* — the same warning this file's
JQL note gives. `statusCategory` is identical on every instance but far too coarse
here: "To Do" covers a ticket nobody has written up as well as one ready to build,
and the whole premise of an unattended run is a description good enough to work
from. Only the workflow's own names can tell those apart, and two projects on one
instance can disagree.

Matched case-insensitively and trimmed, because it's a display string typed into a
config file by hand and the failure mode is a button that silently never appears.
An empty list runs nothing; there is deliberately no "any status" value, since
that's the one setting that turns the check off and it should have to be spelled
out as a list.

It costs nothing to read, because **the dashboard is already grouped by status** —
the whole `Specified` table carries the control and the whole `In Progress` one
doesn't, so it reads as a rule rather than as rows behaving differently.

Configuration, in `[jira]`. The repo map is keyed by project key because a
dashboard built from `assignee = currentUser()` spans whatever projects you're on:

```toml
[jira]
run_statuses = ["Specified", "New"]

[jira.repos]
TFE = "~/code/truefire"
```

`[jira.repos]` must be the **last** thing in `[jira]`: a sub-table header closes
the table above it, so a plain key written after it lands in `repos` instead.

Deliberately **not** persisted: `State::runs` describes live agents, and a restored
record would name shells that a restart already killed. The branch and any commits
are in the repository, which is where the state that survives belongs. A restart
re-spawns a plain shell in the worktree directory — it does not restart the agent,
and it does not clean up: a run killed by a restart leaves its worktree, which
`preflight` then refuses by name if the ticket is started again.

**Untested end to end.** Everything pure is covered, and the git half is now driven
against real repositories — branch names against invalid refs, the prompt's contents,
`Outcome::describe` for each failure shape, and against a purpose-built temp repo:
`preflight`'s refusals, a worktree created beside a *dirty* main checkout leaving it
untouched, a clean worktree removed while a dirty one keeps itself, ignored build
output not counting as work, and a hand-deleted worktree not poisoning its path. What
has still never been driven is a whole run against a live Jira instance and a real
remote. `RUN_BASE` is `main` and `RUN_AGENT` is a constant; both are one-line changes.
