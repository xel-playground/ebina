use include_dir::{include_dir, Dir};

/// Baked into the `ebinactl` binary at compile time — `init` writes these
/// straight out, so a fresh workspace never needs the user to separately
/// `cargo build -p agent --target wasm32-wasip1` or `npm run build` the
/// webui and go copy the results into place by hand. There's no release
/// pipeline to download prebuilt artifacts from instead (this is a
/// from-source monorepo, not a published project), so embedding at build
/// time is the only option that doesn't require a network fetch at `init`
/// time — same "self-contained, no external dependency at runtime"
/// direction as everything else built this session.
///
/// Consequence: both must already be built *before* `cargo build -p
/// ebinactl` — `cargo build -p agent --target wasm32-wasip1 --release` and
/// `npm run build` in `webui/`. `ebinactl` itself is downstream of both in
/// the build order, not the other way around.
pub static AGENT_WASM: &[u8] = include_bytes!("../../target/wasm32-wasip1/release/agent.wasm");

pub static WEBUI_DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/../webui/dist");
