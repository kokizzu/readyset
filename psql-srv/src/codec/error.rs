use std::fmt;
use std::marker::{Send, Sync};
use std::num::{ParseFloatError, ParseIntError, TryFromIntError};
use std::str::Utf8Error;

use hex::FromHexError;
use postgres_types::Type;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("encoding error: {0}")]
    EncodingError(#[from] Utf8Error),

    #[error("incorrect parameter count: {0}")]
    IncorrectParameterCount(usize),

    #[error("invalid byte sequence for encoding \"UTF8\": 0x00")]
    InvalidUtf8,

    // Conversion for errors resulting from postgres_types::FromSql.
    #[error("invalid binary data value: {0}")]
    InvalidBinaryDataValue(#[from] Box<dyn std::error::Error + Send + Sync>),

    #[error("invalid boolean: {0}")]
    InvalidTextBooleanError(String),

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
    InvalidTextMacAddressValue(eui48::ParseError),

    #[error("invalid text ip address value: {0}")]
    InvalidTextIpAddressValue(cidr::errors::NetworkParseError),

    #[error("invalid text uuid value: {0}")]
    InvalidTextUuidValue(uuid::Error),

    #[error("invalid text json value: {0}")]
    InvalidTextJsonValue(serde_json::Error),

    #[error("invalid text bit vector value: {0}")]
    InvalidTextBitVectorValue(String),

    #[error("invalid array value: {0}")]
    InvalidArrayValue(String),

    #[error("invalid numeric value: {0}")]
    InvalidNumericValue(#[from] readyset_decimal::ReadysetDecimalError),

    #[error("unknown enum variant: {0}")]
    UnknownEnumVariant(String),

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
