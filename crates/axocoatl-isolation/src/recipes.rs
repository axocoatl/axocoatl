//! Session image recipes: pinned Containerfile fragments composed over one
//! base and built locally with Podman (`axocoatl recipe build`). An image
//! built from recipes is trusted for Sessions by its recorded image id, the
//! way the browser image is, without `allow_untrusted_images`.
//!
//! Owner: workstream `agents` (registry, composition, build, trust). The
//! `e2e` fragment belongs to workstream `e2e`.
//!
//! A recipe image is named for what it holds and what it was built from:
//! `localhost/axocoatl-recipe-<names, sorted, joined by '-'>:<first 12 hex of
//! the SHA-256 of the composed Containerfile>`. A changed fragment or base
//! therefore names a new image; an old image of the same recipes is never
//! taken for the new one.

use sha2::{Digest, Sha256};

/// One recipe fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    pub name: &'static str,
    pub description: &'static str,
    /// Containerfile instructions appended to the base.
    pub fragment: &'static str,
}

/// The base every recipe image starts from.
pub const RECIPE_BASE: &str = include_str!("../assets/recipes/base.Containerfile");

/// Every recipe, in composition order.
pub const RECIPES: [Recipe; 3] = [
    Recipe {
        name: "claude-code",
        description: "Claude Code CLI, run headless as an external Agent",
        fragment: include_str!("../assets/recipes/claude-code.Containerfile"),
    },
    Recipe {
        name: "codex",
        description: "Codex CLI, run headless as an external Agent",
        fragment: include_str!("../assets/recipes/codex.Containerfile"),
    },
    Recipe {
        name: "e2e",
        description: "tester-army/e2e 0.18.0 with Chromium, for the e2e required check",
        fragment: include_str!("../assets/recipes/e2e.Containerfile"),
    },
];

/// Image name prefix of recipe images: `localhost/axocoatl-recipe-<names>`.
pub const RECIPE_IMAGE_PREFIX: &str = "localhost/axocoatl-recipe-";

/// Hex digits of the Containerfile digest in an image tag.
pub const RECIPE_TAG_HEX: usize = 12;

/// Label every recipe image carries: the recipes it holds, sorted and joined
/// by `,`.
pub const RECIPES_LABEL: &str = "io.axocoatl.recipes";

/// Label with the full SHA-256 of the composed Containerfile.
pub const RECIPE_DIGEST_LABEL: &str = "io.axocoatl.recipe-digest";

#[derive(Debug, thiserror::Error)]
pub enum RecipeError {
    #[error("recipe: not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("recipe: {0}")]
    Invalid(String),
}

/// The recipe named `name`.
pub fn recipe(name: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|recipe| recipe.name == name)
}

/// The recipes named by `names`, each once, in registry order. Refuses an
/// empty list and any name not in [`RECIPES`].
pub fn resolve(names: &[String]) -> Result<Vec<&'static Recipe>, RecipeError> {
    if names.is_empty() {
        return Err(RecipeError::Invalid(format!(
            "name at least one recipe: {}",
            known_names()
        )));
    }
    if let Some(unknown) = names.iter().find(|name| recipe(name).is_none()) {
        return Err(RecipeError::Invalid(format!(
            "there is no recipe named {unknown:?}; recipes: {}",
            known_names()
        )));
    }
    Ok(RECIPES
        .iter()
        .filter(|recipe| names.iter().any(|name| name == recipe.name))
        .collect())
}

