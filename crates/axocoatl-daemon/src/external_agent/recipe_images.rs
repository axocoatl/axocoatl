//! The record of recipe images built on this computer, and the trust it
//! grants. `axocoatl recipe build` writes one entry per image to
//! `{data dir}/recipes/images.json` (owner-only, atomic replace): the image
//! name, the exact image id Podman reported, the recipes and the SHA-256 of
//! the composed Containerfile. A Session may start from a recorded image id
//! without `sandbox.allow_untrusted_images`, the way the browser image built
//! by `axocoatl browser install` is used; a name, a tag or an image id that
//! is not recorded is trusted only by that setting, as before.
//!
//! Owner: workstream `agents`.

use std::path::Path;

use axocoatl_core::SecureDir;
use axocoatl_isolation::recipes;
use serde::{Deserialize, Serialize};

/// Directory under the data root.
pub const RECIPES_DIR: &str = "recipes";
/// The record file inside [`RECIPES_DIR`].
pub const IMAGES_FILE: &str = "images.json";
/// Schema of the record file.
pub const IMAGES_SCHEMA: &str = "axocoatl.recipe-images/1";
/// The most images the record keeps (the oldest are dropped).
pub const MAX_RECORDED_IMAGES: usize = 64;
const MAX_FILE_BYTES: usize = 256 * 1024;

/// One built image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeImageRecord {
    /// `localhost/axocoatl-recipe-<names>:<digest prefix>`.
    pub image: String,
    /// The image id Podman reported for the build: 64 lowercase hex digits.
    pub image_id: String,
    /// The recipes, sorted.
    pub recipes: Vec<String>,
    /// SHA-256 (hex) of the composed Containerfile.
    pub containerfile_sha256: String,
    pub built_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImagesFile {
    schema: String,
    images: Vec<RecipeImageRecord>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecipeImageError {
    #[error("recipe images: {0}")]
    Invalid(String),
    #[error("recipe images: {0}")]
    Io(#[from] std::io::Error),
}

/// A full image id (`[0-9a-f]{64}`), with or without a `sha256:` prefix.
pub fn normalize_image_id(id: &str) -> Option<String> {
    let id = id.trim();
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    (id.len() == 64
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
    .then(|| id.to_string())
}

impl RecipeImageRecord {
    fn validate(&self) -> Result<(), RecipeImageError> {
        let names: Vec<String> = self.recipes.clone();
        let expected = recipes::image_name(&names)
            .map_err(|error| RecipeImageError::Invalid(error.to_string()))?;
        if expected != self.image
            || normalize_image_id(&self.image_id).as_deref() != Some(self.image_id.as_str())
            || recipes::canonical_names(&names)
                .map_err(|error| RecipeImageError::Invalid(error.to_string()))?
                != names.iter().map(String::as_str).collect::<Vec<_>>()
            || recipes::digest(&names).ok().as_deref() != Some(self.containerfile_sha256.as_str())
        {
            return Err(RecipeImageError::Invalid(format!(
                "the record of {} does not match this Axocoatl's recipes; rebuild it",
                self.image
            )));
        }
        Ok(())
    }
}

/// The record a build of `recipes` with Podman image id `image_id` writes.
pub fn record_for(
    names: &[String],
    image_id: &str,
    built_at_ms: u64,
) -> Result<RecipeImageRecord, RecipeImageError> {
    let image_id = normalize_image_id(image_id)
        .ok_or_else(|| RecipeImageError::Invalid(format!("{image_id:?} is not a full image id")))?;
    let record = RecipeImageRecord {
        image: recipes::image_name(names)
            .map_err(|error| RecipeImageError::Invalid(error.to_string()))?,
        image_id,
        recipes: recipes::canonical_names(names)
            .map_err(|error| RecipeImageError::Invalid(error.to_string()))?
            .into_iter()
            .map(str::to_string)
            .collect(),
        containerfile_sha256: recipes::digest(names)
            .map_err(|error| RecipeImageError::Invalid(error.to_string()))?,
        built_at_ms,
    };
    record.validate()?;
    Ok(record)
}

fn read_file(root: &SecureDir) -> Result<ImagesFile, RecipeImageError> {
    let directory = match root.existing_child(RECIPES_DIR) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ImagesFile::default())
        }
        Err(error) => return Err(error.into()),
    };
    let bytes = match directory.read_limited(IMAGES_FILE, MAX_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ImagesFile::default())
        }
        Err(error) => return Err(error.into()),
    };
    let file: ImagesFile = serde_json::from_slice(&bytes)
        .map_err(|error| RecipeImageError::Invalid(format!("{IMAGES_FILE}: {error}")))?;
    if file.schema != IMAGES_SCHEMA {
        return Err(RecipeImageError::Invalid(format!(
            "{IMAGES_FILE} has schema {:?}",
            file.schema
        )));
    }
    Ok(file)
}

/// Every recorded image whose record matches this Axocoatl's recipes,
/// newest first. Records of changed recipes are left out: their images are
/// no longer trusted.
pub fn recorded_images(root: &SecureDir) -> Result<Vec<RecipeImageRecord>, RecipeImageError> {
    let mut images: Vec<RecipeImageRecord> = read_file(root)?
        .images
        .into_iter()
        .filter(|record| record.validate().is_ok())
        .collect();
    images.sort_by_key(|record| std::cmp::Reverse(record.built_at_ms));
    Ok(images)
}

