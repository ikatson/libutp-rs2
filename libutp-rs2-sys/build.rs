use std::path::PathBuf;

const LIBUTP_PATH: &str = "libutp";

fn main() {
    let bindings = bindgen::Builder::default()
        .use_core()
        .header(format!("{LIBUTP_PATH}/utp.h"))
        .allowlist_item("^(utp|UTP|SHUT_).*")
        .anon_fields_prefix("unnamed_field")
        .opaque_type("socklen_t")
        .blocklist_type("sockaddr")
        .generate()
        .expect("unable to generate bindings");

    let out_path = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path)
        .expect("Couldn't write bindings!");

    let mut builder = cc::Build::new();

    builder
        .cpp(true)
        .define(
            if cfg!(unix) {
                "POSIX"
            } else {
                "__UNUSED_NOT_POSIX"
            },
            "",
        )
        .define(
            if cfg!(debug_assertions) {
                "UTP_DEBUG_LOGGING"
            } else {
                "__UNUSED_UTP_DEBUG_LOGGING"
            },
            "1",
        )
        .files(
            [
                "utp_internal.cpp",
                "utp_utils.cpp",
                "utp_hash.cpp",
                "utp_callbacks.cpp",
                "utp_api.cpp",
                "utp_packedsockaddr.cpp",
            ]
            .into_iter()
            .map(|f| format!("{LIBUTP_PATH}/{f}")),
        )
        .warnings(false)
        .compile("libutp");

    println!("cargo:rustc-link-lib=static=libutp")
}
