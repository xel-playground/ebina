use serde_json::Value;

unsafe extern "C" {
    fn syscall(
        name_ptr: *const u8,
        name_len: i32,
        req_ptr: *const u8,
        req_len: i32,
        out_ptr: *mut u8,
        out_cap: i32,
    ) -> i32;
}

/// sentinel for a fatal ABI-level failure — mirrors kernel/src/abi.rs::FATAL
const FATAL: i32 = i32::MIN;

/// Calls host syscall `name` with JSON request `req`, growing the output
/// buffer and retrying if the host reports it was too small. Always returns
/// a JSON value: on protocol failure, a synthesized `{"ok":false,...}`
/// envelope in the same shape the host would produce.
pub fn call(name: &str, req: &Value) -> Value {
    let req_bytes = serde_json::to_vec(req).expect("request must serialize");
    let mut cap: usize = 4096;
    loop {
        let mut buf = vec![0u8; cap];
        let written = unsafe {
            syscall(
                name.as_ptr(),
                name.len() as i32,
                req_bytes.as_ptr(),
                req_bytes.len() as i32,
                buf.as_mut_ptr(),
                buf.len() as i32,
            )
        };

        if written == FATAL {
            panic!("fatal syscall ABI error calling `{name}`");
        }
        if written >= 0 {
            buf.truncate(written as usize);
            return serde_json::from_slice(&buf).unwrap_or_else(|e| {
                serde_json::json!({
                    "ok": false,
                    "error": {"code": "bad_response_json", "message": e.to_string()}
                })
            });
        }
        cap = (-written) as usize;
    }
}
