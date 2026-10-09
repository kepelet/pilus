//! The C ABI the iOS app calls.
//!
//! Every entry point is `extern "C"` and blocking; Swift drives them from
//! `Task.detached` so the network round trip never touches the main actor.
//! Pointers handed out are opaque `PilusClient*` handles or `char*` JSON
//! strings, both owned by the caller until `pilus_client_free` /
//! `pilus_string_free`. String returns are `NULL` on failure with the message
//! in the thread-local slot `pilus_last_error` reads.
//!
//! `unsafe` is unavoidable at an FFI boundary — every raw pointer dereference
//! below is guarded: non-null checked first, strings read as NUL-terminated
//! `CStr`, and ownership moves across the boundary exactly once. The module
//! is the only place the crate lifts `unsafe_code`.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;
use std::ptr;

use serde_json::Value;

use crate::client::{Client, Error};

/// An opaque handle for C callers.
pub struct PilusClient {
    inner: Client,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn record(error: Error) -> *mut c_char {
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(error.to_string()).ok();
    });
    ptr::null_mut()
}

fn record_bool(error: Error) -> i32 {
    let _ = record(error);
    -1
}

fn into_json(value: Value) -> *mut c_char {
    match CString::new(value.to_string()) {
        Ok(string) => string.into_raw(),
        Err(_) => record(Error::Reply("reply contained a NUL byte".to_string())),
    }
}

/// Read a required C string argument.
///
/// SAFETY: the caller promises `pointer` is a valid NUL-terminated UTF-8
/// string for the call's duration; a null or invalid string is an error, not
/// undefined behavior.
unsafe fn arg<'a>(pointer: *const c_char, name: &'static str) -> Result<&'a str, Error> {
    if pointer.is_null() {
        return Err(Error::Reply(format!("missing argument `{name}`")));
    }
    // SAFETY: null was checked above; the FFI contract requires a valid
    // NUL-terminated string for the duration of the call.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map_err(|error| Error::Reply(format!("`{name}` is not utf-8: {error}")))
}

/// Read an optional C string argument (`NULL` → `""`).
///
/// SAFETY: same contract as [`arg`].
unsafe fn arg_or_empty<'a>(pointer: *const c_char) -> Result<&'a str, Error> {
    if pointer.is_null() {
        return Ok("");
    }
    // SAFETY: null was checked above.
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map_err(|error| Error::Reply(format!("argument is not utf-8: {error}")))
}

/// Deref a client handle.
///
/// SAFETY: the handle must be a live pointer produced by `pilus_client_connect`
/// or `pilus_client_resume` and not yet freed; null is an error, not UB.
unsafe fn client<'a>(pointer: *mut PilusClient) -> Result<&'a PilusClient, Error> {
    if pointer.is_null() {
        return Err(Error::Reply("null client handle".to_string()));
    }
    // SAFETY: null was checked above; callers must not use freed handles.
    Ok(unsafe { &*pointer })
}

/// The device's `EndpointId` as a JSON string, without connecting.
///
/// `data_dir` is where `device.key` and `enrollment.json` live — the app's
/// Application Support directory. Returns `NULL` on error.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_device_id(data_dir: *const c_char) -> *mut c_char {
    // SAFETY: `data_dir` follows the FFI string contract.
    let dir = unsafe { arg(data_dir, "data_dir") };
    match dir.and_then(|dir| Client::device_id(&PathBuf::from(dir))) {
        Ok(id) => into_json(serde_json::json!({ "endpoint_id": id })),
        Err(error) => record(error),
    }
}

