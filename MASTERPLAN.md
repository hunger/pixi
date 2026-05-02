# `pixi global install` over the varlink daemon

## Context

`pixi serve` (the varlink daemon) currently exposes a demo `Echo` interface
behind the `Hello`/`Authenticate` handshake. We want to push real work through
it, starting with `pixi global install`. The motivation is shared
caches: many machines/users running their own `pixi global install` each
re-download repodata, packages, and re-do source builds. A daemon with shared
`cache` and `data` directories does each install once and lets every client
materialise the result locally, while keeping each client's user-visible
`~/.pixi/{envs,bin,manifests}` byte-indistinguishable from a non-daemon
install.

Architecture, end-to-end:

```
client                                   server (pixi serve)
──────                                   ───────────────────
discover env_root (e.g. ~/.pixi/envs/)
  │
  ├─ Hello { env_root } ─────────────▶  canonicalise + record
  │                                     against canonical FD
  ◀──── HelloReply { challenge } ────
  │
  write challenge file ⇄ Authenticate  read+verify, store dir
  ◀──── AuthReply ───────────────────
  │
  ├─ Install { env_name, specs,
  │           channels, platform,
  │           expose, with,
  │           force_reinstall,
  │           no_shortcuts } ────────▶  HASH = hmac_sha256(salt,
  │                                       env_root || "/" || env_name)
  │                                     install into <data>/<HASH>/envs/<env_name>/
  │                                     using <cache>/{pkgs,repodata,wheels,...}
  ◀── InstallReply { prefix, trampolines } ──
  │
  ln -s <server prefix> <env_root>/<env_name>      (single symlink)
  for each (exe_name, server trampoline path)
    ln -s <server trampoline binary path>  ~/.pixi/bin/<exe>
    ln -s <server trampoline JSON path>    ~/.pixi/bin/trampoline_configuration/<exe>.json
  sync completions               (client follows the env-root symlink)
  update ~/.pixi/manifests/pixi-global.toml
  install OS shortcuts           (client; lives outside ~/.pixi/)
```

The intended outcome: a flag/config opts a `pixi global install` invocation
into the daemon path; the resulting filesystem layout, trampolines, manifest,
and shortcuts are identical to a normal install. Clients running on machines
without `pixi serve` or without the flag are completely unaffected.

## Recommended approach

Eight independently-shippable steps in three feature phases, **gated on
a Phase 0 prep step** that has to land first because the install
machinery is in flight upstream. Each step compiles, leaves the
existing `pixi global install` working, and is end-to-end testable
through the existing `tests` module pattern in
`pixi_varlink/src/lib.rs` plus the `scripts/pixi_serve_loopback`
harness.

---

### Phase 0 — Port install machinery from `pixi_command_dispatcher` to `pixi_compute_engine`

**Why this exists.** `pixi_command_dispatcher` is being retired; the
DAG-style compute primitives in `pixi_compute_engine` are taking over.
Everything Phase A relies on — `CacheDirs`,
`CommandDispatcher`(Builder), `install_pixi_environment`,
`SolvePixiEnvironmentKey` — currently lives in the dispatcher. Before
we can write the server-side install in Phase A Step 2, those items
need to live in `pixi_compute_engine` instead. We're the consumer that
needs them; we own the port for the slice we use.

**Scope.** Phase 0 is the *full* migration of the pieces our feature
relies on, plus every existing caller of those pieces. The dispatcher
disappears from the workspace at the end of this phase (or shrinks to
just the bits no Phase A/B/C step touches; see verification below).
This is **not a mechanical code-move** — this is the chance to drop
dispatcher-isms that don't fit the engine model. Use compute_engine's
existing layout (`engine.rs` / `ctx.rs` / `data.rs` / `cycle/` /
keys-as-types-implementing-`Key`) as the shape; rework the moved
modules to match. Where `pixi_command_dispatcher::install_pixi/ext.rs`
is built around an `impl ComputeCtx` style and the dispatcher's own
`Reporter` plumbing, the engine version should just be `Key`-typed
operations that drop in naturally to `engine.compute(...)`.

The pieces to migrate, named by responsibility (the engine-side
naming is the implementer's call, guided by what reads cleanly in
the compute_engine style):

- The cache-dirs configuration (currently
  `pixi_command_dispatcher::CacheDirs`).
- The dispatcher itself + builder (currently `CommandDispatcher` /
  `CommandDispatcherBuilder`). In the engine model this likely
  collapses into existing `Engine` / `Builder` types — *don't*
  introduce a parallel `CommandDispatcher` type just because that's
  what the dispatcher had.
- Solve and install primitives — `SolvePixiEnvironmentSpec/Key`
  and `InstallPixiEnvironmentSpec/Result/Error` and the `install_pixi`
  fingerprint logic. Likely become engine `Key` impls (one per
  primitive) and live in fresh module names that match
  compute_engine's existing nomenclature.
