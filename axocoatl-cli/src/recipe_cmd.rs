//! `axocoatl recipe build|list`: build Session images from Axocoatl's
//! pinned recipes (claude-code, codex, e2e) with Podman. Owner: workstream
//! `agents` (the e2e recipe file belongs to `e2e`).

use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum RecipeCommands {
    /// Build one image from one or more recipes
    Build {
        /// Recipe names, such as `e2e` or `claude-code e2e`
        #[arg(required = true)]
        recipes: Vec<String>,
    },
    /// List recipes and the images built from them
    List,
}

/// Returns the process exit code.
pub async fn cmd_recipe(_command: RecipeCommands) -> i32 {
    eprintln!("✗ not implemented: axocoatl recipe");
    5
}
