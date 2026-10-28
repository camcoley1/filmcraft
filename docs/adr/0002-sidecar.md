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
2. **Loopback JSON lines.** The helper listens on `127.0.0.1` (default port 9877). Every request is
   one JSON object on one line, on a fresh TCP connection, answered by one line. The protocol table
   is in `crates/sidecar/src/lib.rs`: `hello`, `job.start`, `job.status`, `job.cancel`, and an
   `error` reply for any of them.
3. **Handshake with a protocol version.** `hello` returns `{protocol, version, features[]}`. A helper
   speaking another protocol is refused: every `ai.*` command stays disabled with "AI helper is
   version X (protocol N); this FilmCraft needs protocol M".
4. **Disabled with a reason, never missing.** With the `sidecar` feature on, the `ai.*` commands
   exist whether or not a helper runs. Without a helper, or when the helper lacks the feature, each
   is disabled with a reason (exactly like `transcript.generate` without Whisper, `NO_SPEECH` in
   `crates/engine/src/transcript.rs`). `ai.status` probes the helper and `ai.connect {port}` picks
   another port.
5. **Bounded, cancellable jobs.** Every job runs as an engine `Job`: progress 0 to 1 in the Events
   panel and `jobs.list`, cancelled by `jobs.cancel` (the helper is told to stop). The client caps
   reply size, result file count and total result size, and a per-job wall-clock timeout; a helper
   that stops answering fails the job. Inputs are bounded by each feature (duration and size caps;
   long audio is processed in bounded chunks).
6. **Results are files; the engine applies them.** The helper writes result files into the job's
   folder in the project's AI cache, `<media cache>/AI/<project id>/<job>/`; it never writes the
   project. The engine checks the files (inside the folder, regular files, within the size cap) and
   applies them through its own edits.
7. **Stale results are dropped, not applied.** A job records a fingerprint when it starts: the
   session's project generation (bumped by New / Open / Close / Recover), the project id, and for
   each target clip its sequence, track, media item, media path, placement, source In, speed and
   direction. When the result arrives, a changed fingerprint (the clip was edited, deleted or the
   edit undone, or another project is open) drops the result with an event ("result kept in
   cache"); nothing is applied.
8. **Atomic apply.** A result is applied in one engine transaction (`Session::edit_group`): every
   step runs, and on any failure the project and history return to the snapshot taken before the
   first step; on success the steps collapse into one undo entry named for the feature.
9. **Project id.** `Project::id` (schema v17, `#[serde(default)]`, empty in older files) is a random
   UUID given to a project on its first AI job; it names the cache folder so results survive a
   rename or move of the project file.
10. **Behind the `sidecar` cargo feature** (engine, forwarded by `apps/filmcraft` and
    `apps/filmcraft-cli`). With it off, no `ai.*` command, socket or job code is compiled and the
    product behaves as before. The client crate (`crates/sidecar`, L2) uses `std::net` only (no
    async runtime), and all socket code is `cfg(not(target_arch = "wasm32"))`.
11. **Licences.** AGENTS.md section 2 applies to the helper too: every model's code **and weights**
    must be permissively licensed (MIT, Apache-2.0, BSD, ISC). Each is listed with both licences in
    `craft-suite/sidecar/LICENSES.md` before it is added; AGPL (Ultralytics YOLO) and
    non-commercial weights are out.

## Consequences

- The fork is no longer pure Rust end to end: AI features need a Python 3.12 install. The product
  binary still is, and builds without the feature are unchanged.
- A helper crash or hang costs one job, never the session: the job fails with a message and the
  project is untouched.
- Each feature (M6a reframe, M6b stems, M6c speech enhance, M6d beat grid and music fit, M6e search,
  M6f upscale and interpolate) is its own sub-milestone with its own acceptance tests against a
  scripted fake helper (`filmcraft_sidecar::fake`) and a real-helper smoke script.
