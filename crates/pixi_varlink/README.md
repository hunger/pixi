# pixi_varlink

Varlink IPC server and client used by `pixi serve`. The server is the
daemon-side surface behind `pixi global install --socket <PATH>` and
`pixi global update --socket <PATH>`; clients on the same host can
share the daemon's `data` and `cache` directories, paying the
solve+download cost only once across machines/users.

The wire protocol is built on the [`zlink`](https://crates.io/crates/zlink)
crate (Rust varlink). The handshake (`Hello` / `Authenticate`) binds
each connection to an authenticated client directory; per-call
authorization is rooted in that directory.

## Daemon vs. local: behavioural differences

`pixi global install` and `pixi global update` produce the same
filesystem artefacts (manifest, exposed mappings, trampolines,
shortcuts, completions) whether run locally or routed through
`pixi serve`. A handful of behaviours diverge by design — they are
listed here so users opting into `--socket` aren't surprised.

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

### Source-built packages are refused

Specs that point at a path, URL, or git repository (e.g.
`pixi global install ./mypkg`, `--with git+https://github.com/...`)
need a build step. The daemon has no view of the client's
filesystem (so path sources are unresolvable by design) and
doesn't run the client's `BackendOverride` (so URL/git source
builds can't share the test mocks the client side uses), so
attempting to install one through `--socket` is refused upfront
with a miette error pointing the user to drop `--socket`. The
daemon also defends server-side, returning
`InstallFailure::UnsupportedSourceSpec` when any source-typed
`PixiSpec` reaches `run_install` — useful for non-conforming
clients. Source-built packages should be installed locally for
now; supporting them over the daemon is a future-work item.

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
