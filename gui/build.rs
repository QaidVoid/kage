//! The Windows resource embed: the executable icon for Explorer, the
//! taskbar and Alt-Tab. Every other target runs this as a no-op.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=assets/kage.ico");
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/kage.ico");
    resource.compile().expect("the Windows icon embeds");
}