- The source-build recursion (`SourceBuildKey/Spec`) reachable from
  install. Same treatment.
- The Reporter trait used for progress callbacks. Step 8 needs
  this exposed; either reuse compute_engine's existing
  reporting hook if there is one, or add it now in the engine's
  idiom.
- Anything `pixi_global::Project::install_environment_with_options`
  reaches into transitively (`crates/pixi_global/src/project/mod.rs:578-693`).

**Constraint.** No behavioural change to user-visible commands. The
*shape* of the API in compute_engine can be different — that's the
cleanup. The bytes on disk after `pixi global install foo` must be
identical before and after this phase.

**No backwards-compat shim in `pixi_command_dispatcher`.** All call
sites move to compute_engine in the same Phase 0 commit (or
contiguous commits within Phase 0):

- `crates/pixi_global/src/project/mod.rs`
- `crates/pixi_cli/src/publish.rs`
- `crates/pixi_cli/src/build.rs`
- `crates/pixi_cli/src/run.rs`
- and any other workspace consumer that today imports from
  `pixi_command_dispatcher`. Run
  `grep -rn pixi_command_dispatcher crates/` to enumerate.

**Files to touch (by responsibility):**
- `crates/pixi_compute_engine/src/` — gains the new modules. Module
  layout follows the engine's existing conventions, *not* the
  dispatcher's. Where the dispatcher had `install_pixi/ext.rs +
  fingerprint.rs + spec.rs`, the engine version might be a single
  `install.rs` plus a `Key` impl on the spec type — whatever reads
  naturally inside `engine.rs` + neighbours.
- `crates/pixi_command_dispatcher/` — code is removed in this
  phase, not re-exported. After Phase 0 it ideally contains nothing
  Phase A reaches for; if other dispatcher-only functionality
  remains for other callers we don't yet need, that's fine, but
  there are no `pub use` shims to ease migration. All callers we
  touch are updated.
- All workspace consumers of the moved APIs — `Cargo.toml`
  dependency lists swap `pixi_command_dispatcher` →
  `pixi_compute_engine` for the migrated pieces.
- `Cargo.toml` root — remove `pixi_compute_engine` from
  `[workspace.metadata.cargo-shear].ignored` since it now has real
  consumers.

**Test invariant.** This is the load-bearing rule for Phase 0,
split by where the test lives:

> **Tests outside `crates/pixi_command_dispatcher/` are not edited
> at all.** Period. They pass unchanged.
>
> **Tests inside `crates/pixi_command_dispatcher/` that exercise
> *functionality* — i.e. user-visible behaviour — are kept alive
> by re-testing the same functionality against the new
> `pixi_compute_engine` API.** Tests of *internals* (private
> mechanics of the dispatcher's plumbing that won't exist in the
> same shape after the port) can be dropped without replacement.

In practice this means:

- **Outside the dispatcher (`pixi_global`, `pixi_cli`, top-level
  `tests/`, every other crate):** zero test edits. Not import
  lines, not formatting, nothing. The higher-level APIs they call
  (`pixi_global::Project::install_environment_with_options`,
  `pixi global install` end-to-end, etc.) keep their exact surface
  and behaviour, so these tests keep passing as a free
  consequence. If any test outside the dispatcher imports from
  `pixi_command_dispatcher` directly, that's an internal leak we
  inherited — keep that one symbol re-exported from
  `pixi_command_dispatcher` (or alias it via `pub use`) so the
  test compiles untouched. The cleanup of those re-exports is a
  follow-up, not Phase 0.
- **Inside the dispatcher, functionality tests:** identify each
  test that asserts user-visible behaviour — solve produces these
  records, install lays down these files on disk, fingerprints
  match across re-installs, source-build recursion produces these
  artefacts, etc. For each one, write the equivalent test against
  the new compute_engine API in `crates/pixi_compute_engine/` (or
  alongside the engine's existing test conventions). Same
  *functionality* asserted, possibly different setup/imports.
  Once the engine-side test is in place and green, delete the
  original.
- **Inside the dispatcher, internals tests:** tests that poke at
  private dispatcher state, exercise a `CommandDispatcherBuilder`
  field, assert ordering of internal callbacks, or otherwise probe
  mechanics that the engine port intentionally reshapes — these
  can be dropped. They were testing the old shape; the new shape
  has its own internal tests where appropriate. Don't fabricate
  parallel tests just to preserve count.

**Pass criteria:**
- `cargo test --workspace --all-targets` succeeds.
- Every test outside `pixi_command_dispatcher/` is byte-identical
  to its pre-Phase-0 form and passes.
- For every functionality previously exercised by a dispatcher
  test, there is a corresponding test in compute_engine asserting
  the same user-visible behaviour.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- If `pixi_command_dispatcher` is now empty or near-empty, drop it
  from the workspace entirely in this phase rather than leaving a
  hollow crate behind.

**Why this is the right gate.** Building Phase A directly against an
engine API we've shaped to read well saves a second cleanup pass
later. If we built on the dispatcher and tracked through a
re-export shim, every Phase A reviewer would see two layers of
plumbing. Bake the cleanup in now.

---

### Phase A — Wire skeleton (no real install yet)

#### Step 1. Minimum viable Install RPC: dummy server

**Goal.** After handshake, client can call `Install { env_name, … }` and the
server returns `InstallReply { prefix: String }` computed deterministically as
`<data>/<HASH>/envs/<env_name>`. **No package fetched, no prefix written.**
Proves the entire wire pipeline (config → keyed hash → response → client
unwrap) end-to-end.

Locked-in scaffolding:
- New varlink interface `dev.prefix.pixi.GlobalInstall`. Echo stays separate.
- Auth state extracted from `EchoService` into a shared `AuthCore`
  (`Arc<Mutex<HashMap<usize, AuthState>>>` + `Hello`/`Authenticate` impls).
  Both `EchoService` and a new `GlobalInstallService` embed `Arc<AuthCore>`,
  so one handshake covers all interfaces on the same connection.
- Wire types: `InstallRequest`, `ExposeMapping`, `InstallReply`,
  `GlobalInstallError { NotAuthenticated, ServerNotConfigured,
  InvalidEnvName, InstallFailed }`.
- `pixi_config::RemoteConfig` gets three new fields, kebab-case,
  `skip_serializing_if = "Option::is_none"`, merged like the existing two:
  `data: Option<PathBuf>`, `cache: Option<PathBuf>`, `salt: Option<String>`
  (hex-encoded 16 bytes; absent → all-zero).
- `pixi serve` gets matching subcommand flags `--data <PATH>`,
  `--cache <PATH>`, `--salt <HEX>`. CLI overrides config the same way
  `--socket` does today. **`data` and `cache` are mandatory**: if either
  is missing after CLI+config resolution, `pixi serve` refuses to start
  with a clear error naming the missing key. There is no echo-only
  fallback. `salt` is optional and defaults to a 16-byte all-zero key
  when unset.
- `scripts/pixi_serve_loopback`: when running, automatically creates
  `<tmp>/data/` and `<tmp>/cache/` alongside the existing socket, and
  auto-injects `--data <tmp>/data --cache <tmp>/cache` into the
  `pixi serve` invocation (after `serve`, before any user-supplied
  client args). Skip injection if the caller already provided the flag
  via server-extra-args. Add `--salt <HEX>` injection too with a default
  fixed value (e.g. all-zero) so test hashes are deterministic. End
  result: every loopback invocation has a self-contained, unique cache
  and data directory under its own tempdir, garbage-collected on exit.
- Hash helper: `env_hash(salt: &[u8;16], auth_path: &Path, env_name: &str)
  -> String` — HMAC-SHA-256 with `salt` as the key, message
  `<auth_path>/<env_name>`. Output: 32 bytes hex-lowercase, 64 chars.
  The hash uniquely identifies a (client env_root, env name) pair, so
  two different env names from the same client get different prefixes.
  Add `hmac` + `sha2` as direct deps on `pixi_varlink`; both are
  already transitively in the tree via `reqwest` / `rustls`.
- The on-disk install path is `<data>/<HASH>/` directly — no inner
  `envs/<env_name>/` segment, since the env name is already encoded in
  HASH. The server therefore won't go through `pixi_global::Project`
  (whose prefix construction is `<env_root>/<env_name>`); instead it
  builds `InstallPixiEnvironmentSpec` directly with `prefix =
  Prefix::create(<data>/<HASH>)` and calls
  `compute_engine.install_pixi_environment(spec)` — using the
  engine's API after Phase 0's port. This also drops the per-HASH
  server-side manifest the previous draft mentioned — there's no
  client-visible manifest to maintain, since the client owns its own.
- `Install` is declared `#[zlink(more)]` from day one even though step 1
  emits only a single terminal `Result` — leaves room for streaming progress
  in step 7 without a wire-incompatible schema change.
