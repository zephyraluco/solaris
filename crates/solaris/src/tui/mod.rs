//! Terminal adapter and its interaction and presentation state.

pub mod app;
pub mod buddy;
pub mod commands;
pub mod connect;
pub(crate) mod inline;
pub mod keymap;
pub mod model;
pub mod presentation;
pub mod state;
pub mod tips;
pub mod view;

/// The greeting line for the welcome box.
pub fn greeting(user: &str) -> String {
    if user == "solaris" {
        "Welcome back!".to_string()
    } else {
        format!("Welcome back {user}!")
    }
}