/// Bind a client for `ticket`. Returns `NULL` on error; call
/// `pilus_last_error` for the message.
///
/// `ticket` is the `EndpointTicket` string `pilus-agent serve --iroh` prints.
/// `relay`/`api_secret` mirror the app's connection settings: empty means the
/// public n0 relays.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_client_connect(
    data_dir: *const c_char,
    ticket: *const c_char,
    relay: *const c_char,
    api_secret: *const c_char,
) -> *mut PilusClient {
    // SAFETY: all four arguments follow the FFI string contract; the last two
    // may be null (treated as empty).
    let dir = unsafe { arg(data_dir, "data_dir") };
    let ticket = unsafe { arg(ticket, "ticket") };
    let relay = unsafe { arg_or_empty(relay) };
    let api_secret = unsafe { arg_or_empty(api_secret) };
    let client = dir.and_then(|dir| {
        ticket.and_then(|ticket| {
            relay.and_then(|relay| {
                api_secret.and_then(|api_secret| {
                    Client::connect(&PathBuf::from(dir), ticket, relay, api_secret)
                })
            })
        })
    });
    match client {
        Ok(inner) => Box::into_raw(Box::new(PilusClient { inner })),
        Err(error) => {
            let _ = record(error);
            ptr::null_mut()
        }
    }
}

/// Reconnect a client from the saved enrollment in `data_dir`.
/// Returns `NULL` (with `not enrolled` as the last error) when none exists.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_client_resume(
    data_dir: *const c_char,
    relay: *const c_char,
    api_secret: *const c_char,
) -> *mut PilusClient {
    // SAFETY: `data_dir` follows the FFI string contract; the last two may be
    // null (treated as empty).
    let dir = unsafe { arg(data_dir, "data_dir") };
    let relay = unsafe { arg_or_empty(relay) };
    let api_secret = unsafe { arg_or_empty(api_secret) };
    let client = dir.and_then(|dir| {
        relay.and_then(|relay| {
            api_secret.and_then(|api_secret| Client::resume(&PathBuf::from(dir), relay, api_secret))
        })
    });
    match client {
        Ok(inner) => Box::into_raw(Box::new(PilusClient { inner })),
        Err(error) => {
            let _ = record(error);
            ptr::null_mut()
        }
    }
}

/// Enroll with the host's printed `code`, then hold the delegated token.
/// Returns 0 on success, -1 on failure (`pilus_last_error`).
#[unsafe(no_mangle)]
pub extern "C" fn pilus_client_enroll(
    client: *mut PilusClient,
    code: *const c_char,
    label: *const c_char,
) -> i32 {
    // SAFETY: `client` must be a live handle; strings follow the FFI contract.
    let client = unsafe { self::client(client) };
    let code = unsafe { arg(code, "code") };
    let label = unsafe { arg_or_empty(label) };
    let result = client.and_then(|client| {
        code.and_then(|code| label.and_then(|label| client.inner.enroll(code, label)))
    });
    match result {
        Ok(()) => 0,
        Err(error) => record_bool(error),
    }
}

/// Refresh the delegated token for an already-enrolled device.
/// Returns 0 on success, -1 on failure.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_client_authorize(client: *mut PilusClient) -> i32 {
    // SAFETY: `client` must be a live handle.
    match unsafe { self::client(client) }.and_then(|client| client.inner.authorize()) {
        Ok(()) => 0,
        Err(error) => record_bool(error),
    }
}

/// Send one UHP request. `params_json` is a JSON object string (`"{}"` for
/// none). Returns the reply's `result` as a JSON string, or `NULL` on error.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_client_request(
    client: *mut PilusClient,
    method: *const c_char,
    params_json: *const c_char,
) -> *mut c_char {
    // SAFETY: `client` must be a live handle; strings follow the FFI contract.
    let client = unsafe { self::client(client) };
    let method = unsafe { arg(method, "method") };
    let params = unsafe { arg(params_json, "params_json") }.and_then(|raw| {
        serde_json::from_str::<Value>(raw)
            .map_err(|error| Error::Reply(format!("params are not json: {error}")))
    });
    let result = client.and_then(|client| {
        method.and_then(|method| params.and_then(|params| client.inner.request(method, params)))
    });
    match result {
        Ok(value) => into_json(value),
        Err(error) => record(error),
    }
}

