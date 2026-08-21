//! The [Agent Plugins 1.0.0](https://agent-plugins.org/) manifest.
//!
//! Only the fields symposium emits are modelled. The name grammar is the
//! format's, not ours: agent plugin names are narrower than symposium plugin
//! names (which are crate names or free-form manifest strings), so a name has
//! to be slugged before it can be written.

use serde::Serialize;

pub const SCHEMA_URL: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

const MAX_NAME_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Manifest {
    #[serde(rename = "$schema")]
    pub schema: &'static str,
    pub name: String,
    /// Always written, even though the format allows omitting it: Codex keys its
    /// plugin cache directory on the version, and defaults a version-less plugin
    /// to `1.0.0` of its own accord. Emitting one ourselves means the cache path
    /// is the value we wrote rather than another tool's default.
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Manifest {
    pub fn new(name: String, version: String, description: Option<String>) -> Self {
        Self {
            schema: SCHEMA_URL,
            name,
            version,
            description,
        }
    }

    pub fn to_json(&self) -> String {
        let mut json = serde_json::to_string_pretty(self).expect("manifest always serializes");
        json.push('\n');
        json
    }
}

/// Gemini CLI reads its own manifest name, carrying just the identity. The
/// directory is otherwise the same, so this is a second file rather than a
/// second layout.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiExtension {
    pub name: String,
    /// Required here, unlike in the Agent Plugins manifest.
    pub version: String,
}

impl GeminiExtension {
    pub fn new(name: String, version: String) -> Self {
        Self { name, version }
    }

    pub fn to_json(&self) -> String {
        let mut json = serde_json::to_string_pretty(self).expect("manifest always serializes");
        json.push('\n');
        json
    }
}

/// The marketplace manifest at a staging root: the index Claude Code, Codex, and
/// Copilot all read to discover the plugins under it. Written at
/// `.claude-plugin/marketplace.json`, which is the one path all three accept.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Marketplace {
    pub name: String,
    pub owner: MarketplaceOwner,
    pub plugins: Vec<MarketplaceEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarketplaceOwner {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarketplaceEntry {
    pub name: String,
    /// Relative to the marketplace root.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Marketplace {
    pub fn new(name: String, plugins: Vec<MarketplaceEntry>) -> Self {
        Self {
            name,
            owner: MarketplaceOwner {
                name: "symposium".to_string(),
            },
            plugins,
        }
    }

    pub fn to_json(&self) -> String {
        let mut json = serde_json::to_string_pretty(self).expect("marketplace always serializes");
        json.push('\n');
        json
    }
}

/// `^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$`, 1 to 64 characters.
pub fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = name.chars();
    if !chars.next().is_some_and(alnum) {
        return false;
    }
    if !name.chars().next_back().is_some_and(alnum) {
        return false;
    }
    name.chars().all(|c| alnum(c) || c == '.' || c == '-')
}

/// Convert a symposium plugin name into a valid manifest name, or `None` when
/// nothing legal survives.
///
/// Two distinct names can slug to the same result (`foo_bar` and `foo-bar`),
/// which is why callers disambiguate the *slug* rather than the original name.
pub fn slug(name: &str) -> Option<String> {
    let lowered: String = name
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let trimmed = trim_to_alnum(&lowered);
    let capped = if trimmed.len() > MAX_NAME_LEN {
        trim_to_alnum(&trimmed[..MAX_NAME_LEN])
    } else {
        trimmed
    };

    (!capped.is_empty()).then_some(capped)
}

fn trim_to_alnum(s: &str) -> String {
    s.trim_matches(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalizes_symposium_names() {
        assert_eq!(slug("pdf-tools").as_deref(), Some("pdf-tools"));
        assert_eq!(slug("my_crate").as_deref(), Some("my-crate"));
        assert_eq!(slug("Serde Guidance").as_deref(), Some("serde-guidance"));
        assert_eq!(
            slug("dev.symposium.tools").as_deref(),
            Some("dev.symposium.tools")
        );
        assert_eq!(
            slug("-leading-and-trailing-").as_deref(),
            Some("leading-and-trailing")
        );
        assert_eq!(slug("_"), None);
        assert_eq!(slug(""), None);
    }

    #[test]
    fn slug_output_is_always_a_valid_name() {
        for name in [
            "pdf-tools",
            "my_crate",
            "Serde Guidance",
            "-leading-",
            "UPPER",
            "a",
            &"x".repeat(200),
            &format!("{}_", "y".repeat(70)),
        ] {
            let slugged = slug(name).expect("slug");
            assert!(
                is_valid_name(&slugged),
                "slug({name:?}) produced invalid name {slugged:?}"
            );
        }
    }

    #[test]
    fn name_grammar_rejects_what_the_format_rejects() {
        assert!(is_valid_name("a"));
        assert!(is_valid_name("pdf-tools"));
        assert!(is_valid_name("a.b-c9"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("trail-"));
        assert!(!is_valid_name("Upper"));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name("under_score"));
        assert!(!is_valid_name(&"x".repeat(MAX_NAME_LEN + 1)));
    }

    #[test]
    fn the_gemini_manifest_carries_only_its_own_fields() {
        let json: serde_json::Value = serde_json::from_str(
            &GeminiExtension::new("pdf-tools".into(), "1.2.0".into()).to_json(),
        )
        .expect("json");
        assert_eq!(json["name"], "pdf-tools");
        assert_eq!(json["version"], "1.2.0");
        assert!(json.get("$schema").is_none(), "gemini has its own manifest");
    }

    #[test]
    fn marketplace_indexes_each_plugin_by_relative_path() {
        let market = Marketplace::new(
            "symposium".into(),
            vec![MarketplaceEntry {
                name: "pdf-tools".into(),
                source: "./pdf-tools".into(),
                description: Some("Table extraction guidance".into()),
            }],
        );
        let json: serde_json::Value = serde_json::from_str(&market.to_json()).expect("json");
        assert_eq!(json["name"], "symposium");
        assert_eq!(json["owner"]["name"], "symposium");
        assert_eq!(json["plugins"][0]["name"], "pdf-tools");
        assert_eq!(json["plugins"][0]["source"], "./pdf-tools");
        assert_eq!(
            json["plugins"][0]["description"],
            "Table extraction guidance"
        );
    }

    #[test]
    fn manifest_omits_absent_optional_fields() {
        let bare = Manifest::new("pdf-tools".into(), "0.0.0".into(), None).to_json();
        assert!(bare.contains(SCHEMA_URL));
        assert!(!bare.contains("description"));

        let full = Manifest::new("pdf-tools".into(), "1.2.0".into(), Some("d".into()));
        let json: serde_json::Value = serde_json::from_str(&full.to_json()).expect("json");
        assert_eq!(json["$schema"], SCHEMA_URL);
        assert_eq!(json["name"], "pdf-tools");
        assert_eq!(json["version"], "1.2.0");
        assert_eq!(json["description"], "d");
    }
}
