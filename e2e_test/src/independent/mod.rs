#![cfg(test)]

mod api_router_tests;
mod dump;
mod grpc_gzip_test;
mod https_api_tests;
mod restart_tests;
mod version_check;

#[cfg(feature = "coordinator_e2e_test")]
mod coordinator_tests;

mod chaos_tests;

mod auth_tests;
mod client_tests;
mod computing_storage_tests;
mod flush_tests;
mod replica_test;
mod stream_computing;
