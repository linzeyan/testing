//! Shared by the GUI (`apitool`) and the headless runner (`apitool-cli`).

pub mod app;
pub mod cli;
mod curl;
mod graphql;
mod grpc;
mod http;
mod loadtest;
mod mcp;
mod model;
mod net;
mod runner;
mod script;
pub mod store;
mod stream;
mod varedit;
