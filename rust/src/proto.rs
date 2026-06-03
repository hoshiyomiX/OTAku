//! Protobuf message definitions for AOSP update_engine payload format.
//!
//! Uses prost with hand-defined structs matching the Python protobuf.py approach.
//! All structs implement `prost::Message` for zero-copy encode/decode.
//!
//! Reference: system/update_engine/update_metadata.proto

use prost::Message;

// ---------------------------------------------------------------------------
//  AOSP InstallOperation type enum
// ---------------------------------------------------------------------------

pub const OP_REPLACE: u32 = 0;
pub const OP_MOVE: u32 = 1;
pub const OP_BSDIFF: u32 = 2;
pub const OP_SOURCE_COPY: u32 = 3;
pub const OP_SOURCE_BSDIFF: u32 = 4;
pub const OP_REPLACE_XZ: u32 = 8;
pub const OP_REPLACE_BROT: u32 = 13;
pub const OP_REPLACE_BZ: u32 = 12;
pub const OP_PUIGZIP: u32 = 14;
pub const OP_ZERO: u32 = 21;
pub const OP_DISCARD: u32 = 22;
pub const OP_BROTLI_BSDIFF: u32 = 23;

pub const OP_TYPE_NAMES: &[(u32, &str)] = &[
    (OP_REPLACE, "REPLACE"),
    (OP_MOVE, "MOVE"),
    (OP_BSDIFF, "BSDIFF"),
    (OP_SOURCE_COPY, "SOURCE_COPY"),
    (OP_SOURCE_BSDIFF, "SOURCE_BSDIFF"),
    (OP_REPLACE_XZ, "REPLACE_XZ"),
    (OP_REPLACE_BROT, "REPLACE_BROT"),
    (OP_REPLACE_BZ, "REPLACE_BZ"),
    (OP_PUIGZIP, "PUIGZIP"),
    (OP_ZERO, "ZERO"),
    (OP_DISCARD, "DISCARD"),
    (OP_BROTLI_BSDIFF, "BROTLI_BZ"),
];

/// Get the human-readable name for an InstallOperation type.
pub fn op_type_name(op_type: u32) -> &'static str {
    OP_TYPE_NAMES
        .iter()
        .find(|(t, _)| *t == op_type)
        .map(|(_, name)| *name)
        .unwrap_or("UNKNOWN")
}

