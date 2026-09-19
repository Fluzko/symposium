//! Validation shared by public names in the telemetry contract.

/// Define a string newtype whose constructors and deserializer enforce one
/// validation function.
macro_rules! validated_string_newtype {
    (
        $(#[$metadata:meta])*
        $visibility:vis struct $name:ident {
            error = $error:ty;
            validate = $validate:path;
            as_str_doc = $as_str_doc:literal;
        }
    ) => {
        $(#[$metadata])*
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(try_from = "String")]
        $visibility struct $name(String);

        impl $name {
            #[doc = $as_str_doc]
            #[must_use]
            $visibility fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = $error;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                ($validate)(&value)?;
                Ok(Self(value))
            }
        }

        impl std::str::FromStr for $name {
            type Err = $error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                ($validate)(value)?;
                Ok(Self(value.to_owned()))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

pub(super) use validated_string_newtype;

/// Rule applied to the first byte of a public name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitialByteRule {
    Alphabetic,
    Alphanumeric,
}

/// Structural reason a public name fails its versioned grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicNameViolation {
    Empty,
    TooLong,
    InvalidInitialByte,
    UnsupportedCharacter,
}

/// Validate the common ASCII shape of a versioned public telemetry name.
pub(super) fn validate_public_name(
    value: &str,
    maximum_bytes: usize,
    initial_byte_rule: InitialByteRule,
) -> Result<(), PublicNameViolation> {
    let Some((first, rest)) = value.as_bytes().split_first() else {
        return Err(PublicNameViolation::Empty);
    };

    if value.len() > maximum_bytes {
        return Err(PublicNameViolation::TooLong);
    }

    let initial_byte_is_valid = match initial_byte_rule {
        InitialByteRule::Alphabetic => first.is_ascii_alphabetic(),
        InitialByteRule::Alphanumeric => first.is_ascii_alphanumeric(),
    };
    if !initial_byte_is_valid {
        return Err(PublicNameViolation::InvalidInitialByte);
    }

    if !rest
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(PublicNameViolation::UnsupportedCharacter);
    }

    Ok(())
}