- Strict env-name validator (`InvalidEnvName` error) rejects anything that
  isn't `EnvironmentName`-shaped. Closes the only auth-granularity gap.
- Server takes an **exclusive `flock`** on both `<data>/.pixi-serve.lock`
  and `<cache>/.pixi-serve.lock` at startup, non-blocking
  (`try_lock_exclusive`). If either lock is already held — by another
  `pixi serve` instance, or any external locker — the daemon refuses to
  start, with an error message that names the **full absolute path** of
  the contested lock file. The locks are held by file handles owned by
  the running server and released automatically on process exit.
  Use `fs2::FileExt::try_lock_exclusive` (or the workspace-existing
  `async-fd-lock` if that's already a dep — verify during impl).

**Files:**
- `crates/pixi_varlink/src/lib.rs` — extract `auth.rs`; export
  `serve_with_install(socket, server_config)` alongside the existing `serve`.
- `crates/pixi_varlink/src/auth.rs` (new).
- `crates/pixi_varlink/src/install.rs` (new) — service, request/reply types,
  `env_hash` helper.
- `crates/pixi_varlink/Cargo.toml` — add `hmac` and `sha2`.
- `crates/pixi_config/src/lib.rs` — extend `RemoteConfig`; merge + snapshot
  update at `crates/pixi_config/src/snapshots/pixi_config__tests__config_merge_multiple.snap`.
- `crates/pixi_cli/src/serve.rs` — add `--data`, `--cache`, `--salt`
  flags to `serve::Args`; resolve final values via CLI > config; when
  both `data` and `cache` end up set, dispatch to
  `serve_with_install`, otherwise old `serve` (echo only).
- `crates/pixi_cli/src/serve_test.rs` — new `install-dry-run` subcommand
  hitting the new RPC for loopback testing.
- `scripts/pixi_serve_loopback` — `mkdir -p $tmp/data $tmp/cache`;
  auto-inject `--data $tmp/data --cache $tmp/cache --salt <fixed>` into
  the spawned `pixi serve` unless the user already supplied them. Update
  the script's docstring to document the new behaviour.

**Tests:**
- `crates/pixi_varlink/src/install.rs::tests` — spin up `serve_with_install`
  against tempdirs for `data`/`cache`, complete the handshake, send
  `Install`, assert returned prefix equals `<data>/<expected hex>/envs/foo`
  for spelled-out salt + path. Property tests for hash determinism and
  salt/path sensitivity.
- `RemoteConfig::is_default()` round-trip and snapshot still pass when the
  new fields are absent.
- Unit test in `crates/pixi_cli/src/serve.rs` that confirms `--data /a
  --cache /b --salt 00…` resolves correctly (CLI > config) and that the
  daemon refuses to start in install-capable mode when only one of
  `data`/`cache` is set.
- Lock test: spawn one `pixi serve` against `<tmp>/data` and `<tmp>/cache`,
  let it acquire the locks, spawn a second one with the same paths and
  assert it exits non-zero with an error containing the absolute path of
  whichever lock was contested. The loopback script's auto-isolation per
  tempdir means real-world test runs never trip this; the test is for the
  diagnostic itself.
- Loopback: `PIXI=./target/debug/pixi scripts/pixi_serve_loopback -- \
  serve-test install-dry-run` works with no extra setup — the script
  auto-creates `data`/`cache` dirs in its own tempdir and passes them
  through. The printed prefix is under that data dir.
- Loopback override: passing `-- --cache /persistent ...` on the
  loopback command line skips auto-injection of `--cache` and uses the
  caller's path instead (useful for cache-reuse tests in later steps).

#### Step 2. Real install body

**Goal.** Server actually fetches and installs into
`<data>/<HASH>/envs/<env_name>/`, using `<cache>` for repodata/packages/wheels
/source-build artifacts. Client still just receives and prints the prefix.

**Files:**
- `crates/pixi_global/src/common.rs` — promote `EnvRoot::new`/`BinDir::new`
  from `#[cfg(test)]` to `pub` (or add `pub from_root(root: PathBuf)`).
- `crates/pixi_global/src/project/mod.rs` — add `with_cache_root(self,
  PathBuf)` builder; the engine-context construction (formerly
  `command_dispatcher()`, after Phase 0 the `pixi_compute_engine`
  equivalent) currently hard-codes the cache root, make it
  overridable. Default behaviour preserved when unset.
- `crates/pixi_varlink/src/install.rs` — real install body:
  1. Solve specs into records using
     `engine.compute(SolvePixiEnvironmentKey::new(...))` — the
     ported compute engine API from Phase 0 — with the configured
     `<cache>` as cache root and `Limits`/concurrency from the
     daemon config.
  2. Build `InstallPixiEnvironmentSpec` with `prefix =
     Prefix::create(<data>/<HASH>)` and the solved records, then
     call `engine.install_pixi_environment(spec)` (Phase 0 ported
     primitive). **No `pixi_global::Project`, no manifest** — the
     client owns its own manifest; the server only produces a prefix
     on disk.
  3. Generate trampolines for the requested `expose` mappings server
     side: write the trampoline binary (or its existing per-bin
     hardlink) into `<data>/<HASH>/.trampoline/<exe>` and the
     matching JSON into `<data>/<HASH>/.trampoline/trampoline_configuration/<exe>.json`.
     The JSON's paths reference the *client's* symlinked location:
     `CONDA_PREFIX = <auth_path>/<env_name>`,
     `original_executable = <auth_path>/<env_name>/bin/<real-name>`.
     The auth path is the directory recorded in the connection's
     `Authenticated` state — server already has it.
     Resolved during step 2 implementation: pixi's trampoline binary
     calls `current_exe().canonicalize()` which on Linux reads
     `/proc/self/exe` (the canonical target) and on macOS canonicalises
     `_NSGetExecutablePath`'s result. So invoking the trampoline
     through the client's `~/.pixi/bin/<exe>` symlink resolves to
     `<data>/<HASH>/.trampoline/<exe>` and the sibling-JSON lookup
     lands in `<data>/<HASH>/.trampoline/trampoline_configuration/<exe>.json`.
     **Server-side JSONs work as-is — no extra symlink layer under
     `~/.pixi/bin/trampoline_configuration/` is needed.**
  4. Reply with `InstallReply { prefix }`. Trampoline locations are
     deterministic from `prefix` + each `ExposeMapping.exe_name`
     (`<prefix>/.trampoline/<exe_name>` and
     `<prefix>/.trampoline/trampoline_configuration/<exe_name>.json`,
     with a `.exe` suffix on Windows), so the client derives them
     itself rather than the server returning a redundant
     `Vec<TrampolineEntry>`.
- `crates/pixi_cli/src/serve_test.rs` — promote `install-dry-run` to
  `install [--inspect]`; `--inspect` asserts
  `<prefix>/conda-meta/.pixi-environment-fingerprint` exists.

**Tests:**
- New unit test: install a tiny package (e.g. `xz`) end-to-end through the
  daemon, assert fingerprint file exists.
- Loopback: `serve-test install --inspect xz` succeeds.
- Regression: ensure `pixi global install` (non-daemon) is unchanged. Add
  a unit test in `crates/pixi_cli/src/global/install.rs` that pins the
  prefix path computation.

#### Step 3. Concurrency & idempotency hardening

**Goal.** Concurrent `Install` calls against the same HASH fail fast on
the second one with `InstallFailure::DuplicateEnvironment` — silent
serialisation would leave a user running `pixi global install foo`
from two terminals at once blocked without any explanation. Sequential
retries after the first install completes acquire cleanly and hit the
engine's `EnvironmentFingerprint::read` short-circuit so the same
prefix is returned without re-running the rattler installer. Since
HASH already encodes both the auth path and the env name, locking is
simply per-HASH — no need to compose a separate (HASH, env_name) key.

**Files:**
- `crates/pixi_varlink/src/install.rs` — per-HASH `tokio::sync::Mutex`
  map on `ServerConfig`; `run_install` calls `try_lock_owned` and
  surfaces `DuplicateEnvironment` on contention. In-memory only; no
  persistence.
- Existing `Project::environment_in_sync_internal` (project/mod.rs:235)
  short-circuits idempotent installs — exercised via the regular path.

**Tests:**
- Unit test on the lock-map semantics: same hash → same Arc'd mutex,
  different hashes → distinct Arcs, held guard makes the second
  `try_lock` fail (which is what surfaces as `DuplicateEnvironment`),
  released guard lets a sequential retry acquire cleanly. Live
  concurrent two-client testing is timing-sensitive (small installs
  finish before the second connect+handshake completes), so the unit
  test plus design review is the verification.

---

### Phase B — Client-side localisation

#### Step 4. Single-symlink localisation

**Goal.** A standalone helper
`pixi_global::localise_prefix(server_prefix, local_path)` that creates a
**single symlink** at `local_path` pointing at `server_prefix`. Nothing
walks; nothing recurses. From `~/.pixi/envs/foo` the path resolves
through the symlink to `<data>/<HASH>/envs/foo` for every read.

This is the minimum we can ship. Trampolines bake the *local* path
`<env_root>/<env_name>` as `CONDA_PREFIX`; the kernel follows the
symlink whenever something reads through it. Cost: deleting the server
prefix breaks the client; the reflink-copy mode in Step 7 removes that
dependency.

**Auth granularity confirmation.** The directory we authenticate
against is the *parent* of where the env will live, i.e.
`EnvRoot::from_env()` (`~/.pixi/envs/`) — already what Step 1 set up.
This is the dir we'll be writing the symlink into, so it's the
correct auth target.

**Files:**
- `crates/pixi_global/src/localise.rs` (new). No new deps; just
  `tokio::fs::symlink`.

**Tests:**
- Unit tests: build a fake "server prefix" dir with a couple of
  files; call `localise_prefix(server_dir, local_dir.join("foo"))`;
  assert `local_dir/foo` is a symlink; assert
  `tokio::fs::read(local_dir.join("foo").join("some-file")).await`
  yields the server file's bytes (via symlink resolution).
- Idempotency: a second call replaces the existing symlink (or no-ops
  if target matches — pick one, document, test).
- Stale-target replacement: if `local_path` already exists as a
  symlink to somewhere else (a previous install at a different HASH),
  the helper rewrites it to the new target.
- Conflict: if `local_path` already exists as a *real directory*
  (e.g. user has run a non-daemon install previously), refuse with a
  clear error rather than silently overwriting. Add a follow-up TODO
  for "migrate dir → symlink" once we have a need.

#### Step 5. Extract the client-side post-install tail (no trampolines)

**Goal.** Refactor `pixi_global::Project::setup_environment` so the
*client-side* post-install bits — completions, OS shortcuts, manifest
save — are callable independently of having just run the install.
**Trampolines are not in this tail**: they're produced server-side
(Step 2) and symlinked over by the client (Step 6). The local code
path keeps using the existing combined function and is unaffected.

**Files:**
- `crates/pixi_global/src/project/mod.rs` — extract
  `Project::finalise_environment_no_trampolines(&self, env_name, args,
  specs) -> miette::Result<StateChanges>` covering `sync_completions`,
  `sync_shortcuts`, and `manifest.save()`. Skip
  `expose_executables_from_environment` / `create_executable_trampolines`
  — those are the trampoline step.
- `crates/pixi_cli/src/global/install.rs` — keep the existing
  `setup_environment` unchanged for the local code path; it still
  invokes the full install + tail-with-trampolines.

**Tests:**
- Unit test against a manually staged prefix (or a symlink to one):
  call `Project::finalise_environment_no_trampolines`. Assert manifest
  gets a new env entry; assert NO files appear under `bin_dir`. Then
  call the regular `setup_environment` and assert trampolines DO appear
  — to lock in that the split is meaningful.
- Existing `pixi global install` integration tests pass unchanged
  (the local path's combined function isn't touched).

#### Step 6. End-to-end: `pixi global install` over the daemon

**Goal.** When `--socket <PATH>` is supplied (CLI flag, or
`remote.socket` from config), `pixi global install` routes through the
daemon — connect, handshake against `EnvRoot::from_env()`, send
`Install`, localise the resulting prefix (Step 4 mode), run the local
tail (Step 5). When the socket is absent, behaviour is exactly as
today: local install, byte-identical. **No new flag, env var, or
config knob** — the existing global `--socket` *is* the trigger,
mirroring how `pixi serve` itself decides between binding a path and
using systemd activation. One mental model for the whole feature.

**Files:**
- `crates/pixi_cli/src/global/install.rs` — top-level branch in
  `execute`: if `global_options.socket.is_some()` (or
  `Config::load_global().remote.socket.is_some()`), build an
  `InstallRequest` from `args`, `connect()` to that socket against
  `EnvRoot::from_env()`, call `Install`, receive
  `InstallReply { prefix }`, then locally:
  1. `localise_prefix(server_prefix, env_root.join(env_name))` —
     single symlink (Step 4).
  2. For each `ExposeMapping` the client itself sent in the request,
     symlink `~/.pixi/bin/<exe_name>` to
     `<server_prefix>/.trampoline/<exe_name>` (with `.exe` on Windows).
     The trampoline canonicalises `current_exe()` at runtime so it
     finds its sibling JSON on the server side — no
     `~/.pixi/bin/trampoline_configuration/` symlinks needed.
     (Removing stale symlinks first if they already exist.)
  3. `Project::finalise_environment_no_trampolines` for completions
     + manifest + OS shortcuts.

  Otherwise fall through to today's combined `setup_environment` for
  a fully-local install.
- `crates/pixi_varlink/src/install.rs` — client-side helper module:
  `Connection::install(req)` mirroring `Connection::ping`'s shape behind
  the typestate.
- `crates/pixi_cli/src/lib.rs` — extend the `--socket` panic guard to
  permit `Command::Global` alongside `serve` / `serve-test` (currently
  only those two pass the guard).

**Tests:**
- New integration test under `tests/`: spin up `pixi serve` in-process
  against tempdirs, run `pixi global install --socket <sock> xz` with a
  tempdir env_root. Assert the layout matches a no-`--socket` install
  (manifest entries, `bin/xz` trampoline, `envs/xz/conda-meta/...`).
  Run the trampoline and check `--version` output.
- Loopback: `scripts/pixi_serve_loopback -vvv ... -- global install xz`
  end-to-end (the loopback already injects `--socket` into the client
  invocation); exit 0; output diffs cleanly against a local install of
  the same package.
- Regression: an explicit `pixi global install xz` with neither
  `--socket` on the CLI nor `remote.socket` in config takes the
  original code path and produces the original layout. Pin via a unit
  test on the dispatch branch in `crates/pixi_cli/src/global/install.rs`.

---

### Phase C — Polish

#### Step 7. Reflink / copy localisation mode

**Goal.** A second `localise_prefix` mode that walks the full server tree
and reflink-copies every regular file (falling back to plain copy on
non-CoW filesystems), reproducing symlinks with appropriate retargeting
(in-prefix absolute symlinks rewritten from `<server>/…` to `<local>/…`;
out-of-prefix symlinks preserved verbatim). After this step the local
prefix is fully self-contained — server `data/` can be GC'd without
breaking clients.

Pick the mode via priority `--localise-mode <mode>` flag > env var
`PIXI_GLOBAL_LOCALISE` > `[remote] localise = "..."` config. Default
remains `symlink` until we have run-time evidence reflink works on the
mainstream filesystems we care about; then flip the default to reflink in
a follow-up commit.

**Files:**
- `crates/pixi_global/src/localise.rs` — add `Mode { Symlink, Reflink,
  Copy }` and the reflink+copy walk.
- `crates/pixi_global/Cargo.toml` — promote `reflink-copy` from
  transitive (`Cargo.lock` 0.1.29) to direct dep; add `walkdir`
  if not transitively reachable.

**Tests:**
- Unit test on a hand-built fake prefix: regular files, in-prefix and
  out-of-prefix absolute symlinks, relative symlinks. Assert per-mode
  expectations (symlink: top-level only; reflink: every file is a real
  inode, content equal to source; copy: same as reflink but no shared
  extents). Symlink retargeting verified by reading
  `lib/python3.X/site-packages/<thing>` through the localised prefix.
- Optional smoke test gated behind a `PIXI_TEST_REFLINK_FS=/btrfs/path`
  env var that exercises `du`-based extent sharing on a real CoW
  filesystem.

#### Step 8. Streaming progress on `Install` — with rendering parity

**Goal.** Wire `pixi_compute_engine`'s progress callbacks (post-Phase
0) into `ProgressEvent`s on the streaming `Install` RPC, **and**
route the client-side rendering through the *same* indicatif renderer
as a local install — so the user sees byte-for-byte equivalent
progress output whether the install ran locally or via the daemon.

