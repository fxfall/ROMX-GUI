fn main() {
    println!("cargo:rerun-if-changed=locales/en.json");
    println!("cargo:rerun-if-changed=locales/zh-CN.json");
    slint_build::compile("ui/app-window.slint").unwrap();
}
