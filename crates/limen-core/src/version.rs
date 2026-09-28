//! What this build is: set by the build's environment (`LIMEN_VERSION`, `LIMEN_BUILD_DATE`, `LIMEN_BUILD_NUMBER`),
//! `dev` otherwise.

pub const VERSION: &str = set_or(option_env!("LIMEN_VERSION"), "dev");

pub const BUILD_DATE: &str = set_or(option_env!("LIMEN_BUILD_DATE"), "");

pub const BUILD_NUMBER: &str = set_or(option_env!("LIMEN_BUILD_NUMBER"), "");

/// `Option::unwrap_or`, which is not `const`.
const fn set_or(value: Option<&'static str>, unset: &'static str) -> &'static str {
    match value {
        Some(value) => value,
        None => unset,
    }
}
