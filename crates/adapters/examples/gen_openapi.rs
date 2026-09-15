//! Offline OpenAPI exporter (no server; stdout is raw JSON for scripts).
#![allow(clippy::doc_markdown)]

use adapters::http::routes::ApiDoc;
use utoipa::OpenApi;

fn main() {
    let doc = ApiDoc::openapi();
    match serde_json::to_string_pretty(&doc) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("failed to serialize openapi: {e}");
            std::process::exit(1);
        }
    }
}