// ---------------------------------------------------------------------------
//  Protobuf message structs (hand-defined, prost-compatible)
//  Field numbers match AOSP update_metadata.proto exactly.
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
    #[prost(message, optional, tag = "6")]
    pub postinstall_hook: Option<PostInstallHook>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PostInstallHook {
    // Placeholder — AOSP has fields we don't need for DD mode
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
pub struct DynamicPartitionMetadata {
    // Placeholder — AOSP has fields we don't need for DD mode
}

#[derive(Clone, PartialEq, Message)]
pub struct DynamicPartitionGroup {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(uint64, tag = "2")]
    pub size: u64,
    #[prost(string, repeated, tag = "3")]
    pub partition_names: Vec<String>,
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
    #[prost(message, optional, tag = "7")]
    pub dynamic_partition_metadata: Option<DynamicPartitionMetadata>,
    #[prost(bool, tag = "8")]
    pub minor_version_applied: bool,
    #[prost(message, repeated, tag = "9")]
    pub groups: Vec<DynamicPartitionGroup>,
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

// ---------------------------------------------------------------------------
//  Decode helpers — high-level parsing from raw protobuf bytes
// ---------------------------------------------------------------------------

/// Decode a PayloadHeader from raw bytes.
pub fn decode_payload_header(data: &[u8]) -> Result<PayloadHeader, String> {
    PayloadHeader::decode(data).map_err(|e| format!("Failed to decode PayloadHeader: {}", e))
}

/// Decode a DeltaArchiveManifest from raw bytes.
pub fn decode_manifest(data: &[u8]) -> Result<DeltaArchiveManifest, String> {
    DeltaArchiveManifest::decode(data).map_err(|e| format!("Failed to decode manifest: {}", e))
}

/// Decode a single InstallOperation from raw bytes.
pub fn decode_install_operation(data: &[u8]) -> Result<InstallOperation, String> {
    InstallOperation::decode(data)
        .map_err(|e| format!("Failed to decode InstallOperation: {}", e))
}

/// Decode a PartitionUpdate from raw bytes.
pub fn decode_partition_update(data: &[u8]) -> Result<PartitionUpdate, String> {
    PartitionUpdate::decode(data).map_err(|e| format!("Failed to decode PartitionUpdate: {}", e))
}

/// Decode an Extent from raw bytes.
pub fn decode_extent(data: &[u8]) -> Result<Extent, String> {
    Extent::decode(data).map_err(|e| format!("Failed to decode Extent: {}", e))
}

/// Decode a PartitionInfo from raw bytes.
pub fn decode_partition_info(data: &[u8]) -> Result<PartitionInfo, String> {
    PartitionInfo::decode(data).map_err(|e| format!("Failed to decode PartitionInfo: {}", e))
}

// ---------------------------------------------------------------------------
//  Encode helpers — serialize structs to bytes
// ---------------------------------------------------------------------------

/// Encode a PayloadHeader to bytes.
pub fn encode_payload_header(header: &PayloadHeader) -> Vec<u8> {
    let mut buf = Vec::with_capacity(header.encoded_len());
    header.encode(&mut buf).expect("prost encode should not fail");
    buf
}

/// Encode a DeltaArchiveManifest to bytes.
pub fn encode_manifest(manifest: &DeltaArchiveManifest) -> Vec<u8> {
    let mut buf = Vec::with_capacity(manifest.encoded_len());
    manifest.encode(&mut buf).expect("prost encode should not fail");
    buf
}

/// Encode an InstallOperation to bytes.
pub fn encode_install_operation(op: &InstallOperation) -> Vec<u8> {
    let mut buf = Vec::with_capacity(op.encoded_len());
    op.encode(&mut buf).expect("prost encode should not fail");
    buf
}

/// Encode a PartitionUpdate to bytes.
pub fn encode_partition_update(part: &PartitionUpdate) -> Vec<u8> {
    let mut buf = Vec::with_capacity(part.encoded_len());
    part.encode(&mut buf).expect("prost encode should not fail");
    buf
}

/// Encode a PartitionInfo to bytes.
pub fn encode_partition_info(info: &PartitionInfo) -> Vec<u8> {
    let mut buf = Vec::with_capacity(info.encoded_len());
    info.encode(&mut buf).expect("prost encode should not fail");
    buf
}

/// Encode an Extent to bytes.
pub fn encode_extent(extent: &Extent) -> Vec<u8> {
    let mut buf = Vec::with_capacity(extent.encoded_len());
    extent.encode(&mut buf).expect("prost encode should not fail");
    buf
}

/// Encode a Signatures message to bytes.
pub fn encode_signatures(sigs: &Signatures) -> Vec<u8> {
    let mut buf = Vec::with_capacity(sigs.encoded_len());
    sigs.encode(&mut buf).expect("prost encode should not fail");
    buf
}

// ---------------------------------------------------------------------------
//  Builder helpers — construct common message types
// ---------------------------------------------------------------------------

/// Build an Extent with the given start_block and num_blocks.
pub fn build_extent(start_block: u64, num_blocks: u64) -> Extent {
    Extent {
        start_block,
        num_blocks,
    }
}

/// Build a PartitionInfo with the given size and hash.
pub fn build_partition_info(partition_size: u64, hash: Vec<u8>) -> PartitionInfo {
    PartitionInfo {
        partition_size,
        hash,
    }
}

/// Build an InstallOperation for a REPLACE-type operation.
pub fn build_replace_operation(
    op_type: u32,
    data_offset: u64,
    data_length: u64,
    dst_extents: Vec<Extent>,
    data_sha256_hash: Vec<u8>,
    dst_length: u32,
) -> InstallOperation {
    InstallOperation {
        r#type: op_type,
        data_offset,
        data_length,
        src_length: 0,
        src_extents: Vec::new(),
        dst_extents,
        dst_length,
        data_sha256_hash,
    }
}

/// Build a PartitionUpdate for a single-operation partition.
pub fn build_partition_update(
    partition_name: String,
    install_operations: Vec<InstallOperation>,
    new_partition_info: Option<PartitionInfo>,
) -> PartitionUpdate {
    PartitionUpdate {
        partition_name,
        run_postinstall: false,
        install_operations,
        old_partition_info: None,
        new_partition_info,
        postinstall_hook: None,
    }
}

/// Build a DeltaArchiveManifest with the given partitions and settings.
pub fn build_manifest(
    block_size: u64,
    minor_version: u32,
    partitions: Vec<PartitionUpdate>,
) -> DeltaArchiveManifest {
    DeltaArchiveManifest {
        signatures: None,
        install_operations: Vec::new(),
        block_size,
        minor_version,
        partitions,
        max_timestamp: 0,
        dynamic_partition_metadata: None,
        minor_version_applied: false,
        groups: Vec::new(),
        has_source_metadata: false,
    }
}

/// Build a PayloadHeader.
pub fn build_payload_header(
    version: u64,
    manifest_len: u64,
    metadata_signature_len: u64,
    minor_version: u32,
) -> PayloadHeader {
    PayloadHeader {
        version,
        manifest_len,
        metadata_signature_len,
        minor_version,
    }
}

// ---------------------------------------------------------------------------
//  JSON serialization for JNI results
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};

