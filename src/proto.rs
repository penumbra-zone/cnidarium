// Autogen code isn't clippy clean:
#[allow(clippy::unwrap_used)]
pub mod v1 {
    include!("gen/penumbra.cnidarium.v1.rs");
    include!("gen/penumbra.cnidarium.v1.serde.rs");
}

// https://github.com/penumbra-zone/penumbra/issues/3038#issuecomment-1722534133
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("gen/proto_descriptor.bin.no_lfs");
