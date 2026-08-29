pub mod agent;
pub mod app;
pub mod autoclose;
pub mod cli;
pub mod config;
mod entry;
pub mod discover;
pub mod hooks;
pub mod notify;
pub mod ops;
pub mod resources;
pub mod server;
pub mod theme;
pub mod tmux;
pub mod ui;
pub mod worker;

pub use entry::run;
