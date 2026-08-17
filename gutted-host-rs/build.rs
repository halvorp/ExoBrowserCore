// Compiles ../host/gutted_wpe.c into libgutted_wpe.a and links the
// dependent WPE / WebKit / GLib / wayland-server pkg-config libraries.

fn main() {
    // Force pkg-config to emit -l for system libraries too; without this the
    // WPE/GLib symbols don't get linked because they live in /usr/lib.
    std::env::set_var("PKG_CONFIG_ALLOW_SYSTEM_LIBS", "1");
    std::env::set_var("PKG_CONFIG_ALLOW_SYSTEM_CFLAGS", "1");

    let pkgs = [
        "wpe-webkit-1.1",
        "wpebackend-fdo-1.0",
        "wayland-server",
        "glib-2.0",
        "gobject-2.0",
    ];

    // 1) Silent probe: collect include paths only. cargo_metadata(false)
    //    suppresses -l emission so we control link order below.
    let mut include_paths = Vec::new();
    for pkg in pkgs {
        let lib = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe(pkg)
            .unwrap_or_else(|e| panic!("pkg-config probe {pkg} failed: {e}"));
        include_paths.extend(lib.include_paths);
    }

    // 2) Compile our static lib. cc emits `cargo:rustc-link-lib=static=gutted_wpe`
    //    which lands BEFORE the pkg-config -l flags we emit next — correct order.
    let mut build = cc::Build::new();
    build.file("../host/gutted_wpe.c");
    for inc in &include_paths {
        build.include(inc);
    }
    build.flag_if_supported("-Wno-unused-parameter");
    build.define("WPE_ENABLE_XKB", Some("1"));
    build.compile("gutted_wpe");

    // 3) Now emit the pkg-config -l flags AFTER the static lib.
    for pkg in pkgs {
        pkg_config::probe_library(pkg)
            .unwrap_or_else(|e| panic!("pkg-config link probe {pkg} failed: {e}"));
    }

    println!("cargo:rerun-if-changed=../host/gutted_wpe.c");
    println!("cargo:rerun-if-changed=../host/gutted_wpe.h");
}
