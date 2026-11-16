//! The Mojang identity document: [`GameProfile`] and its [`Property`] list.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One named, opaque property attached to a [`GameProfile`].
///
/// Properties carry out-of-band identity data Mojang attaches to an account,
/// most notably `textures` (skin and cape). `value` is an opaque,
/// base64-encoded blob as far as this crate is concerned; this crate never
/// decodes it. `signature` holds the Mojang-issued signature over `value`
/// when the property is signed, and is absent otherwise — Mojang omits the
/// key from unsigned properties, so the field is skipped when serializing
/// without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Property {
    /// The property name, e.g. `textures`.
    pub name: String,
    /// The opaque property value (typically base64). Never decoded here.
    pub value: String,
    /// The signature over `value`, if this property is signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl Property {
    /// Create a signed property (e.g. one issued by Mojang with a texture).
    pub fn signed(
        name: impl Into<String>,
        value: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            signature: Some(signature.into()),
        }
    }

    /// Create an unsigned property.
    pub fn unsigned(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            signature: None,
        }
    }
}

/// The identity document for a Minecraft account, as handed to the proxy
/// (online mode: by Mojang's session server; offline mode: derived locally
/// with [`crate::offline_uuid`]).
///
/// `uuid` is stable per account; `name` is the *current* name and can
/// change on Mojang's side, so consumers that cache profiles must treat the
/// UUID as the key, not the name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameProfile {
    /// The account UUID.
    pub id: Uuid,
    /// The current account name.
    pub name: String,
    /// Opaque account properties attached by Mojang (usually `textures`).
    /// Absent on the wire as far as this crate is concerned: a missing key
    /// deserializes to an empty list.
    #[serde(default)]
    pub properties: Vec<Property>,
}

impl GameProfile {
    /// Create a profile without any properties.
    pub fn new(id: Uuid, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            properties: Vec::new(),
        }
    }
}
