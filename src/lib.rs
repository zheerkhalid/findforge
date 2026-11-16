//! FindForge: instant file search for Windows.
//!
//! The logic lives in this library so that it can be unit-tested; `main.rs`
//! only parses the command line and opens the window.
pub mod cli;
pub mod demo;
pub mod desktop;
pub mod engine;
pub mod export;
pub mod index;
pub mod ntfs;
pub mod query;
pub mod selftest;
pub mod settings;
pub mod shell;
pub mod sys;
pub mod ui;
pub mod util;
pub mod volumes;
pub mod walk;
pub mod watch;
