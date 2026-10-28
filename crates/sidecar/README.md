# filmcraft-sidecar

Client for `craft-ai`, the optional local AI helper ([ADR 0002](../../docs/adr/0002-sidecar.md)).

- `SidecarClient`: hello, start, status, cancel. `TcpClient` talks to the helper on `127.0.0.1`
  (one JSON line per request, one reply line, one deadline for the whole exchange) and sends the
  helper's per-launch token from `token_file()`; the engine's tests install a scripted fake.
- `drive`: runs one job to the end with progress, cancel, a wall-clock timeout and a limit on
  failed polls (a killed helper fails the job).
- `check_result`: result files must be regular files (not links) inside the job's folder, within
  the count and size caps of `Limits`; `open_result` checks again on the open handle.
- `fake` (feature `fake`): an in-process loopback helper whose `Script` produces malformed replies,
  version mismatches, missing / oversized / escaping result files, failures, a helper that dies
  mid-job, a silent or trickling peer and a required token.

Socket code is native only; the protocol types and `drive` build for wasm32 too (`cargo xtask
wasm`). No async runtime: `std::net` only.
