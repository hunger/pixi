# pixi_varlink

Varlink IPC server and client used by `pixi serve`. The server is the
daemon-side surface behind `pixi global install --socket <PATH>`,
`pixi global update --socket <PATH>`, and
`pixi global uninstall --socket <PATH>`; clients on the same host
can share the daemon's `data` and `cache` directories, paying the
solve+download cost only once across machines/users.

The wire protocol is built on the [`zlink`](https://crates.io/crates/zlink)
crate (Rust varlink). The handshake (`Hello` / `Authenticate`) binds
each connection to an authenticated client directory; per-call
authorization is rooted in that directory.

## Daemon vs. local: behavioural differences

`pixi global install`, `pixi global update`, and
`pixi global uninstall` produce the same filesystem artefacts
(manifest, exposed mappings, trampolines, shortcuts, completions)
whether run locally or routed through `pixi serve`. A handful of
behaviours diverge by design — they are listed here so users
opting into `--socket` aren't surprised.

### `pixi global uninstall` removes the daemon's prefix too

When `--socket` is set, after the local cleanup (manifest entry,
`~/.pixi/envs/<env>`, trampolines, shortcuts, completions), the
client sends an `Uninstall` RPC so the daemon can `remove_dir_all`
its `<data>/<HASH>/` for that env. The local cleanup is the
authoritative outcome — if the daemon-side removal fails (e.g.
permission error, daemon restarted) the user-visible env is still
gone and the failure surfaces only as a `tracing::warn!`. A
`UninstallFailure::EnvNotFound` from the daemon (the prefix was
already absent — typical when the user installed locally and is
now uninstalling via a daemon that has never seen this env) is
treated as success silently.

### Concurrent same-env install rejected

Two `pixi global install foo` invocations against the same
authenticated client directory and same env name, running at the
same time on the same daemon, are not serialised. The first
acquires a per-`(client, env)` mutex and proceeds; the second
fails fast with `InstallFailure::DuplicateEnvironment` and a
diagnostic naming the env. The client should wait for the first
install to finish and retry — the daemon's fingerprint
short-circuit will turn the retry into a no-op when the first
installed the same record set.

The local install path has no such guard: two concurrent
invocations race on the manifest file. The daemon's behaviour is
strictly more conservative.

### Source-built packages are built client-side

Specs that point at a path, URL, or git repository (e.g.
`pixi global install ./mypkg`, `--with git+https://github.com/...`)
need a build step. The daemon doesn't have one — it has no view of
the client's filesystem (path sources are unreachable) and doesn't
run the client's `BackendOverride` (the in-memory build-backend
mocks tests inject would not match across processes).

The client handles this by running its local
`pixi_command_dispatcher` over each source spec to produce a
`.conda` artefact, then shipping the artefact path along with the
resulting `RepoDataRecord` to the daemon as
`InstallRequest::extra_records`. The daemon extracts each artefact
into its package cache and splices the record into the install
transaction; the source build's runtime deps are folded into
`InstallRequest::specs` so the daemon's solve resolves the binary
closure even though the source specs themselves are not on the
wire.

End-to-end this means the client pays for the source build (the
same cost a local install would incur), and the daemon's shared
cache still benefits multi-machine deployments for the binary
closure. The daemon's server-side defense
(`InstallFailure::UnsupportedSourceSpec`) stays in place to reject
source specs from non-conforming clients that ship them through
`request.specs` instead of building them locally.

### `--force-reinstall` clobbers the local prefix

The end state of `--force-reinstall` is the same on both paths,
but the transient differs and a process running from the env will
notice.

- **Local:** the rattler installer rewrites files in place inside
  the existing prefix at `~/.pixi/envs/<env>`. A binary already
  running from the env keeps using its open file handles; only
  freshly-launched processes see the new content.
- **Daemon:** the client `remove_dir_all`'s `~/.pixi/envs/<env>`
  before localising, the server installs into
  `<data>/<HASH>/`, and the client re-localises afresh. Any
  process running from the env loses its `bin/<exe>` mid-flight;
  exec(3) of the trampoline during the reinstall window will
  transiently fail with `ENOENT` until localisation completes.

If you need an in-place reinstall while a process is using the
env, run `pixi global install --force-reinstall` without
`--socket` (locally).
