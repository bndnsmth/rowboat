# Workspace AI and performance review

Rowboat can delegate workspace questions and performance evidence to an **already
installed, signed-in Codex, Claude Code, or OpenCode V2 CLI**. This is optional,
off by default, and advice-only unless trusted investigation is explicitly enabled
for a connection. Rowboat does not install agents, manage model
accounts/API keys, or implement provider HTTP calls.

## Desktop workflow

1. Open **Settings → AI**, enable assistance, and choose your agent. Leave the
   executable override empty for PATH detection, or supply an absolute executable
   path (including paths with spaces). Shell commands and additional arguments
   are not accepted. An optional model identifier and 15–600-second deadline are
   available. Changing providers clears executable/model overrides in the draft;
   reselecting the same provider keeps them. Save preferences before starting a review.
2. **Check agent availability** checks advertised automation controls (or Claude
   Code's supported version baseline). It does not submit a prompt or verify account/model access.
3. Use the **sparkles AI button at the top right** (or **Open AI query
   assistant…** in Commands) to toggle the **Workspace AI** right sidebar. It
   stays open while you work, with one memory-only conversation per workspace.
   Opening it alone neither queries the database nor contacts an agent. On narrow
    windows it overlays the right side instead of crushing the SQL editor.
   If AI is off, **Enable AI** in the sidebar enables assistance for the current
   workspace without granting investigation permission or submitting a prompt.
4. Ask a question, author SQL, explain the current query or discuss results. The
   context strip follows the current database/table/query; **Send** / **Ctrl+Enter**
   freezes a snapshot. SQL and visible result metadata are included by default;
   schema, metrics and selected **values** have separate sharing switches.
   **Context** collapses the sharing controls into a single-row toolbar and
   expands them again. The toolbar keeps the target summary and **Trusted**,
   **Values**, or **Pinned** indicators visible; hover the summary for details.
   Sending automatically collapses the controls to make room for the conversation.
   Expanding/collapsing does not change sharing, drafts or a pinned preview, and
   remains available while the agent works.
   **Review context** opens an editable/redactable preview. That preview is pinned
   until sent or explicitly discarded, even if the editor/selection changes or
   **Review context** is clicked again. Closing/reopening the sidebar preserves
   sharing choices; global schema/metrics defaults apply only on its first opening.
5. Responses have passive Markdown formatting and SQL cards. **New SQL tab** opens
   a suggestion without executing it or replacing your current draft. **Stop**
   cancels this request. Closing the sidebar cancels pending work but keeps the
   conversation in memory. **New chat** clears it. No provider session is resumed.
   Pending requests show animated activity, including while partial advice arrives;
   it stops on completion or cancellation. Focused performance review uses the same
   indicator. Animation respects reduced motion and pauses for hidden windows.

### Focused performance review

Explain a query, then use **Ask AI** / **AI assistant** in the plan viewer for the
original performance-specific workflow. **Ask in sidebar** instead attaches the
captured plan and SQL to Workspace AI and fills a question without sending it.
**Load query plan…** in the sidebar is an explicit estimated-EXPLAIN request.

1. Read/edit the performance context. It includes the SQL template and captured plan,
    not an automatically executed ANALYZE. Optional sections use **already loaded
    selected-table metadata** and an **existing builtin metric sample**. Changing
    these options updates an untouched preview automatically. Edited previews are
    preserved; **Update preview (replaces edits)** is required before sending
    changed evidence options. No fresh schema/metrics queries are issued.
2. Choose **Analyze query** to send the reviewed text. Advice has its own view,
    with headings, lists and inline code cards rather than raw Markdown. It remains
    labeled unverified. **Review context** returns to the editable preview. Cancel
    stops this request, not a database query or a separate agent session.
3. Complete fenced `sql` suggestions have **Copy** and **Open SQL in editor**
    controls. Opening creates a new tab and never runs the suggestion.
    Tabs include an unverified-AI comment. Ordinary manual SQL
   execution still executes immediately; review the real target and operational
   risks before running any suggested statement.

A follow-up adds the current context, previous answer and question to the
editable review through **Review follow-up**. It does **not** send automatically
or resume a provider session; choose **Analyze query** after reviewing it.
Opening new SQL, refreshing a plan, closing the viewer or changing the database
cannot rebind a pending response to a different plan/session. Closing/discarding
an analysis cancels its subprocess. Cancelling or failed advice cannot be opened
using the suggestion buttons; partial text can still be copied for inspection.

Settings are split into **Appearance**, **Editor & Results**, **Shortcuts**, and
**AI**. Changes remain drafts until Save. AI may be disabled for the current saved
connection independently of the application-wide switch; that policy is checked
again when sending. A connection/session or observed database change resets the
sidebar conversation, pinned preview and plan attachment, cancels its request,
and disables sharing selected values; an unsent question is kept. Merely changing
tables/query tabs does not erase the conversation. Previous messages remain in
the conversation even after sharing options change; use **New chat** to remove them.

## Shared context contract

`rowboat_services::ai_context::WorkspaceContext` is a versioned, UI-independent
snapshot. A single GUI projection supplies workspace/session/connection identity,
the last observed database and context confidence, selected table, editor revision
and selected SQL, visible result metadata/source/completeness, optional original
selected cells, loaded schema/metrics, transaction state and pending-edit count.
Editor SQL and executed-result source are separate; pins use their own provenance,
and a visible comparison never silently attaches hidden live result rows.
Capturing a snapshot performs no database operations. It contains no GPUI entities,
database handles, credentials, saved-profile collection or query history.

Context is bounded: SQL/source excerpts 32 KiB each; schema/plan 48 KiB each;
metrics 16 KiB; first 128 result columns; at most 20 selected rows/100 cells and
24 KiB estimated cell encoding. Oversized cells/sections are omitted and marked,
not silently presented as complete. Selected values are original loaded cells,
not staged edits. Context/conversation is at most 192 KiB; oversized messages fail
with a request to reduce context or start a new chat. Questions are at most 16 KiB,
and a sidebar conversation is limited to eight turns. Chats are not persisted.

## Trusted investigation (opt-in)

Enable **Allow agent investigation** in the sidebar, then acknowledge the warning
for that connection. It can also be changed explicitly in **Settings → AI**.
If AI is off or blocked for that connection, the confirmation explicitly offers
**Enable AI + investigation** and enables both in one change. Cancelling changes
neither setting. Read-only preferences disable these actions rather than pretending
permission was saved. Enabling alone never sends a request; press **Send** separately.
The default remains tools-off advice. Performance-only **Analyze query** remains
tools-off even when the sidebar has trusted permission.

Trusted mode enables shell/read tools and supplies instructions to first read
`rowboatctl skills get core --full`, then investigate using `--read-only`, bounded
queries, deadlines and plain EXPLAIN. The CLI must be alongside Rowboat or on PATH.
Rowboat creates a private temporary profile store containing **only the exact
active target**, with read-only profile policy and the original ID for keyring
lookup. SQLite targets always use open-existing mode, including files originally
created by Rowboat. Relative SQLite, TLS and SSH identity paths are resolved against
Rowboat's working directory before the private profile is written; the agent's
temporary directory does not change their meaning. It does not pass the GUI's
session-only password, history, other profiles or live connection. The temporary
profile/context directory is cleaned up when
the request ends. Read-only policy applies when that CLI target is used; the
agent is not confined to that target or command.

**This is trusted-agent execution, not an enforced tool broker or sandbox.** The
agent can ignore instructions, execute shell commands with user authority, read
files, use available credentials/keyring access, contact networks or select other
targets. Enable it only for a trusted executable and connection. Only generic tool
activity counts are shown, not raw shell arguments/output. Trusted tools can fetch
and share database values independently of the initial snapshot's selected-value
switch. Advice still requires
review; Rowboat itself never automatically applies SQL suggestions.

Every CLI invocation opens a **new session**. GUI transactions, temporary tables,
SQLite attachments and session settings are not inherited. Current database context
must be known and the GUI idle before starting an investigation; context switching
cancels/invalidates the response but is not proof that a remote query stopped.

The performance dialog defaults to a bounded 960×720 logical-pixel surface,
clamped to the window, with separate **Execution plan** and **AI assistant** tabs.
Captured SQL is collapsed in the plan view; **Captured SQL** reveals it.
Shared dialog controls provide corner dragging, maximize/restore and keyboard
resizing (focus the grip, then use arrows; Shift uses smaller steps). Sizes are
remembered per dialog kind in the current window only, not saved preferences.
Legacy form contents may scroll rather than fully reflow at narrow sizes.
Advice formatting is passive: no active links, remote images, HTML or scripts.

## Privacy and trust boundary

**Installed locally does not mean inferred locally.** Your agent may send the
reviewed context to its configured cloud provider, charge your account, and
retain prompts/responses or diagnostic logs. Authentication and provider policies
remain the agent's responsibility. Only use trusted agent executables and agent
configuration; CLI permission controls are **not an OS sandbox around the agent
program itself**.

Advice-only performance review does not supply connection profiles or result rows.
The sidebar shares selected values only after explicit opt-in, and trusted mode
creates the scoped private profile described above (including its configured
TLS/SSH paths). Neither mode forwards passwords, query history or a live database
handle. It removes Rowboat/legacy
Keel and conventional database environment variables from child environments.
Authentication-related agent environment remains available to the selected
program. This is not a claim that every arbitrarily named environment secret can
be recognized or that an untrusted program cannot access the user's filesystem.

SQL literals, comments, plan predicates, table definitions/defaults and metric
scope may contain sensitive values. Review and redact them. A captured plan can
include expanded parameter values even though the SQL field uses the template.
Schema metadata and metric samples may be stale; metric counters are not rates.
Server version is currently unknown rather than inferred. The initial schema
section covers only the selected, already loaded table, **not all query relations**.
Result samples, custom-card results, fresh metrics and server-version discovery
are not fetched by AI context preparation.

Rowboat retains analysis context/advice in its owning workspace/viewer memory only, not
its preferences/history/recovery. Opening a suggestion intentionally creates an
ordinary recoverable SQL draft. OpenCode needs a private temporary attachment;
Rowboat uses a 0700 temporary directory and 0600 context file and removes them on
normal completion/cancellation. Agent-owned storage is separate: OpenCode can
retain its own session even after the temporary attachment is removed.

## Adapter restrictions and compatibility

All adapters use argument-vector subprocess execution, separate process groups,
bounded output, deadlines, cancellation and request/session identities. Reviewed
SQL is passed through stdin or a private attachment, never process arguments.
Raw stderr/provider errors are withheld because they can echo sensitive data.
Unknown/incompatible automation output fails rather than falling back to an
unrestricted agent. Tool-action events stop advice-only requests; trusted requests
accept them and show generic activity without rendering tool payloads.

- **Codex:** requires `exec --ignore-user-config`, `--ignore-rules`, `--ephemeral`,
  JSON output and sandbox support. In advice-only mode, user tool/MCP configuration is not
  loaded. Shell, hooks, apps/plugins, browser/computer/image capabilities,
  multi-agent operation, memory and image viewing are disabled for the request.
  This uses the existing Codex authentication but clean session defaults; custom
  user model/provider configuration is not imported. Set a model override when
   needed. No bypass-approval/sandbox flags are used. Advice mode ignores the exact
   nonfatal disabled-Code-Mode-host notice emitted by Codex 0.163.0; actual tool
   events, other item errors and failed turns still stop advice-only requests.
- **Claude Code:** requires **2.1.312 or newer in major version 2**, safe mode, tool restrictions, strict empty MCP
  configuration, no-session-persistence and unattended permission-denial controls.
  Builtin/MCP tools are disabled, hooks are disabled and Chrome is disabled.
  Existing authentication/model settings remain agent-owned.
- **OpenCode V2:** requires private `run --standalone` support. A unique
  advice-only primary agent has all permissions denied and a single model step;
  temporary project policies additionally deny tools and integrations. User and
  managed configuration still follows OpenCode's own precedence rules. This does
  not connect to or stop the user's shared OpenCode service. V1 is not supported.

Trusted Codex requests enable shell tools and the Code Mode host required by
current releases, with workspace-write sandbox mode and network access. The
installed Codex must include its `codex-code-mode-host` helper. Advice-only requests
keep that runtime disabled. Claude requests enable Bash/Read with unattended permission rules;
OpenCode requests use a unique private agent allowing shell/read/external-directory
actions and up to 12 model steps. Other tools/integrations remain disabled where
configured, but shell access can bypass those restrictions. Agent/user/managed
policies can still deny operations. No unrestricted fallback is attempted.

Agent CLI releases evolve independently. Help-capability checks and deterministic
fake-executable tests do not establish successful authentication or live model
behavior. Real-account end-to-end validation is explicit and separate; there is
no automatic request to any installed agent during tests/startup.

An explicit native acceptance run with **Codex CLI 0.163.0** on 2026-10-27 used
the fictional Northstar PostgreSQL launch fixture. From the actual sidebar, Codex
read the embedded CLI guide, inspected schema and executed bounded read-only
queries, then produced a fulfillment briefing and a shipping-priority SELECT.
The generated SELECT was opened in a new SQL tab and manually executed, returning
eight real fixture rows. This verified the required Code Mode host in trusted
mode; it does not establish live compatibility for every agent/version or the
correctness of arbitrary generated advice. No user database was involved.

## CLI

AI commands work on explicit files and never open a database. Package existing
evidence, inspect the generated file, then consent to delegation:

```sh
rowboatctl ai context --engine postgres --sql query.sql --plan plan.json \
  --output context.json
# Review/redact context.json in your editor.
rowboatctl ai check --agent codex
rowboatctl ai analyze --agent codex --context context.json --confirm-share \
  --output advice.md
```

`ai context` optionally accepts `--schema` and `--metrics` files. It does not read
saved profiles or require an agent. `ai check` and `ai analyze` use the shared AI
preferences; explicit `--agent` opts in for that invocation without changing
preferences. `--executable`, `--model`, and `--deadline` provide scoped overrides.
An agent override does not reuse another provider's saved executable/model.

`--connection NAME` only checks that saved connection's AI-disable policy; it does
not connect or automatically establish the provenance of an arbitrary context
file. A disabled connection cannot be bypassed with `--agent`. URL/file database
targets are rejected for AI commands. `--confirm-share` is mandatory even when AI
is enabled in settings. Ctrl+C cancels the owned subprocess. Output files are
created privately without overwriting an existing destination.

Context is bounded to **192 KiB**, advice to **128 KiB**, and subprocess stdout/
stderr to **2 MiB each**, with a **256 KiB** maximum output line. The integration
does not persist investigations, automatically compare plans, apply suggestions,
or schedule analysis. Trusted investigation is instruction-guided v1 behavior,
not a guarantee that an agent cannot perform a mutation.
