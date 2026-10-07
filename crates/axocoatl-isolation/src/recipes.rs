//! Session image recipes: pinned Containerfile fragments composed over one
//! base and built locally with Podman (`axocoatl recipe build`). An image
//! built from recipes is trusted for Sessions by its recorded image id, the
//! way the browser image is, without `allow_untrusted_images`.
//!
//! Owner: workstream `agents` (registry, composition, build, trust). The
//! `e2e` fragment belongs to workstream `e2e`.

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

/// The full Containerfile for `names` (deduplicated, in registry order).
pub fn compose(_names: &[String]) -> Result<String, RecipeError> {
    Err(RecipeError::NotImplemented("recipes::compose"))
}

/// The image name for `names`.
pub fn image_name(_names: &[String]) -> Result<String, RecipeError> {
    Err(RecipeError::NotImplemented("recipes::image_name"))
}
