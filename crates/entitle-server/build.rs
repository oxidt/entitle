fn main() {
    // tailwind.css imports oxidt-ui's theme through this generated file; the
    // path is the dependency's checkout, handed over by its build script.
    let theme = std::env::var("DEP_OXIDT_UI_THEME").unwrap();
    let import = format!("@import \"{theme}/oxidt.css\";\n");
    // Rewriting an unchanged file would wake `topcoat dev` on every build.
    if std::fs::read_to_string("oxidt-ui.css").ok().as_deref() != Some(import.as_str()) {
        std::fs::write("oxidt-ui.css", import).unwrap();
    }
    println!("cargo::rerun-if-changed={theme}");
    for path in ["tailwind.css", "src/dashboard"] {
        println!("cargo::rerun-if-changed={path}");
    }
    topcoat::tailwind::BuildConfig::new()
        .input("tailwind.css")
        .render()
        .unwrap();
}
