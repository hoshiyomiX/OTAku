//! Protobuf message definitions for AOSP update_engine payload format.
//!
//! Uses prost with hand-defined structs (matching the Python protobuf.py approach).
//! Phase 2 may switch to prost-build codegen from .proto files.
//!
//! Reference: system/update_engine/update_metadata.proto

use prost::Message;

// ---------------------------------------------------------------------------
//  AOSP InstallOperation type enum
// ---------------------------------------------------------------------------

pub const OP_REPLACE: u32 = 0;
pub const OP_REPLACE_BZ: u32 = 12;
pub const OP_REPLACE_XZ: u32 = 8;
pub const OP_PUIGZIP: u32 = 14;
pub const OP_BROTLI_BSDIFF: u32 = 23;
pub const OP_ZERO: u32 = 21;
pub const OP_DISCARD: u32 = 22;

pub const OP_TYPE_NAMES: &[(u32, &str)] = &[
    (OP_REPLACE, "REPLACE"),
    (OP_REPLACE_BZ, "REPLACE_BZ"),
    (OP_REPLACE_XZ, "REPLACE_XZ"),
    (1, "MOVE"),
    (3, "SOURCE_COPY"),
    (4, "SOURCE_BSDIFF"),
    (OP_PUIGZIP, "PUIGZIP"),
    (OP_BROTLI_BSDIFF, "BROTLI_BZ"),
    (OP_ZERO, "ZERO"),
    (OP_DISCARD, "DISCARD"),
    (13, "REPLACE_BROT"),
];

// ---------------------------------------------------------------------------
//  Protobuf message structs (hand-defined, prost-compatible)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct Extent {
    #[prost(uint64, tag = "1")]
    pub start_block: u64,
    #[prost(uint64, tag = "2")]
    pub num_blocks: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct PartitionInfo {
    #[prost(uint64, tag = "1")]
    pub partition_size: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InstallOperation {
    #[prost(uint32, tag = "1")]
    pub r#type: u32,
    #[prost(uint64, tag = "2")]
    pub data_offset: u64,
    #[prost(uint64, tag = "3")]
    pub data_length: u64,
    #[prost(uint32, tag = "5")]
    pub src_length: u32,
    #[prost(message, repeated, tag = "6")]
    pub src_extents: Vec<Extent>,
    #[prost(message, repeated, tag = "7")]
    pub dst_extents: Vec<Extent>,
    #[prost(uint32, tag = "8")]
    pub dst_length: u32,
    #[prost(bytes = "vec", tag = "9")]
    pub data_sha256_hash: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PartitionUpdate {
    #[prost(string, tag = "1")]
    pub partition_name: String,
    #[prost(bool, tag = "2")]
    pub run_postinstall: bool,
    #[prost(message, repeated, tag = "3")]
    pub install_operations: Vec<InstallOperation>,
    #[prost(message, optional, tag = "4")]
    pub old_partition_info: Option<PartitionInfo>,
    #[prost(message, optional, tag = "5")]
    pub new_partition_info: Option<PartitionInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Signature {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub unpadded_data_size: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Signatures {
    #[prost(message, repeated, tag = "1")]
    pub signatures: Vec<Signature>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeltaArchiveManifest {
    #[prost(message, optional, tag = "1")]
    pub signatures: Option<Signatures>,
    #[prost(message, repeated, tag = "2")]
    pub install_operations: Vec<InstallOperation>,
    #[prost(uint64, tag = "3")]
    pub block_size: u64,
    #[prost(uint32, tag = "4")]
    pub minor_version: u32,
    #[prost(message, repeated, tag = "5")]
    pub partitions: Vec<PartitionUpdate>,
    #[prost(uint64, tag = "6")]
    pub max_timestamp: u64,
    #[prost(bool, tag = "11")]
    pub has_source_metadata: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct PayloadHeader {
    #[prost(uint64, tag = "1")]
    pub version: u64,
    #[prost(uint64, tag = "2")]
    pub manifest_len: u64,
    #[prost(uint64, tag = "3")]
    pub metadata_signature_len: u64,
    #[prost(uint32, tag = "4")]
    pub minor_version: u32,
}
