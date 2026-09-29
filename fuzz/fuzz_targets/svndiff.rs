#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    version_control_rs::svn_http::fuzz_entry::svndiff(data);
});
