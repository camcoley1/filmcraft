# ADR 0002: AI features through an optional local helper process (`craft-ai`)

- **Status:** accepted for this fork (2026-10-28, approved by the project owner; recorded as a fork
  exception to `CLAUDE.md` "Pure Rust")
- **Milestone:** M6 (sidecar AI features); M6.0 is the foundation this record describes

## Context

Tracked reframing, stem separation, speech enhancement, beat detection, semantic search, upscaling
and frame interpolation all rest on neural models whose reference implementations, weights and
runtimes are Python (PyTorch, ONNX Runtime). Porting each model to pure Rust is months of work per
model and would freeze us on today's versions. FilmCraft itself must stay pure Rust: the product
we build and ship is unchanged.

## Decision

AI features run in **`craft-ai`**, a separate Python process installed by craft-suite
(`craft-suite/sidecar/`, a `uv` project pinned to Python 3.12). FilmCraft is its client.

1. **The app never bundles, installs or spawns Python.** The helper is started by the craft-suite
   launcher or by hand (`craft-suite/sidecar/run.sh`). FilmCraft only connects to it.
2. **Loopback JSON lines, with a token.** The helper listens on `127.0.0.1` (default port 9877).
   Every request is one JSON object on one line, on a fresh TCP connection, answered by one line,
   all within one deadline (a silent or trickling peer fails at it). The protocol table is in
   `crates/sidecar/src/lib.rs`: `hello`, `job.start`, `job.status`, `job.cancel`, and an `error`
   reply for any of them. Progress is polled (`job.status`) rather than streamed, so a helper
   restart is just a refused connection. Each launch writes a random token to a user-only file
   (`~/Library/Application Support/craft-ai/token` on macOS, `CRAFT_AI_TOKEN_FILE` to override)
   and refuses requests without it, so other local users and programs cannot run jobs through
   it; job ids are random.
3. **Handshake with a protocol version.** `hello` returns `{protocol, version, features[]}`. A helper
   speaking another protocol is refused: every `ai.*` command stays disabled with "AI helper is
   version X (protocol N); this FilmCraft needs protocol M".
4. **Disabled with a reason, never missing.** With the `sidecar` feature on, the `ai.*` commands
   exist whether or not a helper runs. Without a helper, or when the helper lacks the feature, each
   is disabled with a reason (exactly like `transcript.generate` without Whisper, `NO_SPEECH` in
   `crates/engine/src/transcript.rs`). `ai.status` probes the helper and `ai.connect {port}` picks
   another port. `enabled()` only reads the cached handshake.
5. **No helper I/O on the UI thread.** In the desktop app (`AiState::background`) every handshake
   runs on a worker thread and `wait: true` is ignored; jobs always run on their own thread. Agents
   that want to block get it from the automation layer, which runs `ai.*` with `wait` as a polled
   job (`LONG_COMMANDS`), as it does exports; a headless host applies finished results each time
   it takes the session (`Session::poll_persistence` before every command), as the app does every
   frame. The CLI and tests may block. A waited job whose result is held by a gesture replies
   `{"applied": false, "pending": true}`. A running job's folder carries a heartbeat file, so
   another FilmCraft's cache pruning leaves it alone.
6. **Bounded, cancellable jobs.** Every job runs as an engine `Job`: progress 0 to 1 in the Events
   panel and `jobs.list`, cancelled by `jobs.cancel` (the helper is told to stop). The client caps
   reply size, result file count and total result size, and a per-job wall-clock timeout; a helper
   that stops answering fails the job. Inputs are bounded by each feature (duration and size caps;
   long audio is processed in bounded chunks). The helper runs a fixed number of jobs with a
   bounded queue and refuses more with an error.
7. **Results are files; the engine applies them.** The helper writes result files into the job's
   folder in the project's AI cache, `<media cache>/AI/<project id>/<job>/`; it never writes the
   project. The engine checks the files (inside the folder, regular files and not links, within
   the size cap) and reads them through `open_result`, which checks again on the open handle. The
   cache is kept under a size cap (default 32 GiB): starting a job deletes the oldest finished job
   folders, never one still running or waiting to be applied, nor one changed in the last ten
   minutes.
8. **Stale results are dropped, not applied, and the job says so.** A job records a fingerprint
   when it starts: the session's project generation (bumped by New / Open / Close / Recover /
   New from Template), the project id, the sequence settings, and for each target clip the whole
   clip (placement, source range, speed, effects, enabled) and the media it shows (file, subclip
   range, interpretation). A job stays unfinished until its result is applied. A changed
   fingerprint (an edit, a delete, an undo, a locked target track, another project) fails the job
   with the reason ("result kept in cache"); `wait: true` returns that error. Opening another
   project cancels its running jobs. A result never lands during a gesture that Escape may take
   back or during a dynamic trim; it waits for the next idle frame.
9. **Atomic apply.** A result is applied in one engine transaction (`Session::edit_group`): every
   step runs, and on any failure the project and history return to the snapshot taken before the
   first step; on success the steps collapse into one undo entry named for the feature.
10. **Project id.** `Project::id` (schema v17, `#[serde(default)]`, empty in older files) is a random
   UUID given to a project on its first AI job; it names the cache folder so results survive a
   rename or move of the project file. It comes from the project file, so anything but a UUID is
   replaced before it names a folder, and the folder must lie inside `<media cache>/AI`. The
   session remembers the id it gave and puts it back after an undo to an older state; undo
   snapshots are not rewritten. A project without an id is still written as schema v16, so a
   build without the feature writes exactly what it wrote before. Copies of a project file (Save a
   Copy, Finder) share the id and therefore the cache folder; that is harmless because results
   are guarded by the fingerprint and the cache is pruned per job folder, never per project.
11. **Behind the `sidecar` cargo feature** (engine, forwarded by `apps/filmcraft` and
    `apps/filmcraft-cli`). With it off, no `ai.*` command, socket or job code is compiled and the
    product behaves as before. The client crate (`crates/sidecar`, L2) uses `std::net` only (no
    async runtime), and all socket code is `cfg(not(target_arch = "wasm32"))`.
12. **Licences.** AGENTS.md section 2 applies to the helper too: every model's code **and weights**
    must be permissively licensed (MIT, Apache-2.0, BSD, ISC). Each is listed with both licences in
    `craft-suite/sidecar/LICENSES.md` before it is added; AGPL (Ultralytics YOLO) and
    non-commercial weights are out.

13. **A feature may call a hosted service through the helper (M8f, Generative Extend).** Only the
    helper talks to the network, with the user's own key (`FAL_KEY` or `~/.config/craft-ai/fal.env`,
    never put in a project, a job folder, a log or a result); the feature is offered only while a
    key is present, and the command's description says that it costs money and sends the clip's
    frame to the provider. FilmCraft itself still opens no outbound connection. The service and
    its terms are recorded in `LICENSES.md` instead of a weights licence.

## Consequences

- The fork is no longer pure Rust end to end: AI features need a Python 3.12 install. The product
  binary still is, and builds without the feature are unchanged.
- A helper crash or hang costs one job, never the session: the job fails with a message and the
  project is untouched.
- Each feature (M6a reframe, M6b stems, M6c speech enhance, M6d beat grid and music fit, M6e search,
  M6f upscale and interpolate) is its own sub-milestone with its own acceptance tests against a
  scripted fake helper (`filmcraft_sidecar::fake`) and a real-helper smoke script.
