fn main() {
    cc::Build::new()
        .file("controls/mini2/buttons.c")
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Os")
        .flag_if_supported("-fstack-protector-strong")
        .warnings(true)
        .warnings_into_errors(true)
        .compile("mini2_controls");
    for name in ["buttons.c", "config.h", "lights.h", "display.h"] {
        println!("cargo:rerun-if-changed=controls/mini2/{name}");
    }
}
