//! Generate the Agent Substrate client from the vendored, unmodified proto.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("../../vendor/substrate");
    let proto_dir = root.join("pkg/proto/ateapipb");
    let proto = proto_dir.join("ateapi.proto");
    println!("cargo:rerun-if-changed={}", proto.display());

    // Use the pinned protoc and well-known types rather than whatever the host
    // has installed, so generated code depends only on Cargo.lock.
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    let well_known = protoc_bin_vendored::include_path()?;

    tonic_prost_build::configure()
        // The server trait exists for in-process contract tests; unimplemented
        // methods answer UNIMPLEMENTED rather than requiring every RPC.
        .generate_default_stubs(true)
        .compile_protos(&[proto], &[proto_dir, well_known])?;
    Ok(())
}
