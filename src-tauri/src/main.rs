// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(feature = "web-server"))]
fn main() {
    llm_wiki_lib::run();
}

// The desktop entry point does not exist in web-server builds (the command
// runtime is MockRuntime there, so no window can be created). Keep a stub so
// `cargo build --features web-server` still compiles every target.
#[cfg(feature = "web-server")]
fn main() {
    eprintln!("This binary was built with the `web-server` feature; run `llm-wiki-server` instead.");
    std::process::exit(1);
}
