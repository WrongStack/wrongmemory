//! Only presentation preferences. Memory text, queries and credentials have
//! no field here. Unknown future versions are never overwritten.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read};
use wrongmemory_ipc::{DataRoot, endpoint};

const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    pub version: u32,
    pub appearance: Appearance,
    pub project: Option<String>,
    pub view: String,
    pub status: Option<String>,
    pub graph_zoom: f32,
    pub sidebar_width: f32,
    pub library_width: f32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            version: 1,
            appearance: Appearance::System,
            project: None,
            view: "Library".into(),
            status: Some("active".into()),
            graph_zoom: 1.,
            sidebar_width: 228.,
            library_width: 510.,
        }
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Preferences {
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1 {
            return Err(invalid(
                "Unsupported desktop preferences version; the file was left intact",
            ));
        }
        if !matches!(
            self.view.as_str(),
            "Library" | "Review" | "Relations" | "Clients" | "Health"
        ) {
            return Err(invalid("Invalid saved view"));
        }
        if self.status.as_deref().is_some_and(|s| {
            !matches!(
                s,
                "active" | "stale" | "archived" | "superseded" | "contradicted" | "deleted"
            )
        }) {
            return Err(invalid("Invalid saved status"));
        }
        if self.project.as_ref().is_some_and(|s| {
            s.is_empty()
                || s.len() > 128
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }) {
            return Err(invalid("Invalid saved project identifier"));
        }
        for (value, min, max) in [
            (self.graph_zoom, 0.5, 1.6),
            (self.sidebar_width, 195., 380.),
            (self.library_width, 340., 1100.),
        ] {
            if !value.is_finite() || !(min..=max).contains(&value) {
                return Err(invalid("Invalid saved presentation dimensions"));
            }
        }
        Ok(())
    }

    fn read(root: &DataRoot) -> io::Result<Option<Value>> {
        let file = match std::fs::File::open(root.path().join("desktop-ui.json")) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("Desktop preferences exceed 64 KiB"));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
        let decoded: Self =
            serde_json::from_value(value.clone()).map_err(|e| invalid(e.to_string()))?;
        decoded.validate()?;
        Ok(Some(value))
    }

    pub fn load(root: &DataRoot) -> io::Result<Self> {
        match Self::read(root)? {
            Some(value) => serde_json::from_value(value).map_err(|e| invalid(e.to_string())),
            None => Ok(Self::default()),
        }
    }

    pub fn save(&self, root: &DataRoot) -> io::Result<()> {
        self.validate()?;
        // Preserve unrecognized optional fields from the original JSON object.
        let mut existing = Self::read(root)?.unwrap_or_else(|| serde_json::json!({}));
        let object = existing
            .as_object_mut()
            .ok_or_else(|| invalid("Preferences must be an object"))?;
        let patch = serde_json::to_value(self).map_err(|e| invalid(e.to_string()))?;
        for (key, value) in patch
            .as_object()
            .ok_or_else(|| invalid("Invalid preferences"))?
        {
            object.insert(key.clone(), value.clone());
        }
        let bytes = serde_json::to_vec_pretty(&existing).map_err(|e| invalid(e.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid("Desktop preferences exceed 64 KiB"));
        }
        root.ensure_private_dirs()?;
        endpoint::write_private_atomic(&root.path().join("desktop-ui.json"), &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_round_trip_without_erasing_unknown_optional_fields() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(temp.path())).unwrap();
        std::fs::write(
            root.path().join("desktop-ui.json"),
            r#"{"version":1,"futureOptional":null}"#,
        )
        .unwrap();
        let mut prefs = Preferences::load(&root).unwrap();
        prefs.appearance = Appearance::Dark;
        prefs.project = Some("p_fixture".into());
        prefs.sidebar_width = 260.;
        prefs.save(&root).unwrap();
        let loaded = Preferences::load(&root).unwrap();
        assert_eq!(loaded.appearance, Appearance::Dark);
        assert_eq!(loaded.sidebar_width, 260.);
        let raw: Value =
            serde_json::from_slice(&std::fs::read(root.path().join("desktop-ui.json")).unwrap())
                .unwrap();
        assert!(raw.get("futureOptional").is_some_and(Value::is_null));
        assert!(raw.get("text").is_none());
        assert!(raw.get("secret").is_none());
        assert!(raw.get("query").is_none());
    }
    #[test]
    fn newer_or_corrupt_preferences_are_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(temp.path())).unwrap();
        let path = root.path().join("desktop-ui.json");
        for original in [r#"{"version":2,"importantFutureState":true}"#, "{broken"] {
            std::fs::write(&path, original).unwrap();
            assert!(Preferences::load(&root).is_err());
            assert!(Preferences::default().save(&root).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
    #[test]
    fn oversized_and_invalid_geometry_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(temp.path())).unwrap();
        let mut prefs = Preferences {
            sidebar_width: f32::NAN,
            ..Default::default()
        };
        assert!(prefs.save(&root).is_err());
        prefs.sidebar_width = 196.;
        prefs.graph_zoom = 0.1;
        assert!(prefs.save(&root).is_err());
        std::fs::write(
            root.path().join("desktop-ui.json"),
            vec![b' '; MAX_BYTES as usize + 1],
        )
        .unwrap();
        assert!(Preferences::load(&root).is_err());
    }
}