/// Record a built image, replacing the record of the same image name.
pub fn record_image(root: &SecureDir, record: RecipeImageRecord) -> Result<(), RecipeImageError> {
    record.validate()?;
    let mut file = read_file(root).unwrap_or_default();
    file.schema = IMAGES_SCHEMA.into();
    file.images
        .retain(|existing| existing.image != record.image);
    file.images.push(record);
    file.images
        .sort_by_key(|record| std::cmp::Reverse(record.built_at_ms));
    file.images.truncate(MAX_RECORDED_IMAGES);
    let directory = root.child(RECIPES_DIR)?;
    directory.restrict_owner_only()?;
    let bytes = serde_json::to_vec_pretty(&file)
        .map_err(|error| RecipeImageError::Invalid(error.to_string()))?;
    directory.atomic_write_with_mode(IMAGES_FILE, &bytes, 0o600)?;
    Ok(())
}

/// [`record_image`] for a data directory path (the CLI).
pub fn record_image_at(data_dir: &Path, record: RecipeImageRecord) -> Result<(), RecipeImageError> {
    record_image(&SecureDir::open_existing_all(data_dir)?, record)
}

/// [`recorded_images`] for a data directory path (the CLI).
pub fn recorded_images_at(data_dir: &Path) -> Result<Vec<RecipeImageRecord>, RecipeImageError> {
    recorded_images(&SecureDir::open_existing_all(data_dir)?)
}

/// The recorded image built from exactly `names` by this Axocoatl's
/// recipes. A loadout's `environment.recipes` runs its Session on this
/// record's `image_id`.
pub fn image_for_recipes(
    root: &SecureDir,
    names: &[String],
) -> Result<RecipeImageRecord, RecipeImageError> {
    let image =
        recipes::image_name(names).map_err(|error| RecipeImageError::Invalid(error.to_string()))?;
    recorded_images(root)?
        .into_iter()
        .find(|record| record.image == image)
        .ok_or_else(|| {
            let sorted = recipes::canonical_names(names)
                .map(|names| names.join(" "))
                .unwrap_or_default();
            RecipeImageError::Invalid(format!(
                "no image is built from the recipes {sorted}; run `axocoatl recipe build {sorted}`"
            ))
        })
}

/// Whether a Session may start from `image` without
/// `sandbox.allow_untrusted_images`: it is exactly the image id of a
/// recorded recipe build (64 hex, optionally `sha256:`-prefixed). Names and
/// tags are never trusted this way, so a retagged image cannot borrow trust.
pub fn is_trusted_image(root: &SecureDir, image: &str) -> bool {
    let Some(id) = normalize_image_id(image) else {
        return false;
    };
    recorded_images(root)
        .map(|images| images.iter().any(|record| record.image_id == id))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn a_recorded_build_trusts_exactly_its_image_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = SecureDir::open(dir.path()).unwrap();
        assert!(recorded_images(&root).unwrap().is_empty());
        assert!(!is_trusted_image(&root, ID));
        let record = record_for(
            &names(&["codex", "claude-code"]),
            &format!("sha256:{ID}"),
            5,
        )
        .unwrap();
        assert_eq!(record.recipes, ["claude-code", "codex"]);
        assert_eq!(record.image_id, ID);
        record_image(&root, record.clone()).unwrap();
        let file = dir.path().join(RECIPES_DIR).join(IMAGES_FILE);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(is_trusted_image(&root, ID));
        assert!(is_trusted_image(&root, &format!("sha256:{ID}")));
        // The name and any other id are not trusted by the record.
        assert!(!is_trusted_image(&root, &record.image));
        assert!(!is_trusted_image(&root, &ID.replace('0', "1")));
        assert!(!is_trusted_image(&root, &ID[..12]));
        assert_eq!(
            image_for_recipes(&root, &names(&["claude-code", "codex"])).unwrap(),
            record
        );
        let missing = image_for_recipes(&root, &names(&["codex"])).unwrap_err();
        assert!(missing.to_string().contains("axocoatl recipe build codex"));
        // A rebuild replaces the record of the same image name.
        let rebuilt =
            record_for(&names(&["claude-code", "codex"]), &ID.replace('a', "b"), 9).unwrap();
        record_image(&root, rebuilt.clone()).unwrap();
        assert_eq!(recorded_images(&root).unwrap(), [rebuilt]);
        assert!(!is_trusted_image(&root, ID));
    }

    #[test]
    fn a_record_that_no_longer_matches_the_recipes_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let root = SecureDir::open(dir.path()).unwrap();
        let mut stale = record_for(&names(&["claude-code"]), ID, 1).unwrap();
        stale.containerfile_sha256 = "0".repeat(64);
        assert!(record_image(&root, stale.clone()).is_err());
        // Written by an older Axocoatl with other fragments.
        std::fs::create_dir(dir.path().join(RECIPES_DIR)).unwrap();
        std::fs::write(
            dir.path().join(RECIPES_DIR).join(IMAGES_FILE),
            serde_json::to_vec(&ImagesFile {
                schema: IMAGES_SCHEMA.into(),
                images: vec![stale],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(recorded_images(&root).unwrap().is_empty());
        assert!(!is_trusted_image(&root, ID));
        assert!(record_for(&names(&["claude-code"]), "not-an-id", 1).is_err());
    }
}
