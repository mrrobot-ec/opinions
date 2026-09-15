//! The one place a Postgres suite learns where its database is.
//!
//! Every Pg suite in this directory used to resolve `DATABASE_URL` itself and
//! `return` early when anything about the setup failed — an unset variable, a
//! refused connection, a failed `create database`, a failed migration. Each of
//! those turned the whole suite into a no-op that still printed `ok`. Running
//! the nine Pg suites with `DATABASE_URL` unset reported 75 passing tests in
//! 0.01s without executing a single statement.
//!
//! So: there is no skip path here. An unset `DATABASE_URL` is a panic with an
//! actionable message, and every suite helper that builds on this one turns
//! connect/create/migrate failures into panics rather than `None`. A Pg suite
//! that cannot reach Postgres must FAIL, never silently pass — a skipped Pg
//! suite is worse than none.
//!
//! `justfile` already exports a default `DATABASE_URL`, so the normal gate is
//! unaffected; what changes is that a bare `cargo test` or a down database can
//! no longer masquerade as a green run.

/// Resolve the suite's database URL, or fail the test loudly.
///
/// # Panics
/// `DATABASE_URL` unset or blank.
#[must_use]
pub fn database_url() -> String {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => panic!(
            "DATABASE_URL is required by this PostgreSQL suite and is unset or blank. \
             Start the database and export it (`docker compose up -d db`; the justfile \
             defaults to postgres://opinions:opinions@localhost:15434/opinions). \
             This suite refuses to skip: a Pg suite that silently passes without a \
             database is verification theatre."
        ),
    }
}
