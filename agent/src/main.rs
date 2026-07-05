mod agent_loop;
mod memory;
mod scheduler;
mod skills;
mod syscall;
mod time;

use std::fs;
use std::io::Write;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "hello".to_string());

    match mode.as_str() {
        "hello" => hello(),
        "traversal" => try_read("../../../../../../etc/passwd"),
        "abs" => try_read("/etc/passwd"),
        "symlink" => try_read("escape_link/passwd"),
        "notify" => notify_demo(),
        "sleep_until" => sleep_until_demo(),
        "db_exec" => db_exec_demo(),
        "db_attach_escape" => db_attach_escape(),
        "db_slow_query" => db_slow_query(),
        "memory_hog" => memory_hog(),
        "spin_forever" => spin_forever(),
        "llm_call_demo" => llm_call_demo(),
        "embed_demo" => embed_demo(),
        "http_fetch_demo" => http_fetch_demo(),
        "http_fetch_ssrf_localhost" => http_fetch_url("http://127.0.0.1:1/"),
        "http_fetch_ssrf_private" => http_fetch_url("http://192.168.1.1/"),
        "http_fetch_ssrf_metadata" => http_fetch_url("http://169.254.169.254/"),
        "http_fetch_post" => http_fetch_post_demo(),
        "http_fetch_long_url" => http_fetch_url(&format!("https://example.com/?q={}", "a".repeat(3000))),
        "exec_wasm_demo" => exec_wasm_demo(),
        "search_web_demo" => search_web_demo(),
        "read_path" => read_path_mode(),
        "run" => run_mode(),
        other => {
            println!("RESULT:unknown_mode:{other}");
        }
    }
}

fn run_mode() {
    let trigger_str = std::env::args()
        .nth(2)
        .unwrap_or_else(|| r#"{"type":"manual","text":"introduce yourself briefly, then finish"}"#.to_string());
    let trigger: serde_json::Value =
        serde_json::from_str(&trigger_str).unwrap_or_else(|_| serde_json::json!({"type": "manual", "raw": trigger_str}));
    agent_loop::run(&trigger);
}

fn notify_demo() {
    let resp = syscall::call("notify", &serde_json::json!({"message": "hello from agent guest"}));
    println!("RESULT:{resp}");
}

fn sleep_until_demo() {
    let resp = syscall::call("sleep_until", &serde_json::json!({"timestamp": 1_900_000_000i64}));
    println!("RESULT:{resp}");
}

fn db_exec_demo() {
    let create = syscall::call(
        "db_exec",
        &serde_json::json!({"sql": "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT)", "params": []}),
    );
    let insert = syscall::call(
        "db_exec",
        &serde_json::json!({"sql": "INSERT INTO notes (text) VALUES (?1)", "params": ["hello from guest"]}),
    );
    let select = syscall::call(
        "db_exec",
        &serde_json::json!({"sql": "SELECT id, text FROM notes", "params": []}),
    );
    println!("RESULT:create={create} insert={insert} select={select}");
}

fn db_attach_escape() {
    let resp = syscall::call(
        "db_exec",
        &serde_json::json!({"sql": "ATTACH DATABASE '/etc/passwd' AS evil", "params": []}),
    );
    println!("RESULT:{resp}");
}

fn db_slow_query() {
    let resp = syscall::call(
        "db_exec",
        &serde_json::json!({
            "sql": "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x < 200000000) SELECT count(*) FROM cnt",
            "params": []
        }),
    );
    println!("RESULT:{resp}");
}

/// tries to allocate well past the kernel's 512MB memory cap — should abort
/// (via `memory.grow` failing) before the RESULT print below is reached
fn memory_hog() {
    let mut buf: Vec<u8> = Vec::with_capacity(600 * 1024 * 1024);
    for i in 0..buf.capacity() {
        buf.push((i % 256) as u8);
    }
    println!("RESULT:allocated {} bytes", buf.len());
}

/// busy-loops forever — should be trapped by epoch interruption rather than
/// hanging the kernel
fn spin_forever() {
    let mut x: u64 = 0;
    loop {
        x = x.wrapping_add(1);
        std::hint::black_box(x);
    }
}

fn llm_call_demo() {
    let resp = syscall::call(
        "llm_call",
        &serde_json::json!({
            "messages": [{"role": "user", "content": "say hi in 3 words"}],
            "stream": false
        }),
    );
    println!("RESULT:{resp}");
}

fn embed_demo() {
    let resp = syscall::call("embed", &serde_json::json!({"texts": ["hello world", "second text"]}));
    println!("RESULT:{resp}");
}

fn http_fetch_demo() {
    http_fetch_url("https://example.com/");
}

fn http_fetch_url(url: &str) {
    let resp = syscall::call("http_fetch", &serde_json::json!({"method": "GET", "url": url}));
    println!("RESULT:{resp}");
}

fn http_fetch_post_demo() {
    let resp = syscall::call(
        "http_fetch",
        &serde_json::json!({"method": "POST", "url": "https://example.com/submit", "body": "hi"}),
    );
    println!("RESULT:{resp}");
}

fn exec_wasm_demo() {
    let resp = syscall::call(
        "exec_wasm",
        &serde_json::json!({"wasm_path": "bin/tool.wasm", "args": ["read_path", "/memory/notes/pet.md"]}),
    );
    println!("RESULT:{resp}");
}

/// reads the path given as argv[2] — used both directly (host-driven tests)
/// and as the payload run *inside* an `exec_wasm` sub-Store, where "/" is
/// scoped to `workspace/` instead of the whole agent-home
fn read_path_mode() {
    let path = std::env::args().nth(2).unwrap_or_default();
    try_read(&path);
}

fn search_web_demo() {
    let resp = syscall::call("search_web", &serde_json::json!({"query": "Ningxia night market Taipei opening hours"}));
    println!("RESULT:{resp}");
}

fn hello() {
    println!("hello from agent guest");

    let path = "/workspace/hello.txt";
    let mut f = fs::File::create(path).expect("write hello.txt");
    f.write_all(b"hello from guest\n").expect("write bytes");
    drop(f);

    let contents = fs::read_to_string(path).expect("read hello.txt");
    print!("read back: {contents}");
}

fn try_read(path: &str) {
    match fs::read_to_string(path) {
        Ok(contents) => println!("RESULT:ok:{contents}"),
        Err(e) => println!("RESULT:blocked:{e}"),
    }
}
