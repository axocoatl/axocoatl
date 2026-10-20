//! Session image recipes: pinned Containerfile fragments composed over one
//! base and built locally with Podman (`axocoatl recipe build`). An image
//! built from recipes is trusted for Sessions by its recorded image id, the
//! way the browser image is, without `allow_untrusted_images`.
//!
//! Owner: workstream `agents` (registry, composition, build, trust). The
//! `e2e` fragment belongs to workstream `e2e`; the language toolchains
//! (`node`, `python`, `rust`, `go`) to workstream `toolchain`.
//!
//! A recipe image is named for what it holds and what it was built from:
//! `localhost/axocoatl-recipe-<names, sorted, joined by '-'>:<first 12 hex of
//! the SHA-256 of the composed Containerfile>`. A changed fragment or base
//! therefore names a new image; an old image of the same recipes is never
//! taken for the new one.
//!
//! A language toolchain recipe gives the image the repository's language: an
//! official Debian-based image of it, pinned by its multi-architecture image
//! index digest ([`TOOLCHAIN_IMAGES`]). An image holds at most one. Without
//! one, every image starts from [`RECIPE_BASE`] (Node 24 on Debian 12 slim),
//! exactly as before toolchains existed, so the images of `claude-code`,
//! `codex` and `e2e` keep their names. With `node`, the image starts from
//! that base too; with `python`, `rust` or `go` it starts from that
//! language's image, and when the image also holds an agent or e2e recipe
//! (whose installs need Node) Node is copied in from the recipe base's own
//! pinned image ([`NODE_GRAFT`]). The toolchain's fragment always comes
//! before the other recipes' fragments.

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

/// Every agent and check recipe, in composition order.
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

/// The language toolchain recipes. An image holds at most one.
pub const TOOLCHAIN_RECIPES: [Recipe; 4] = [
    Recipe {
        name: "node",
        description: "Node.js 24 with npm, and pnpm and yarn through corepack",
        fragment: include_str!("../assets/recipes/toolchains/node.Containerfile"),
    },
    Recipe {
        name: "python",
        description: "Python 3.13 with pip and venv",
        fragment: include_str!("../assets/recipes/toolchains/python.Containerfile"),
    },
    Recipe {
        name: "rust",
        description: "Rust 1.99 with cargo, rustc and gcc",
        fragment: include_str!("../assets/recipes/toolchains/rust.Containerfile"),
    },
    Recipe {
        name: "go",
        description: "Go 1.27 with git and gcc",
        fragment: include_str!("../assets/recipes/toolchains/go.Containerfile"),
    },
];

/// Where every toolchain image keeps its package caches, and where a
/// loadout run mounts the Workspace's toolchain cache volume.
pub const TOOLCHAIN_CACHE_DIR: &str = "/opt/axocoatl-cache";

/// The official image a language toolchain starts from, and what a build of
/// it costs: the compressed layers of the pinned image plus the Debian
/// packages a build adds, and the built image on disk (measured on
/// linux/arm64 with Podman 5.8 on 2026-10-19; the amd64 layers of rust are
/// about 40 MB larger, the others within a few MB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolchainImage {
    /// The toolchain recipe's name.
    pub toolchain: &'static str,
    /// The official image and tag the base starts from.
    pub image: &'static str,
    /// The multi-architecture image index digest the base pins.
    pub index_digest: &'static str,
    /// The base Containerfile; `None` for the recipe base itself.
    pub base: Option<&'static str>,
    /// About how many megabytes a first build downloads.
    pub download_mb: u32,
    /// About how many megabytes the built image takes on disk.
    pub image_mb: u32,
}

