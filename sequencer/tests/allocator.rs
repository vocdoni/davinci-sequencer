//! The node binary runs jemalloc with the decay baked in by
//! `.cargo/config.toml`; glibc's per-thread arenas kept GBs of freed memory.

use std::process::Command;

#[test]
fn binary_runs_jemalloc_with_the_baked_decay() {
    let out = Command::new(env!("CARGO_BIN_EXE_davinci-sequencer"))
        .arg("--version")
        .env("_RJEM_MALLOC_CONF", "stats_print:true")
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(
            r#"config.malloc_conf: "background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0""#
        ),
        "not jemalloc with the baked config:\n{err}"
    );
    assert!(err.contains("opt.background_thread: true"), "{err}");
}