The key design constraint is that the renderer is unified, not
duplicated. Concretely:

```
local path:  pixi_compute_engine     ──┐
                                       ▼
remote path: server engine → wire  ──▶  shared `ProgressRenderer`
             ProgressEvent ─ client    (drives indicatif via
             reverse-Reporter ──┘       global_multi_progress())
```

Both event sources land in the same `ProgressRenderer` on the client,
which converts a normalised event stream into indicatif bar updates.
The local path's rendering code is refactored (in this step) to also
go through `ProgressRenderer`, so it can't drift from the remote
rendering.

`Install` was already declared `#[zlink(more)]` in Step 1, so adding
events is a non-breaking schema change.

**Files:**
- `crates/pixi_compute_engine` — confirm/expose the existing
  Reporter trait so both sides can attach one. (Likely already
  ported alongside the install primitive in Phase 0; locate during
  impl.)
- `crates/pixi_varlink/src/install.rs` — server attaches an outbound
  Reporter that funnels callbacks into
  `InstallProgress::Progress { event }`, terminating with
  `InstallProgress::Result`.
- `crates/pixi_global/src/project/mod.rs` (or a sibling module) — a
  new `ProgressRenderer { mp: MultiProgress, bars: HashMap<…> }`
  that takes a `ProgressEvent` and applies it to indicatif. Both the
  local consumer (the engine Reporter wrapped to emit
  `ProgressEvent`s) and the remote consumer (varlink stream →
  events → renderer) feed the same instance.
