fn main() {
    println!("cargo:rerun-if-changed=windows");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile(
            "windows/oxide-ide.rc",
            embed_resource::ParamsIncludeDirs(["windows"]),
        )
        .manifest_required()
        .expect("Oxide IDE requires its Windows application manifest");
    }
}
