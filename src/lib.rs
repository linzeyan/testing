//! Shared by the GUI (`apitool`) and the headless runner (`apitool-cli`).

pub mod app;
mod auth;
pub mod cli;
mod codegen;
mod cookies;
mod curl;
mod docs;
mod graphql;
mod grpc;
mod http;
mod loadtest;
mod mcp;
mod mock;
mod model;
mod net;
mod postman;
mod runner;
mod script;
pub mod store;
mod stream;
mod varedit;