/// JSON-serializable representation of a parsed payload header.
#[derive(Debug, Serialize, Deserialize)]
pub struct PayloadHeaderJson {
    pub version: u64,
    pub manifest_len: u64,
    pub metadata_signature_len: u64,
    pub minor_version: u32,
}

impl From<&PayloadHeader> for PayloadHeaderJson {
    fn from(h: &PayloadHeader) -> Self {
        PayloadHeaderJson {
            version: h.version,
            manifest_len: h.manifest_len,
            metadata_signature_len: h.metadata_signature_len,
            minor_version: h.minor_version,
        }
    }
}

/// JSON-serializable representation of an InstallOperation.
#[derive(Debug, Serialize, Deserialize)]
pub struct InstallOperationJson {
    #[serde(rename = "type")]
    pub op_type: u32,
    pub type_name: String,
    pub data_offset: u64,
    pub data_length: u64,
    pub src_length: u32,
    pub dst_length: u32,
    pub dst_extents: Vec<ExtentJson>,
    pub src_extents: Vec<ExtentJson>,
    pub data_sha256_hash: String,
}

impl From<&InstallOperation> for InstallOperationJson {
    fn from(op: &InstallOperation) -> Self {
        InstallOperationJson {
            op_type: op.r#type,
            type_name: op_type_name(op.r#type).to_string(),
            data_offset: op.data_offset,
            data_length: op.data_length,
            src_length: op.src_length,
            dst_length: op.dst_length,
            dst_extents: op.dst_extents.iter().map(ExtentJson::from).collect(),
            src_extents: op.src_extents.iter().map(ExtentJson::from).collect(),
            data_sha256_hash: op
                .data_sha256_hash
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect(),
        }
    }
}

/// JSON-serializable representation of an Extent.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExtentJson {
    pub start_block: u64,
    pub num_blocks: u64,
}

impl From<&Extent> for ExtentJson {
    fn from(e: &Extent) -> Self {
        ExtentJson {
            start_block: e.start_block,
            num_blocks: e.num_blocks,
        }
    }
}

/// JSON-serializable representation of a PartitionInfo.
#[derive(Debug, Serialize, Deserialize)]
pub struct PartitionInfoJson {
    pub partition_size: u64,
    pub hash: String,
}

impl From<&PartitionInfo> for PartitionInfoJson {
    fn from(info: &PartitionInfo) -> Self {
        PartitionInfoJson {
            partition_size: info.partition_size,
            hash: info.hash.iter().map(|b| format!("{:02x}", b)).collect(),
        }
    }
}

/// JSON-serializable representation of a PartitionUpdate.
#[derive(Debug, Serialize, Deserialize)]
pub struct PartitionUpdateJson {
    pub partition_name: String,
    pub run_postinstall: bool,
    pub install_operations: Vec<InstallOperationJson>,
    pub new_partition_info: Option<PartitionInfoJson>,
    pub old_partition_info: Option<PartitionInfoJson>,
}