/// One row per language toolchain: the pinned official image (tag and image
/// index digest) and its approximate size.
pub const TOOLCHAIN_IMAGES: [ToolchainImage; 4] = [
    ToolchainImage {
        toolchain: "node",
        image: "docker.io/library/node:24.21.0-bookworm-slim",
        index_digest: "sha256:d6aa754f16b3197301076f047b5def2f02ea1dbbc2ca920407d46d7ec7f87b20",
        base: None,
        download_mb: 100,
        image_mb: 365,
    },
    ToolchainImage {
        toolchain: "python",
        image: "docker.io/library/python:3.13.16-slim-bookworm",
        index_digest: "sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641",
        base: Some(include_str!(
            "../assets/recipes/toolchains/python-base.Containerfile"
        )),
        download_mb: 65,
        image_mb: 250,
    },
    ToolchainImage {
        toolchain: "rust",
        image: "docker.io/library/rust:1.99.1-slim-bookworm",
        index_digest: "sha256:a24e292b4af7bdee351a31d3a6379972a5652f0ea22a1b6d6883324ac4f5097c",
        base: Some(include_str!(
            "../assets/recipes/toolchains/rust-base.Containerfile"
        )),
        download_mb: 330,
        image_mb: 920,
    },
    ToolchainImage {
        toolchain: "go",
        image: "docker.io/library/golang:1.27.2-bookworm",
        index_digest: "sha256:5cf287a799e6b94384bad13d16b14904c531f51ba65792237e122ce42b392f61",
        base: Some(include_str!(
            "../assets/recipes/toolchains/go-base.Containerfile"
        )),
        download_mb: 300,
        image_mb: 880,
    },
];

/// Node, npm and corepack for a `python`, `rust` or `go` image that also
/// holds an agent or e2e recipe, copied from the recipe base's pinned image.
pub const NODE_GRAFT: &str = include_str!("../assets/recipes/toolchains/node-graft.Containerfile");

/// About how many megabytes an agent or check recipe, or the Node graft
/// (`"node-graft"`), adds to an image on disk (its layers, measured on
/// linux/arm64 with Podman 5.8 on 2026-10-19). `None` when not measured.
pub fn recipe_disk_mb(name: &str) -> Option<u32> {
    match name {
        "claude-code" => Some(255),
        "codex" => Some(405),
        "node-graft" => Some(140),
        _ => None,
    }
}

/// What a first build of the image of `names` costs, about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSize {
    /// The toolchain's (or the recipe base's) official image to download.
    pub base_image: &'static str,
    /// About how many megabytes that image's download is.
    pub base_download_mb: u32,
    /// About how many megabytes the built image takes on disk, the
    /// measured recipes summed.
    pub disk_mb: u32,
    /// Recipes whose own downloads (an agent's package, a browser) come on
    /// top, and those whose size was not measured.
    pub also_downloads: Vec<&'static str>,
    pub unmeasured: Vec<&'static str>,
}

/// The approximate size of a first build of the image of `names`.
pub fn image_size(names: &[String]) -> Result<ImageSize, RecipeError> {
    let recipes = resolve(names)?;
    let toolchain = recipes
        .iter()
        .find_map(|recipe| toolchain_image(recipe.name))
        .or_else(|| toolchain_image("node"))
        .expect("the node toolchain row exists");
    let others: Vec<&Recipe> = recipes
        .iter()
        .copied()
        .filter(|recipe| !is_toolchain(recipe.name))
        .collect();
    let mut size = ImageSize {
        base_image: toolchain.image,
        base_download_mb: toolchain.download_mb,
        disk_mb: toolchain.image_mb,
        also_downloads: Vec::new(),
        unmeasured: Vec::new(),
    };
    if toolchain.base.is_some() && !others.is_empty() {
        size.disk_mb += recipe_disk_mb("node-graft").unwrap_or_default();
        size.also_downloads.push("Node.js");
    }
    for recipe in others {
        size.also_downloads.push(recipe.name);
        match recipe_disk_mb(recipe.name) {
            Some(mb) => size.disk_mb += mb,
            None => size.unmeasured.push(recipe.name),
        }
    }
    Ok(size)
}

/// The toolchain recipe named `name`.
pub fn toolchain_image(name: &str) -> Option<&'static ToolchainImage> {
    TOOLCHAIN_IMAGES
        .iter()
        .find(|image| image.toolchain == name)
}

/// Whether `name` is a language toolchain recipe.
pub fn is_toolchain(name: &str) -> bool {
    toolchain_image(name).is_some()
}

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
    #[error("recipe: {0}")]
    Invalid(String),
}

