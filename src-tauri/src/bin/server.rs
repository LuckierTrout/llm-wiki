//! LLM Wiki web server — serves the full desktop feature set over HTTP.
//! Built with `cargo build --release --features web-server --bin llm-wiki-server`.

fn main() {
    llm_wiki_lib::web_server::run();
}