impl From<&PartitionUpdate> for PartitionUpdateJson {
    fn from(p: &PartitionUpdate) -> Self {
        PartitionUpdateJson {
            partition_name: p.partition_name.clone(),
            run_postinstall: p.run_postinstall,
            install_operations: p
                .install_operations
                .iter()
                .map(InstallOperationJson::from)
                .collect(),
            new_partition_info: p.new_partition_info.as_ref().map(PartitionInfoJson::from),
            old_partition_info: p.old_partition_info.as_ref().map(PartitionInfoJson::from),
        }
    }
}

/// JSON-serializable representation of a full parsed payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct ParsedPayloadJson {
    pub header: PayloadHeaderJson,
    pub manifest: ManifestJson,
    pub data_offset: u64,
    pub file_size: u64,
}

/// JSON-serializable representation of a manifest.
#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestJson {
    pub block_size: u64,
    pub minor_version: u32,
    pub partitions: Vec<PartitionUpdateJson>,
    pub max_timestamp: u64,
    pub has_source_metadata: bool,
}

impl From<&DeltaArchiveManifest> for ManifestJson {
    fn from(m: &DeltaArchiveManifest) -> Self {
        ManifestJson {
            block_size: m.block_size,
            minor_version: m.minor_version,
            partitions: m.partitions.iter().map(PartitionUpdateJson::from).collect(),
            max_timestamp: m.max_timestamp,
            has_source_metadata: m.has_source_metadata,
        }
    }
}

// ---------------------------------------------------------------------------
//  Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_extent() {
        let ext = build_extent(0, 1024);
        let encoded = encode_extent(&ext);
        let decoded = decode_extent(&encoded).unwrap();
        assert_eq!(ext, decoded);
    }

    #[test]
    fn test_encode_decode_partition_info() {
        let info = build_partition_info(4096, vec![0xAB; 32]);
        let encoded = encode_partition_info(&info);
        let decoded = decode_partition_info(&encoded).unwrap();
        assert_eq!(info, decoded);
    }

    #[test]
    fn test_encode_decode_install_operation() {
        let op = build_replace_operation(
            OP_REPLACE_XZ,
            0,
            12345,
            vec![build_extent(0, 1024)],
            vec![0xCD; 32],
            4096,
        );
        let encoded = encode_install_operation(&op);
        let decoded = decode_install_operation(&encoded).unwrap();
        assert_eq!(op, decoded);
    }

    #[test]
    fn test_encode_decode_manifest() {
        let manifest = build_manifest(
            4096,
            0,
            vec![build_partition_update(
                "boot".to_string(),
                vec![build_replace_operation(
                    OP_REPLACE,
                    0,
                    8192,
                    vec![build_extent(0, 2)],
                    vec![0xFF; 32],
                    8192,
                )],
                Some(build_partition_info(8192, vec![0xFF; 32])),
            )],
        );
        let encoded = encode_manifest(&manifest);
        let decoded = decode_manifest(&encoded).unwrap();
        assert_eq!(manifest, decoded);
    }

    #[test]
    fn test_encode_decode_payload_header() {
        let header = build_payload_header(2, 500, 0, 0);
        let encoded = encode_payload_header(&header);
        let decoded = decode_payload_header(&encoded).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_op_type_name() {
        assert_eq!(op_type_name(OP_REPLACE), "REPLACE");
        assert_eq!(op_type_name(OP_REPLACE_XZ), "REPLACE_XZ");
        assert_eq!(op_type_name(OP_PUIGZIP), "PUIGZIP");
        assert_eq!(op_type_name(99), "UNKNOWN");
    }

    #[test]
    fn test_json_serialization() {
        let op = build_replace_operation(
            OP_REPLACE_XZ,
            100,
            5000,
            vec![build_extent(0, 50)],
            vec![0xAA; 32],
            204800,
        );
        let json_op = InstallOperationJson::from(&op);
        let json_str = serde_json::to_string(&json_op).unwrap();
        assert!(json_str.contains("REPLACE_XZ"));
        assert!(json_str.contains("\"type\":8"));
    }
}
