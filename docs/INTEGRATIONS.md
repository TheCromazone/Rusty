# Git and repository hosts

Rusty uses Git itself, GitHub CLI (`gh`), GitLab CLI (`glab`), and the Bitbucket
Cloud REST API. There is no extra Git implementation, SDK dependency, token
database or authentication broker. Git SSH keys and credential helpers keep
working. GitHub/GitLab use their existing CLI accounts; Rusty never runs login
implicitly or copies their credentials into its settings.

```sh
rusty --integrations                    # local repository, provider and remote
rusty --integrations setup github       # native authentication instructions
rusty --integrations configure github   # project-specific provider override
rusty --integrations configure gitlab upstream  # self-hosted / explicit base repo
rusty --integrations configure auto origin      # restore host inference
rusty --integrations check              # explicit live authentication check
rusty --integrations prs                # up to 20 open PRs / merge requests
rusty --integrations git status         # no model key required
```

Inside a session, use `/integrations ...` and `/git status|diff|log`. The
configuration is private, scoped to the current project, and contains only
the provider and Git remote name. Choosing `upstream` explicitly keeps a
fork's origin from becoming the review target. Output includes source and
base branches from the host rather than guessing them. Unknown enterprise
hosts need an explicit `github` or `gitlab` configuration. SSH aliases require
a remote whose hostname the host CLI can resolve; no SSH config expansion is
claimed. Bitbucket Data Center has a different API and is not this adapter.

`status` stays offline and reports authentication as **not checked**.
`check` and `prs` contact the selected host only when asked. Missing CLIs,
invalid/expired credentials, deny rules, timeouts and nonzero exits fail
clearly. Native Git views are read-only. Commits, branches, pushes, PR creation,
comments and merges continue through the existing agent shell tools and their
permission checks; these helpers do not grant a new mutation path. Side chats
must return to main before changing integrations. Remote-tool sessions use
remote Git/CLI state; Bitbucket HTTP stays on the controller. Credentials are
not transferred to a sandbox. Deny `integration_read` to block the Bitbucket
read adapter; normal Git/host CLI reads obey their shell deny rules.

For GitHub, follow the printed `gh auth login --hostname HOST` instructions;
`gh auth setup-git --hostname HOST` optionally connects HTTPS Git authentication.
For GitLab, run `glab auth login --hostname HOST` and keep the chosen Git
SSH/HTTPS setup. Account switching and logout remain native CLI commands.
Their storage behavior is owned by those CLIs, including their fallback when
an OS keyring is unavailable.

For Bitbucket Cloud, set **either** `BITBUCKET_ACCESS_TOKEN` (repository,
project or workspace access token), **or** `BITBUCKET_EMAIL` plus
`BITBUCKET_API_TOKEN`. Use your shell or a private `~/.config/rusty/.env`; never
put credentials in remote URLs, chat, flags or project integration settings.
The adapter uses bearer authentication for access tokens and Basic for the
email/API-token pair. Its HTTPS origin is fixed to `api.bitbucket.org`, with
no redirects, retries, token-in-argv, or pagination URL following. Responses
are limited to 1 MiB and requests to 15 seconds. The PR view identifies when
more results are available. Git cloning/pushing still uses Git's credentials.

The unified `cargo xtask qa` gate includes real CLI journeys over temporary Git
repositories and fake native host CLIs: project persistence, fork targeting,
self-hosted GitLab, permission denials, missing Bitbucket credentials, malformed
remotes and authentication/redirect failures. Bitbucket protocol tests use a
local HTTP stand-in, never a real token. A live host/account check remains
separate evidence; offline tests do not establish OAuth or account access.

Additional release journeys to retain: clone from each host; create a branch,
commit and push under the selected policy; create a draft PR/MR against the
correct base; observe CI; handle revoked credentials and branch protection;
verify failed writes were not replayed. Use disposable repositories and record
independent host state. Do not call these qualified from a successful list.

References: [GitHub auth](https://cli.github.com/manual/gh_auth_login),
[GitLab auth](https://docs.gitlab.com/cli/auth/login/), and
[Bitbucket access tokens](https://support.atlassian.com/bitbucket-cloud/docs/using-access-tokens/).

## MCP servers in the same setup surface

```sh
rusty --integrations setup mcp
rusty --integrations mcp list
rusty --integrations mcp add my-server -- /path/to/server 'argument with spaces'
rusty --integrations mcp add github -- --env GITHUB_TOKEN github-mcp-server stdio
rusty --integrations mcp check
rusty --integrations mcp remove my-server
```

The same commands work under `/integrations`. The existing `.mcp.json` (or
`RUSTY_MCP_CONFIG`) remains the only MCP manifest. Add/list never execute a
server; check explicitly starts it. Add saves the server as optional, so a
typo can't stop later sessions (or a teammate's, through a committed
`.mcp.json`); once check passes, set `"required": true` on it by hand if a
session should refuse to start without it. `.mcp.json` is often committed, so
add refuses arguments that look like secrets: `--env VAR` instead saves
`"env": {"VAR": "${VAR}"}`, and the server gets VAR from Rusty's environment
when it starts (a missing variable fails that server's start). Configuration
changes keep the file's existing permissions and serialize concurrent writers across profiles without leaving a repository lock file, obey write permissions, preserve other server entries and environment
values, and refuse accidental replacement or malformed files. The file is
rewritten pretty-printed with keys in sorted order. List shows no
environment values. Changes take effect in the next session. Health checks
fail on any disconnected configured server, including optional ones; the
existing `--mcp-check` required-server contract stays unchanged.

This reuses the current stdio tools client, including pagination, required
tools and destructive-tool confirmation. Direct HTTP/OAuth transport,
resources and prompts are not advertised. A user-selected stdio bridge may
supply remote HTTP/OAuth functionality without another client stack in Rusty.
Configure bridges explicitly; Rusty does not install packages or initiate
authentication merely by listing integrations.

The bounded stdio transport, request deadlines and reader/process cleanup are described in [verification and qualification](VERIFICATION.md).