fn known_names() -> String {
    RECIPES
        .iter()
        .map(|recipe| recipe.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The full Containerfile for `names` (deduplicated, in registry order): the
/// base followed by each fragment.
pub fn compose(names: &[String]) -> Result<String, RecipeError> {
    let recipes = resolve(names)?;
    let mut containerfile = String::from(RECIPE_BASE);
    for recipe in recipes {
        if !containerfile.ends_with('\n') {
            containerfile.push('\n');
        }
        containerfile.push_str(&format!("\n# ---- recipe: {} ----\n", recipe.name));
        containerfile.push_str(recipe.fragment);
    }
    if !containerfile.ends_with('\n') {
        containerfile.push('\n');
    }
    Ok(containerfile)
}

/// SHA-256 (hex) of the composed Containerfile for `names`.
pub fn digest(names: &[String]) -> Result<String, RecipeError> {
    Ok(format!("{:x}", Sha256::digest(compose(names)?.as_bytes())))
}

/// The recipes `names` names, sorted and each once: what an image of them is
/// named and labeled for.
pub fn canonical_names(names: &[String]) -> Result<Vec<&'static str>, RecipeError> {
    let mut sorted: Vec<&'static str> = resolve(names)?
        .into_iter()
        .map(|recipe| recipe.name)
        .collect();
    sorted.sort_unstable();
    Ok(sorted)
}

/// The image name for `names`:
/// `localhost/axocoatl-recipe-<sorted names>:<digest prefix>`.
pub fn image_name(names: &[String]) -> Result<String, RecipeError> {
    let sorted = canonical_names(names)?;
    let digest = digest(names)?;
    Ok(format!(
        "{RECIPE_IMAGE_PREFIX}{}:{}",
        sorted.join("-"),
        &digest[..RECIPE_TAG_HEX]
    ))
}

/// The labels `podman build` puts on an image of `names`.
pub fn labels(names: &[String]) -> Result<Vec<(String, String)>, RecipeError> {
    Ok(vec![
        ("io.axocoatl.role".into(), "recipe".into()),
        (RECIPES_LABEL.into(), canonical_names(names)?.join(",")),
        (RECIPE_DIGEST_LABEL.into(), digest(names)?),
    ])
}

/// Whether `image` is shaped like a recipe image name (not proof that it is
/// one: trust comes only from the recorded image id).
pub fn is_recipe_image_name(image: &str) -> bool {
    image.strip_prefix(RECIPE_IMAGE_PREFIX).is_some_and(|rest| {
        rest.split_once(':').is_some_and(|(names, tag)| {
            !names.is_empty()
                && names
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && tag.len() == RECIPE_TAG_HEX
                && tag.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn compose_puts_the_base_first_and_each_fragment_once_in_registry_order() {
        let composed = compose(&names(&["codex", "claude-code", "codex"])).unwrap();
        assert!(composed.starts_with(RECIPE_BASE));
        let claude = composed.find("# ---- recipe: claude-code ----").unwrap();
        let codex = composed.find("# ---- recipe: codex ----").unwrap();
        assert!(claude < codex);
        assert_eq!(composed.matches("# ---- recipe: codex ----").count(), 1);
        assert!(!composed.contains("# ---- recipe: e2e ----"));
        // One FROM: fragments extend the base, never replace it.
        assert_eq!(
            composed
                .lines()
                .filter(|line| line.trim_start().starts_with("FROM "))
                .count(),
            1
        );
        assert_eq!(
            compose(&names(&["claude-code", "codex"])).unwrap(),
            composed
        );
    }

    #[test]
    fn unknown_and_empty_lists_are_refused() {
        assert!(compose(&[]).is_err());
        let error = compose(&names(&["claude-code", "cursor"])).unwrap_err();
        assert!(error.to_string().contains("\"cursor\""), "{error}");
        assert!(error.to_string().contains("claude-code, codex, e2e"));
        assert!(image_name(&names(&["../x"])).is_err());
    }

    #[test]
    fn image_names_sort_the_recipes_and_tag_the_containerfile_digest() {
        let one = image_name(&names(&["codex", "claude-code"])).unwrap();
        let two = image_name(&names(&["claude-code", "codex", "claude-code"])).unwrap();
        assert_eq!(one, two);
        let digest = digest(&names(&["claude-code", "codex"])).unwrap();
        assert_eq!(
            one,
            format!(
                "localhost/axocoatl-recipe-claude-code-codex:{}",
                &digest[..12]
            )
        );
        assert!(is_recipe_image_name(&one));
        let claude = image_name(&names(&["claude-code"])).unwrap();
        assert!(claude.starts_with("localhost/axocoatl-recipe-claude-code:"));
        assert_ne!(claude, one);
        assert!(!is_recipe_image_name("localhost/axocoatl-recipe-x:latest"));
        assert!(!is_recipe_image_name("docker.io/library/alpine:3.20"));
        let labels = labels(&names(&["codex", "claude-code"])).unwrap();
        assert!(labels.contains(&(RECIPES_LABEL.into(), "claude-code,codex".into())));
        assert!(labels.contains(&(RECIPE_DIGEST_LABEL.into(), digest)));
    }

    #[test]
    fn the_base_and_agent_fragments_are_pinned() {
        // Node >= 24.8 from an image index digest.
        assert!(RECIPE_BASE.contains(
            "FROM docker.io/library/node:24-bookworm-slim@sha256:d6aa754f16b3197301076f047b5def2f02ea1dbbc2ca920407d46d7ec7f87b20"
        ));
        assert!(
            RECIPE_BASE.contains("apt-get install -y --no-install-recommends ca-certificates git")
        );
        let claude = recipe("claude-code").unwrap().fragment;
        assert!(claude.contains("@anthropic-ai/claude-code-${platform}@2.1.292"));
        assert_eq!(claude.matches("integrity='sha512-").count(), 2);
        assert!(claude.contains("axocoatl-verify-integrity"));
        let codex = recipe("codex").unwrap().fragment;
        assert!(codex.contains("@openai/codex@0.160.1-${platform}"));
        assert_eq!(codex.matches("integrity='sha512-").count(), 2);
        assert!(codex.contains("axocoatl-verify-integrity"));
        for fragment in [claude, codex] {
            assert!(
                !fragment.contains("FROM "),
                "a fragment never replaces the base"
            );
            assert!(!fragment.contains("@latest"));
        }
    }
}