/// The recipe named `name`.
pub fn recipe(name: &str) -> Option<&'static Recipe> {
    RECIPES
        .iter()
        .chain(TOOLCHAIN_RECIPES.iter())
        .find(|recipe| recipe.name == name)
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
    let toolchains: Vec<&str> = TOOLCHAIN_RECIPES
        .iter()
        .map(|recipe| recipe.name)
        .filter(|toolchain| names.iter().any(|name| name == toolchain))
        .collect();
    if toolchains.len() > 1 {
        return Err(RecipeError::Invalid(format!(
            "an image holds one language toolchain, and {} were named",
            toolchains.join(" and ")
        )));
    }
    Ok(RECIPES
        .iter()
        .chain(TOOLCHAIN_RECIPES.iter())
        .filter(|recipe| names.iter().any(|name| name == recipe.name))
        .collect())
}

fn known_names() -> String {
    RECIPES
        .iter()
        .chain(TOOLCHAIN_RECIPES.iter())
        .map(|recipe| recipe.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The language toolchain among `names`, if any (they must resolve).
pub fn toolchain_of(names: &[String]) -> Result<Option<&'static str>, RecipeError> {
    Ok(resolve(names)?
        .into_iter()
        .map(|recipe| recipe.name)
        .find(|name| is_toolchain(name)))
}

/// The full Containerfile for `names` (deduplicated): the base, then the
/// language toolchain's fragment when there is one, then each other
/// fragment in registry order. Without a toolchain the base is
/// [`RECIPE_BASE`] and the result is exactly what it was before toolchains
/// existed. With `python`, `rust` or `go` the base is that language's
/// pinned image, plus [`NODE_GRAFT`] when another recipe needs Node.
pub fn compose(names: &[String]) -> Result<String, RecipeError> {
    let recipes = resolve(names)?;
    let toolchain = recipes
        .iter()
        .copied()
        .find(|recipe| is_toolchain(recipe.name));
    let others: Vec<&Recipe> = recipes
        .iter()
        .copied()
        .filter(|recipe| !is_toolchain(recipe.name))
        .collect();
    let base = toolchain
        .and_then(|recipe| toolchain_image(recipe.name))
        .and_then(|image| image.base);
    let mut containerfile = String::from(base.unwrap_or(RECIPE_BASE));
    if base.is_some() && !others.is_empty() {
        push_section(&mut containerfile, "node for the other recipes", NODE_GRAFT);
    }
    for recipe in toolchain.into_iter().chain(others) {
        push_section(
            &mut containerfile,
            &format!("recipe: {}", recipe.name),
            recipe.fragment,
        );
    }
    if !containerfile.ends_with('\n') {
        containerfile.push('\n');
    }
    Ok(containerfile)
}

fn push_section(containerfile: &mut String, header: &str, fragment: &str) {
    if !containerfile.ends_with('\n') {
        containerfile.push('\n');
    }
    containerfile.push_str(&format!("\n# ---- {header} ----\n"));
    containerfile.push_str(fragment);
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
    fn images_without_a_toolchain_keep_their_names() {
        // The composition of the agent and check recipes is exactly what it
        // was before toolchains existed, so images built by 1.4 keep their
        // records and are not rebuilt.
        assert_eq!(
            image_name(&names(&["claude-code"])).unwrap(),
            "localhost/axocoatl-recipe-claude-code:37856fb2badf"
        );
        assert_eq!(
            image_name(&names(&["codex"])).unwrap(),
            "localhost/axocoatl-recipe-codex:f44abab2bdbd"
        );
        assert_eq!(toolchain_of(&names(&["claude-code", "e2e"])).unwrap(), None);
    }

    #[test]
    fn a_toolchain_image_starts_from_its_pinned_language_image() {
        for image in TOOLCHAIN_IMAGES {
            assert!(recipe(image.toolchain).is_some(), "{}", image.toolchain);
            assert!(is_toolchain(image.toolchain));
            assert!(image.index_digest.starts_with("sha256:"));
            assert_eq!(image.index_digest.len(), 71, "{}", image.toolchain);
            let composed = compose(&names(&[image.toolchain])).unwrap();
            let from = composed
                .lines()
                .filter(|line| line.starts_with("FROM "))
                .collect::<Vec<_>>();
            assert_eq!(from.len(), 1, "{composed}");
            match image.base {
                // node starts from the recipe base, which pins the same tag.
                None => {
                    assert!(composed.starts_with(RECIPE_BASE));
                    assert!(RECIPE_BASE.contains(image.index_digest));
                }
                Some(base) => {
                    assert!(composed.starts_with(base));
                    let (repository, _tag) = image.image.rsplit_once(':').unwrap();
                    assert!(
                        from[0].starts_with(&format!("FROM {repository}:")),
                        "{}",
                        from[0]
                    );
                    assert!(from[0].ends_with(&format!("@{}", image.index_digest)));
                    assert!(from[0].contains(image.image.rsplit_once(':').unwrap().1));
                    // No other recipe: no Node copied in.
                    assert!(!composed.contains("COPY --from="), "{}", image.toolchain);
                }
            }
            assert!(composed.contains(&format!("# ---- recipe: {} ----", image.toolchain)));
            assert!(composed.contains(&format!("AXOCOATL_CACHE={TOOLCHAIN_CACHE_DIR}")));
            assert!(composed.contains(&format!("io.axocoatl.toolchain={}", image.toolchain)));
            assert!(!composed.contains("@latest"));
            let name = image_name(&names(&[image.toolchain])).unwrap();
            assert!(name.starts_with(&format!("{RECIPE_IMAGE_PREFIX}{}:", image.toolchain)));
            assert!(is_recipe_image_name(&name));
        }
    }

    #[test]
    fn an_agent_on_a_language_image_gets_node_from_the_recipe_base_image() {
        let composed = compose(&names(&["python", "claude-code"])).unwrap();
        let python = toolchain_image("python").unwrap();
        assert!(composed.starts_with(python.base.unwrap()));
        let graft = composed
            .find("# ---- node for the other recipes ----")
            .unwrap();
        let toolchain = composed.find("# ---- recipe: python ----").unwrap();
        let agent = composed.find("# ---- recipe: claude-code ----").unwrap();
        assert!(graft < toolchain && toolchain < agent, "{composed}");
        // Node comes from the exact image index the recipe base pins.
        let node = toolchain_image("node").unwrap();
        assert!(NODE_GRAFT.contains(node.index_digest));
        assert!(RECIPE_BASE.contains(node.index_digest));
        assert!(composed.contains("axocoatl-verify-integrity"));
        assert_eq!(
            image_name(&names(&["claude-code", "python"])).unwrap(),
            image_name(&names(&["python", "claude-code"])).unwrap()
        );
        assert!(image_name(&names(&["claude-code", "python"]))
            .unwrap()
            .starts_with("localhost/axocoatl-recipe-claude-code-python:"));
        // The node toolchain needs no graft: the base is Node already.
        let node = compose(&names(&["codex", "node"])).unwrap();
        assert!(node.starts_with(RECIPE_BASE));
        assert!(!node.contains("COPY --from="));
        assert!(
            node.find("# ---- recipe: node ----").unwrap()
                < node.find("# ---- recipe: codex ----").unwrap()
        );
        assert_eq!(
            toolchain_of(&names(&["codex", "node"])).unwrap(),
            Some("node")
        );
    }

    /// TC-NU-6: a composed image's size adds its recipes' own.
    #[test]
    fn composed_images_add_their_recipes_sizes() {
        let python = toolchain_image("python").unwrap();
        let alone = image_size(&names(&["python"])).unwrap();
        assert_eq!(alone.disk_mb, python.image_mb);
        assert_eq!(alone.base_download_mb, python.download_mb);
        assert!(alone.also_downloads.is_empty() && alone.unmeasured.is_empty());
        let composed = image_size(&names(&["claude-code", "python"])).unwrap();
        assert_eq!(composed.base_image, python.image);
        assert_eq!(composed.disk_mb, python.image_mb + 140 + 255);
        assert_eq!(composed.also_downloads, ["Node.js", "claude-code"]);
        // On the node toolchain (the recipe base) there is no graft.
        let node = image_size(&names(&["codex", "node"])).unwrap();
        assert_eq!(
            node.disk_mb,
            toolchain_image("node").unwrap().image_mb + 405
        );
        assert_eq!(node.also_downloads, ["codex"]);
        let e2e = image_size(&names(&["e2e", "go"])).unwrap();
        assert_eq!(e2e.unmeasured, ["e2e"]);
    }

    #[test]
    fn an_image_holds_one_toolchain() {
        let error = compose(&names(&["python", "rust"])).unwrap_err();
        assert!(
            error.to_string().contains("one language toolchain"),
            "{error}"
        );
        assert!(image_name(&names(&["go", "node", "codex"])).is_err());
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
