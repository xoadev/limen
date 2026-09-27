//! What this build is: set by the build's environment (`LIMEN_VERSION`, `LIMEN_BUILD_DATE`, `LIMEN_BUILD_NUMBER`),
//! `dev` otherwise.

pub const VERSION: &str = match option_env!("LIMEN_VERSION") {
    Some(v) => v,
    None => "dev",
};

pub const BUILD_DATE: &str = match option_env!("LIMEN_BUILD_DATE") {
    Some(v) => v,
    None => "",
};

pub const BUILD_NUMBER: &str = match option_env!("LIMEN_BUILD_NUMBER") {
    Some(v) => v,
    None => "",
};
