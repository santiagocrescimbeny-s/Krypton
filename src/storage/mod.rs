//! Off-chain metadata engine (IPFS / Pinata uploads, Metaplex-compatible JSON).

pub mod metadata;

pub use metadata::{
    ImageAsset, ImageFormat, IpfsCid, MetadataDocument, MetadataUploader, PinataConfig,
    TokenMetadata, UploadedMetadata,
};
