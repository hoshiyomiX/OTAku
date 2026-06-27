fn main() {
    // Rebuild when proto files change.
    //
    // The protobuf schema (proto/update_metadata.proto) is currently consumed
    // by hand-written prost structs in src/proto.rs — no codegen step runs at
    // build time. If/when proto codegen is enabled, replace this file with:
    //
    //   prost_build::Config::new()
    //       .bytes(&[
    //           ".otaku.InstallOperation.data_sha256_hash",
    //           ".otaku.PartitionInfo.hash",
    //           ".otaku.Signature.data",
    //       ])
    //       .compile_protos(
    //           &["proto/update_metadata.proto"],
    //           &["proto/"],
    //       )
    //       .unwrap();
    println!("cargo:rerun-if-changed=proto/");
}
