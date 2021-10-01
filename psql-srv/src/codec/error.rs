use eui48::ParseError;
use hex::FromHexError;
use postgres_types::Type;
use std::ffi::FromBytesWithNulError;
use std::fmt;
use std::marker::{Send, Sync};
use std::num::{ParseFloatError, ParseIntError, TryFromIntError};
use std::str::Utf8Error;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("encoding error: {0}")]
    EncodingError(#[from] Utf8Error),

    #[error("incorrect parameter count: {0}")]
    IncorrectParameterCount(usize),

    #[error("invalid c string: {0}")]
    InvalidCStr(#[from] FromBytesWithNulError),

    #[error("invalid numeric: {0}")]
    InvalidNumeric(#[from] rust_decimal::Error),

    // Conversion for errors resulting from postgres_types::FromSql.
    #[error("invalid binary data value: {0}")]
    InvalidBinaryDataValue(#[from] Box<dyn std::error::Error + Send + Sync>),

    #[error("invalid format: {0}")]
    InvalidFormat(i16),

    #[error("invalid integer: {0}")]
    InvalidInteger(#[from] TryFromIntError),

    #[error("invalid text float value: {0}")]
    InvalidTextFloatValue(#[from] ParseFloatError),

    #[error("invalid text integer value: {0}")]
    InvalidTextIntegerValue(#[from] ParseIntError),

    #[error("invalid text timestamp value: {0}")]
    InvalidTextTimestampValue(#[from] chrono::ParseError),

    #[error("invalid text byte array value: {0}")]
    InvalidTextByteArrayValue(FromHexError),

    #[error("invalid text mac address value: {0}")]
    InvalidTextMacAddressValue(ParseError),

    #[error("invalid text uuid value: {0}")]
    InvalidTextUuidValue(uuid::Error),

    #[error("invalid text json value: {0}")]
    InvalidTextJsonValue(serde_json::Error),

    #[error("invalid text bit vector value: {0}")]
    InvalidTextBitVectorValue(String),

    #[error("invalid type: {0}")]
    InvalidType(u32),

    #[error("internal error: {0}")]
    InternalError(String),

    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("unknown prepared statement: {0}")]
    UnknownPreparedStatement(String),

    #[error("unexpected message end")]
    UnexpectedMessageEnd,

    #[error("unexpected value: {0}")]
    UnexpectedValue(u8),

    #[error("unsupported message: {0}")]
    UnsupportedMessage(u8),

    #[error("unsupported type: {0}")]
    UnsupportedType(Type),
}

#[derive(Debug, Error)]
pub enum EncodeError {
    #[error("encoding error: {0}")]
    EncodingError(#[from] Utf8Error),

    // Conversion for errors resulting from postgres_types::ToSql.
    #[error("invalid binary data value: {0}")]
    InvalidBinaryDataValue(#[from] Box<dyn std::error::Error + Send + Sync>),

    #[error("invalid text data value: {0}")]
    InvalidTextDataValue(#[from] fmt::Error),

    #[error("invalid integer: {0}")]
    InvalidInteger(#[from] TryFromIntError),

    #[error("internal error: {0}")]
    InternalError(String),

    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),
}
