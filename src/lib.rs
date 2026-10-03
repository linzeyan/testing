//! Shared by the GUI (`apitool`) and the headless runner (`apitool-cli`).

pub mod app;
mod auth;
pub mod cli;
mod codegen;
mod cookies;
mod curl;
mod docs;
mod fake;
mod graphql;
mod grpc;
mod http;
mod jsonpath;
mod loadtest;
mod mcp;
mod mock;
mod model;
mod mqtt;
mod net;
mod postman;
mod runner;
mod script;
mod sigv4;
pub mod store;
mod stream;
mod varedit;