- `crates/pixi_cli/src/global/install.rs` — both code paths now
  install a `ProgressRenderer` and let it drive indicatif. The local
  path stops using the engine's reporter directly.

**Tests:**

Two layers — basic "events arrive" smoke test, plus a fixture-driven
rendering-parity test that doesn't require running a real install on
every run:

- Smoke: streaming test mirroring `round_trip_ping::long_ping`;
  assert at least `Started`, one `Advance`, `Finished` arrive before
  `Result`.

- **Render-parity via recorded fixture.** The contract we want to
  pin: "given progress event sequence X, the client renders terminal
  output Y." Fixture files, both checked into the repo:

  - `tests/fixtures/install_progress.events.json` — a recorded
    `ProgressEvent` sequence from a real local install.
  - `tests/fixtures/install_progress.expected.txt` — the terminal
    transcript the renderer produces for that sequence (after
    normalisation: strip durations, byte-rate counters, throughput
    values, anything else timing-dependent; replace with placeholder
    tokens).

  Test body: stand up a **fake server** (in-process, in test code)
  whose `Install` handler replays the fixture's events to the client
  and then closes. Run the daemon-routed install path against this
  fake server, capture the client's indicatif output through a
  buffer-backed `TermLike`, normalise, assert byte-equality against
  `expected.txt`.

  Crucially: the test never runs a real install. No network, no
  package cache, no platform dependency on filesystem CoW or shell
  trampolines. The events come from a JSON file; the renderer is the
  thing under test. CI runs in milliseconds.

