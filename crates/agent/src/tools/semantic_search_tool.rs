use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use codebase_index::{CodebaseIndex, SearchResult};
use gpui::{App, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::sync::Arc;

/// Default hits returned. Enough to cover a concept spread across a few files,
/// few enough that the results do not crowd out the rest of the context.
const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 25;

/// Search the codebase by meaning rather than by exact text.
///
/// Use this when you can describe what you are looking for but do not know the
/// identifier to search for — "where do we decide a session has expired", "the
/// code that retries failed uploads". It finds code whose *purpose* matches your
/// description, even when it shares no words with it.
///
/// - Prefer `grep` when you know the exact symbol, string, or pattern: it is
///   faster and always reflects the file on disk.
/// - Prefer this tool when a `grep` guess has already failed, or when you are
///   orienting in unfamiliar code.
/// - Results are ranked by similarity and cite the file and line range they came
///   from, so follow up with `read_file` to see the surrounding code.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SemanticSearchToolInput {
    /// A description of what the code does, phrased as you would explain it to
    /// another engineer.
    ///
    /// <example>
    /// "the retry logic for failed uploads"
    /// "where we decide whether a thread is stale"
    /// </example>
    pub query: String,
    /// How many results to return. Defaults to 8.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SemanticSearchToolOutput {
    Success { matches: String },
    Error { error: String },
}

impl From<SemanticSearchToolOutput> for LanguageModelToolResultContent {
    fn from(output: SemanticSearchToolOutput) -> Self {
        match output {
            SemanticSearchToolOutput::Success { matches } => matches.into(),
            SemanticSearchToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct SemanticSearchTool {
    project: Entity<Project>,
}

impl SemanticSearchTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

fn render_matches(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("No code in the index matched “{query}”.");
    }

    let mut rendered = format!(
        "{} result(s) for “{query}”, most similar first.\n",
        results.len()
    );
    for result in results {
        // The score is included because it is the only signal the model has for
        // how much to trust a hit: semantic search always returns its closest
        // matches, even when nothing is actually close.
        let _ = write!(
            rendered,
            "\n## {}:{}-{} (similarity {:.2})\n```\n{}\n```\n",
            result.path.display(),
            result.start_line,
            result.end_line,
            result.score,
            result.text
        );
    }
    rendered
}

impl AgentTool for SemanticSearchTool {
    type Input = SemanticSearchToolInput;
    type Output = SemanticSearchToolOutput;

    const NAME: &'static str = "semantic_search";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Search the codebase for “{}”", input.query).into(),
            Err(_) => "Search the codebase".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();

        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| {
                SemanticSearchToolOutput::Error {
                    error: error.to_string(),
                }
            })?;

            let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

            let index: Entity<CodebaseIndex> = cx
                .update(|cx| codebase_index::index_for_project(&project, cx))
                .ok_or_else(|| SemanticSearchToolOutput::Error {
                    error: "Semantic search is not configured: no embedding endpoint, or no \
                            API key for a remote one (ACUTO_EMBEDDING_API_KEY or \
                            NVIDIA_API_KEY). Use the `grep` tool instead."
                        .into(),
                })?;

            // Built on first use rather than at startup: indexing costs real
            // money per token, so it happens when the capability is actually
            // asked for. Brought up to date on every search after that, which
            // only embeds the chunks that changed -- the rest are reused by
            // digest -- so results never come from code that is gone.
            let (is_empty, is_indexing, root) = index.read_with(cx, |index, _| {
                (
                    index.is_empty(),
                    index.is_indexing(),
                    index.worktree_root().to_path_buf(),
                )
            });

            if is_indexing {
                if is_empty {
                    return Err(SemanticSearchToolOutput::Error {
                        error: "The codebase index is still being built. Use the `grep` \
                                tool for now, or try again shortly."
                            .into(),
                    });
                }
            } else {
                let files = cx
                    .update(|cx| codebase_index::collect_project_files(&project, &root, cx))
                    .await;

                let rebuild = index.update(cx, |index, cx| index.rebuild(files, cx));

                if let Err(error) = rebuild.await {
                    if is_empty {
                        return Err(SemanticSearchToolOutput::Error {
                            error: format!("Could not build the codebase index: {error:#}"),
                        });
                    }
                    log::warn!("could not refresh the codebase index: {error:#}");
                }
            }

            let search = index.read_with(cx, |index, cx| index.search(input.query.clone(), limit, cx));

            match search.await {
                Ok(results) => Ok(SemanticSearchToolOutput::Success {
                    matches: render_matches(&input.query, &results),
                }),
                Err(error) => Err(SemanticSearchToolOutput::Error {
                    error: format!("Semantic search failed: {error:#}"),
                }),
            }
        })
    }
}
