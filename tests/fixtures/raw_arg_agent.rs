#[cfg(unix)]
fn main() {
    use std::os::unix::ffi::OsStrExt;

    let capture = std::env::var_os("ENVOY_FAKE_RAW_CAPTURE").expect("capture path");
    let argument = std::env::args_os().nth(1).expect("captured argument");
    std::fs::write(capture, argument.as_os_str().as_bytes()).expect("write capture");
}

#[cfg(not(unix))]
fn main() {}