- **Update mechanism.** A small dev tool —
  `cargo run -p pixi_varlink_dev_tools --bin update-progress-fixture`
  (name TBD) — that:

  1. Runs an actual local `pixi global install` against a tempdir
     env_root and a tempdir cache, with a recording Reporter
     attached at the dispatcher.
  2. The recorder writes the captured `ProgressEvent` sequence to
     `events.json`.
  3. After the install, the recorded sequence is then replayed
     through the same `ProgressRenderer` with a buffer-backed
     `TermLike`, and the normalised transcript is written to
     `expected.txt`.

  Step 3 is the "translate synthetic events to expected terminal"
  step the user described. By generating the expected output *from
  the same renderer that the test exercises*, we guarantee the
  fixture is internally consistent — when the renderer changes
  legitimately, both files update together.

  Run the script, check the diff, commit if happy. CI never runs the
  generator; only the asserting test.

The parity test is what locks in the contract: any future change
that perturbs the renderer (a new bar style, a different layout, a
field reordering on `ProgressEvent`) trips the snapshot diff
immediately. Run the update script, eyeball the new transcript, and
commit. Drift becomes a deliberate, reviewable act.

---

## Decisions locked in

These were open questions in earlier drafts; recording the answers
inline so they don't need re-discussion during implementation.

