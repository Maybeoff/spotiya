fn main() {
    let arm = std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm");
    let mut native = cc::Build::new();
    if arm {
        native.file("controls/alsa_compat.c");
    }
    native
        .file("controls/mini2/buttons.c")
        .file("controls/plum/buttons.c")
        .file("controls/stdio.c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Os")
        .flag_if_supported("-fstack-protector-strong")
        .warnings(true)
        .warnings_into_errors(true)
        .compile("mini2_controls");
    for name in ["buttons.c", "config.h", "lights.h", "display.h"] {
        println!("cargo:rerun-if-changed=controls/mini2/{name}");
        println!("cargo:rerun-if-changed=controls/plum/{name}");
    }
    println!("cargo:rerun-if-changed=controls/stdio.c");
    println!("cargo:rerun-if-changed=controls/alsa_compat.c");
    if arm {
        // Vendor ALSA plugins also load libraries from this directory.
        println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags,-rpath,/system/vendor/quasar/libs");
        for name in [
            "get_buffer_size",
            "get_buffer_size_min",
            "get_buffer_size_max",
            "get_period_size",
            "get_period_size_min",
            "get_period_size_max",
            "set_buffer_size_near",
            "set_period_size_near",
        ] {
            println!("cargo:rustc-link-arg=-Wl,--wrap=snd_pcm_hw_params_{name}");
        }
    }
}