/// One hub request. `machine` is `""` for the paired host itself; the reply's
/// `result` comes back as a JSON string, or `NULL` on error. Older connectors
/// without the hub surface fail the connect — callers fall back to
/// `pilus_client_request`.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_client_hub_request(
    client: *mut PilusClient,
    machine: *const c_char,
    method: *const c_char,
    params_json: *const c_char,
) -> *mut c_char {
    // SAFETY: `client` must be a live handle; strings follow the FFI contract.
    let client = unsafe { self::client(client) };
    let machine = unsafe { arg_or_empty(machine) };
    let method = unsafe { arg(method, "method") };
    let params = unsafe { arg(params_json, "params_json") }.and_then(|raw| {
        serde_json::from_str::<Value>(raw)
            .map_err(|error| Error::Reply(format!("params are not json: {error}")))
    });
    let result = client.and_then(|client| {
        machine.and_then(|machine| {
            method.and_then(|method| {
                params.and_then(|params| client.inner.hub_request(method, machine, params))
            })
        })
    });
    match result {
        Ok(value) => into_json(value),
        Err(error) => record(error),
    }
}

/// Open `hub.watch` on the paired host. Returns the subscription's first
/// `result` as JSON, or `NULL` on error. Afterwards `pilus_hub_watch_next`
/// yields one change signal per call.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_hub_watch_open(client: *mut PilusClient) -> *mut c_char {
    // SAFETY: `client` must be a live handle.
    match unsafe { self::client(client) }.and_then(|client| client.inner.hub_watch_open()) {
        Ok(value) => into_json(value),
        Err(error) => record(error),
    }
}

/// The next `hub.watch` signal as a JSON string, or `NULL` when the stream
/// ended or errored (the watch is then dropped and must be reopened).
#[unsafe(no_mangle)]
pub extern "C" fn pilus_hub_watch_next(client: *mut PilusClient) -> *mut c_char {
    // SAFETY: `client` must be a live handle.
    match unsafe { self::client(client) }.and_then(|client| client.inner.hub_watch_next()) {
        Ok(value) => into_json(value),
        Err(error) => record(error),
    }
}

/// Close the open watch, if any.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_hub_watch_close(client: *mut PilusClient) {
    // SAFETY: `client` must be a live handle or NULL (then this is a no-op).
    if let Ok(client) = unsafe { self::client(client) } {
        client.inner.hub_watch_close();
    }
}

/// The cached enrollment summary as JSON, or `null` when absent.
/// Fields: `endpoint_id` (host), `ticket`.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_client_enrollment(data_dir: *const c_char) -> *mut c_char {
    // SAFETY: `data_dir` follows the FFI string contract.
    let dir = unsafe { arg(data_dir, "data_dir") };
    let enrollment = dir
        .ok()
        .and_then(|dir| Client::enrollment(&PathBuf::from(dir)));
    match enrollment {
        Some(enrollment) => into_json(serde_json::json!({
            "endpoint_id": enrollment.endpoint_id,
            "ticket": enrollment.ticket,
        })),
        None => into_json(Value::Null),
    }
}

/// Delete the saved enrollment under `data_dir`.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_forget(data_dir: *const c_char) {
    // SAFETY: `data_dir` follows the FFI string contract.
    if let Ok(dir) = unsafe { arg(data_dir, "data_dir") } {
        Client::forget(&PathBuf::from(dir));
    }
}

/// Free a client handle.
///
/// SAFETY: `client` must come from `pilus_client_connect`/`resume` and be
/// freed at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pilus_client_free(client: *mut PilusClient) {
    if !client.is_null() {
        // SAFETY: `client` was allocated by `Box::into_raw` in connect/resume.
        drop(unsafe { Box::from_raw(client) });
    }
}

/// The last error message on this thread, as a new string. `NULL` when the
/// last call succeeded. Free with `pilus_string_free`.
#[unsafe(no_mangle)]
pub extern "C" fn pilus_last_error() -> *mut c_char {
    LAST_ERROR.with(|slot| {
        let slot = slot.borrow();
        let Some(message) = slot.as_ref() else {
            return ptr::null_mut();
        };
        match CString::new(message.as_bytes()) {
            Ok(copy) => copy.into_raw(),
            Err(_) => ptr::null_mut(),
        }
    })
}

/// Free a string returned by any `pilus_*` function.
///
/// SAFETY: `string` must come from this library and be freed at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pilus_string_free(string: *mut c_char) {
    if !string.is_null() {
        // SAFETY: `string` was allocated by `CString::into_raw` above.
        drop(unsafe { CString::from_raw(string) });
    }
}