- **Hash function.** HMAC-SHA-256, keyed by the salt, message
  `<auth_path>/<env_name>`. Both `hmac` and `sha2` are already
  transitively in the workspace. Output: 32 bytes hex-lowercase.
- **Localise default.** Long-term default is reflink-copy with a
  per-file copy fallback; symlink mode stays selectable via config
  (`[remote] localise-mode = "reflink" | "symlink" | "copy"`).
  Implementation order: ship symlink-only first (Step 4) so we have
  a working `pixi global install` over the daemon; add the reflink
  mode in Step 7; flip the default to reflink in Step 7's commit (or
  the immediate follow-up) once it's been exercised in CI.
- **Manifest source-of-truth.** The client's
  `~/.pixi/manifests/pixi-global.toml` is the user-facing truth. The
  server doesn't keep a manifest of its own — it produces a prefix
  for each `Install` request from request contents alone, so drift
  between client and server is by construction impossible.
- **Streaming progress timing.** Defer to Step 8 (polish phase); v1
  `Install` is request → reply with no intermediate events. The
  fixture-based parity testing (Step 8 details) keeps the local and
  remote rendering paths from drifting once streaming lands.

## Critical files

- `crates/pixi_varlink/src/lib.rs`
- `crates/pixi_varlink/src/auth.rs` (new)
- `crates/pixi_varlink/src/install.rs` (new)
- `crates/pixi_varlink/Cargo.toml`
- `crates/pixi_config/src/lib.rs` + snapshot file
- `crates/pixi_cli/src/serve.rs`
- `crates/pixi_cli/src/serve_test.rs`
- `crates/pixi_cli/src/global/install.rs`
- `crates/pixi_cli/src/lib.rs` (panic-guard widening for `--socket`)
- `crates/pixi_global/src/common.rs` (promote `EnvRoot::new`/`BinDir::new`)
- `crates/pixi_global/src/project/mod.rs` (`with_cache_root`;
  `finalise_environment_no_trampolines` extraction; reuse of the
  trampoline-creation primitive at a custom directory for the server
  side)
- `crates/pixi_global/src/localise.rs` (new)
- `crates/pixi_global/Cargo.toml`
- `tests/` (top-level integration test for `--remote`)

## Verification

End-to-end:
- `cargo test -p pixi_varlink` — every step adds at least one test in this
  crate.
- `cargo test -p pixi_global` — steps 4-5 add localise + finalise tests.
- `cargo test -p pixi_cli` — steps 6+ add CLI integration tests.
- `scripts/pixi_serve_loopback -- serve-test install xz` — manual smoke
  test after step 2.
- `scripts/pixi_serve_loopback -- global install --remote xz` after step 6,
  followed by running `~/.pixi/bin/xz --version`.
- After step 6: a `diff -ur` of `~/.pixi/{envs,bin,manifests}` between a
  remote-install and a local-install of the same packages, modulo file
  timestamps, should be empty.

Each step also runs `cargo clippy --workspace --all-targets -- -D warnings`
and the project lint tasks per the standard cleanup pass.
