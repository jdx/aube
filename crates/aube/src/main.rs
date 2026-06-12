// Thin binary wrapper: the whole CLI lives in the `aube` library crate
// (src/lib.rs) so the command layer can also be embedded as a library.
// Only two things belong here: the global-allocator choice (a
// binary-level policy the library must not impose on embedders) and the
// `main` that forwards to `aube::cli_main`. The `aubr` / `aubx`
// multicall shims `include!` this file, so all three bins stay
// byte-identical in behavior.

// mimalloc as global allocator on release builds. Cuts linker-phase
// wall time and peak RSS on large installs. Per-thread heaps suit
// rayon work-stealing and tokio's blocking pool. Gated on
// `not(debug_assertions)` so `cargo run` and `cargo test` keep the
// system allocator, which keeps Valgrind, ASAN, and Miri happy.
// `secure` feature skipped. aube's hot path is tarball extraction
// with bounded input, not a sandbox boundary.
#[cfg(all(feature = "mimalloc", not(debug_assertions)))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // The standalone `aube` binary runs with aube's own embedder profile.
    // Embedders call `aube::cli_main` with their own `&'static Embedder`
    // instead (and `cli_main_with_defaults` to also seed setting defaults).
    aube::cli_main(&aube_util::identity::AUBE)
}
