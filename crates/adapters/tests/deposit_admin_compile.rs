//! Compile contract for the coordinator-mounted W3 deposit admin module.
//!
//! Until `routes/mod.rs` declares the module, the Rust module graph would not
//! compile this file at all. The shim mirrors its real parent modules without
//! changing the coordinator-owned route aggregation.

mod shim {
    pub mod http {
        pub mod dto {
            pub use adapters::http::dto::*;
        }

        pub mod error {
            pub use adapters::http::error::*;
        }

        pub mod routes {
            pub use adapters::http::routes::AppState;

            pub mod deposit_admin {
                include!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/http/routes/deposit_admin.rs"
                ));
            }
        }
    }
}

#[test]
fn deposit_admin_router_is_a_valid_app_state_router() {
    let _ = shim::http::routes::deposit_admin::router::<application::fakes::InMemoryStore>();
}
