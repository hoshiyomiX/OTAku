fn main() {
    // Rebuild when proto files change
    println!("cargo:rerun-if-changed=proto/");

    // Phase 2 will add actual protobuf compilation here.
    // For now, we use hand-defined prost structs in src/proto.rs.
    //
    // When ready:
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
}
