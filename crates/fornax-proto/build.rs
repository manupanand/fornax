// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Compiles `proto/fornax/runner/v1/runner.proto` without `protoc`: `protox` (pure Rust)
//! produces the `FileDescriptorSet`, `tonic-prost-build` generates messages, client and server.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/fornax/runner/v1/runner.proto";
    println!("cargo:rerun-if-changed={proto}");
    let fds = protox::compile([proto], ["proto"])?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(fds)?;
    Ok(())
}
