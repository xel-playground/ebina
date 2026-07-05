//! Minimal WASI-only binary with no `env::syscall` import — used as a
//! stand-in "installed tool" to test `exec_wasm`'s isolated Store (kernel/
//! src/syscalls/exec_wasm.rs). Real agent.wasm always imports `env::syscall`
//! regardless of which mode runs, so it can't be used for that test itself.

use std::fs;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "read_path" => {
            let path = std::env::args().nth(2).unwrap_or_default();
            match fs::read_to_string(&path) {
                Ok(contents) => println!("RESULT:ok:{contents}"),
                Err(e) => println!("RESULT:blocked:{e}"),
            }
        }
        "echo" => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            println!("RESULT:{}", rest.join(" "));
        }
        other => println!("RESULT:unknown_mode:{other}"),
    }
}
