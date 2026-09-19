//! Public extension vocabulary shared by telemetry rows.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::name::{
    InitialByteRule, PublicNameViolation, validate_public_name, validated_string_newtype,
};

const MAX_PUBLIC_EXTENSION_NAME_BYTES: usize = 64;

/// Plugin or skill named by eligible public telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::telemetry) enum ExtensionKind {
    Plugin,
    Skill,
}

impl ExtensionKind {
    /// Return the frozen version 1 wire label.
    #[must_use]
    pub(in crate::telemetry) const fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Skill => "skill",
        }
    }
}

/// Public source approved for version 1 extension telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::telemetry) enum PublicExtensionSource {
    SymposiumRecommendations,
    CratesIo,
}

impl PublicExtensionSource {
    /// Return the frozen version 1 wire label.
    #[must_use]
    pub(in crate::telemetry) const fn as_str(self) -> &'static str {
        match self {
            Self::SymposiumRecommendations => "symposium-recommendations",
            Self::CratesIo => "crates-io",
        }
    }
}

validated_string_newtype! {
    /// Public plugin or skill name accepted by the version 1 telemetry contract.
    pub(in crate::telemetry) struct PublicExtensionName {
        error = PublicExtensionNameError;
        validate = validate_public_extension_name;
        as_str_doc = "Return the validated extension name without changing its spelling.";
    }
}

fn validate_public_extension_name(value: &str) -> Result<(), PublicExtensionNameError> {
    validate_public_name(
        value,
        MAX_PUBLIC_EXTENSION_NAME_BYTES,
        InitialByteRule::Alphanumeric,
    )
    .map_err(PublicExtensionNameError::from)
}

/// Reason an extension name cannot enter public telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) enum PublicExtensionNameError {
    Empty,
    TooLong,
    NonAlphanumericFirstCharacter,
    UnsupportedCharacter,
}

impl From<PublicNameViolation> for PublicExtensionNameError {
    fn from(violation: PublicNameViolation) -> Self {
        match violation {
            PublicNameViolation::Empty => Self::Empty,
            PublicNameViolation::TooLong => Self::TooLong,
            PublicNameViolation::InvalidInitialByte => Self::NonAlphanumericFirstCharacter,
            PublicNameViolation::UnsupportedCharacter => Self::UnsupportedCharacter,
        }
    }
}

impl fmt::Display for PublicExtensionNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("public extension name must not be empty"),
            Self::TooLong => write!(
                formatter,
                "public extension name exceeds {MAX_PUBLIC_EXTENSION_NAME_BYTES} bytes"
            ),
            Self::NonAlphanumericFirstCharacter => formatter
                .write_str("public extension name must start with an ASCII letter or digit"),
            Self::UnsupportedCharacter => formatter.write_str(
                "public extension name may contain only ASCII letters, digits, hyphens, and underscores",
            ),
        }
    }
}

impl std::error::Error for PublicExtensionNameError {}

#[cfg(test)]
mod tests {
    use super::super::assert_contract_names_with_labels;
    use super::*;

    #[test]
    fn extension_kinds_round_trip_with_contract_names() {
        let cases = [
            (ExtensionKind::Plugin, "plugin"),
            (ExtensionKind::Skill, "skill"),
        ];

        assert_contract_names_with_labels(&cases, ExtensionKind::as_str);
    }

    #[test]
    fn public_extension_sources_round_trip_with_contract_names() {
        let cases = [
            (
                PublicExtensionSource::SymposiumRecommendations,
                "symposium-recommendations",
            ),
            (PublicExtensionSource::CratesIo, "crates-io"),
        ];

        assert_contract_names_with_labels(&cases, PublicExtensionSource::as_str);
    }

    #[test]
    fn extension_vocabulary_rejects_unknown_contract_names() {
        let kind = serde_json::from_str::<ExtensionKind>(r#""command""#);
        let source = serde_json::from_str::<PublicExtensionSource>(r#""user-plugins""#);

        assert!(kind.is_err());
        assert!(source.is_err());
    }

    #[test]
    fn public_extension_names_accept_the_contract_grammar() {
        for value in ["0", "Example-runtime_2", &"a".repeat(64)] {
            let name = value.parse::<PublicExtensionName>().unwrap();

            assert_eq!(name.as_str(), value);
        }
    }

    #[test]
    fn public_extension_names_reject_invalid_length() {
        let empty = "".parse::<PublicExtensionName>();
        let too_long = "a".repeat(65).parse::<PublicExtensionName>();

        assert_eq!(empty.unwrap_err(), PublicExtensionNameError::Empty);
        assert_eq!(too_long.unwrap_err(), PublicExtensionNameError::TooLong);
    }

    #[test]
    fn public_extension_names_require_an_ascii_alphanumeric_first_byte() {
        for value in ["-extension", "_extension", "\u{e9}xtension"] {
            let result = value.parse::<PublicExtensionName>();

            assert_eq!(
                result.unwrap_err(),
                PublicExtensionNameError::NonAlphanumericFirstCharacter
            );
        }
    }

    #[test]
    fn public_extension_names_reject_unsupported_characters() {
        for value in [
            "extension.name",
            "extension name",
            "extension/name",
            "a\u{e9}",
        ] {
            let result = value.parse::<PublicExtensionName>();

            assert_eq!(
                result.unwrap_err(),
                PublicExtensionNameError::UnsupportedCharacter
            );
        }
    }

    #[test]
    fn public_extension_names_round_trip_without_normalization() {
        let name = "Example-runtime_2".parse::<PublicExtensionName>().unwrap();

        let encoded = serde_json::to_string(&name).unwrap();
        let decoded = serde_json::from_str::<PublicExtensionName>(&encoded).unwrap();

        assert_eq!(encoded, r#""Example-runtime_2""#);
        assert_eq!(decoded, name);
    }

    #[test]
    fn public_extension_name_validation_runs_during_deserialization() {
        let invalid = serde_json::from_str::<PublicExtensionName>(r#""extension.name""#);

        assert!(invalid.is_err());
    }
}
