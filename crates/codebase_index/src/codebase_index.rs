//! Semantic search over the project's source, for the agent.
//!
//! The agent can already find code it knows the name of — `grep`, `find_path`,
//! `find_references` all work from an exact string. What it cannot do is find
//! code it can only *describe*: "where do we decide whether a thread is stale",
//! "the retry logic for uploads". This crate closes that gap by embedding the
//! project into vectors once and answering description-shaped questions against
//! them.
//!
//! It is not a replacement for the grep tools and does not try to be. Exact
//! search is faster, always current, and correct for identifiers; this is for
//! when you do not have an identifier to search for.
//!
//! # Shape
//!
//! Files are cut into overlapping line windows, each window is embedded through
//! an OpenAI-compatible `/v1/embeddings` endpoint, and the vectors are kept
//! normalized so that cosine similarity is a plain dot product. Both halves are
//! persisted under the data dir, keyed by a hash of the worktree path, so a
//! restart does not mean re-embedding (and re-paying for) the whole project.
//!
//! Chunking is by lines rather than by syntax on purpose: a tree-sitter
//! dependency would tie indexing to the set of languages with grammars, and a
//! window that spans a function boundary is a much smaller retrieval problem
//! than a file that cannot be indexed at all.

use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext, Context, Entity, Task, TaskExt as _};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Lines per chunk. Large enough to hold a small function whole, small enough
/// that a hit points at something specific rather than at a whole file.
const CHUNK_LINES: usize = 40;

/// Lines each chunk shares with the previous one, so a definition that straddles
/// a boundary still appears intact in one of them.
const CHUNK_OVERLAP_LINES: usize = 10;

/// Texts per embedding request. Bounded because providers cap both request body
/// size and inputs per call, and a rejected batch wastes the whole batch.
const EMBED_BATCH_SIZE: usize = 32;

/// Files above this are skipped: minified bundles and checked-in data files cost
/// far more to embed than they are worth retrieving.
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Where the embedding endpoint lives and which model answers.
///
/// Read from the environment rather than settings so that changing providers
/// does not mean rebuilding the settings schema, which sits near the root of
/// the dependency graph.
#[derive(Clone, Debug)]
pub struct EmbeddingConfig {
    pub api_url: Arc<str>,
    pub model: Arc<str>,
    pub api_key: Option<Arc<str>>,
}

impl EmbeddingConfig {
    pub fn from_env() -> Option<Self> {
        let api_url = std::env::var("ACUTO_EMBEDDING_API_URL").ok()?;
        let model = std::env::var("ACUTO_EMBEDDING_MODEL").ok()?;
        if api_url.trim().is_empty() || model.trim().is_empty() {
            return None;
        }
        let api_key: Option<Arc<str>> = std::env::var("ACUTO_EMBEDDING_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .map(Into::into);
        // Without a key a remote provider refuses every request, and only
        // after the code in it has already been sent. A server on this machine
        // may need none.
        if api_key.is_none() && !is_loopback_url(&api_url) {
            return None;
        }
        Some(Self {
            api_url: api_url.into(),
            model: model.into(),
            api_key,
        })
    }
}

fn is_loopback_url(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.rsplit_once(':').map_or(authority, |(host, _)| host),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// One indexed window of a file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    pub path: PathBuf,
    /// 1-based and inclusive, to match how the rest of the editor talks about
    /// line numbers and how a model is asked to cite them.
    pub start_line: usize,
    pub end_line: usize,
    /// Hash of the chunk's text, so an unchanged chunk is not re-embedded.
    pub digest: String,
}

/// A retrieval hit.
#[derive(Clone, Debug)]
pub struct SearchResult {
    pub path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
    pub text: String,
}

/// Splits text into overlapping line windows.
///
/// Returns nothing for text that is entirely blank, so empty files do not
/// occupy an index slot or an embedding call.
pub fn chunk_text(path: &Path, text: &str) -> Vec<(Chunk, String)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.iter().all(|line| line.trim().is_empty()) {
        return Vec::new();
    }

    let stride = CHUNK_LINES.saturating_sub(CHUNK_OVERLAP_LINES).max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < lines.len() {
        let end = (start + CHUNK_LINES).min(lines.len());
        let body = lines[start..end].join("\n");

        if !body.trim().is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(body.as_bytes());
            let digest = format!("{:x}", hasher.finalize());

            chunks.push((
                Chunk {
                    path: path.to_path_buf(),
                    start_line: start + 1,
                    end_line: end,
                    digest,
                },
                body,
            ));
        }

        if end == lines.len() {
            break;
        }
        start += stride;
    }

    chunks
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
    /// NVIDIA's retrieval models embed queries and documents differently and
    /// refuse a request that does not say which this is.
    #[serde(skip_serializing_if = "Option::is_none")]
    input_type: Option<&'static str>,
    /// NVIDIA refuses an input past the model's length instead of cutting it
    /// unless asked to truncate, and a 40-line chunk of code is often longer.
    #[serde(skip_serializing_if = "Option::is_none")]
    truncate: Option<&'static str>,
}

/// What a batch of text is being embedded as.
#[derive(Clone, Copy)]
enum InputKind {
    Query,
    Passage,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

/// Scales a vector to unit length so that cosine similarity reduces to a dot
/// product at query time.
///
/// A zero vector is left alone rather than divided by zero; it simply never
/// matches anything.
fn normalize(vector: &mut [f32]) {
    let magnitude = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if magnitude > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= magnitude;
        }
    }
}

/// Embeds one batch, returning vectors in the order the inputs were given.
///
/// Providers are permitted to return `data` out of order, and at least one does,
/// so results are placed by their `index` rather than by arrival.
async fn embed_batch(
    http_client: Arc<dyn HttpClient>,
    config: &EmbeddingConfig,
    inputs: &[String],
    kind: InputKind,
) -> Result<Vec<Vec<f32>>> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }

    // Only NVIDIA's API takes these; OpenAI's refuses fields it does not know.
    let is_nvidia = config.api_url.contains("api.nvidia.com");
    let body = serde_json::to_string(&EmbeddingRequest {
        model: &config.model,
        input: inputs,
        input_type: is_nvidia.then_some(match kind {
            InputKind::Query => "query",
            InputKind::Passage => "passage",
        }),
        truncate: is_nvidia.then_some("END"),
    })?;

    let mut builder = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(config.api_url.as_ref())
        .header("Content-Type", "application/json");
    if let Some(api_key) = &config.api_key {
        builder = builder.header("Authorization", format!("Bearer {api_key}"));
    }

    let request = builder.body(http_client::AsyncBody::from(body))?;
    let mut response = http_client.send(request).await?;
    let status = response.status();

    let mut payload = String::new();
    response.body_mut().read_to_string(&mut payload).await?;

    if !status.is_success() {
        bail!("embedding request failed: {status} - {payload}");
    }

    let parsed: EmbeddingResponse = serde_json::from_str(&payload)
        .with_context(|| format!("could not parse embedding response: {payload}"))?;

    if parsed.data.len() != inputs.len() {
        bail!(
            "embedding provider returned {} vectors for {} inputs",
            parsed.data.len(),
            inputs.len()
        );
    }

    let mut ordered = vec![Vec::new(); inputs.len()];
    for datum in parsed.data {
        let slot = ordered
            .get_mut(datum.index)
            .ok_or_else(|| anyhow!("embedding index {} out of range", datum.index))?;
        let mut embedding = datum.embedding;
        normalize(&mut embedding);
        *slot = embedding;
    }

    if ordered.iter().any(|vector| vector.is_empty()) {
        bail!("embedding provider left gaps in the returned batch");
    }

    Ok(ordered)
}

/// The metadata half of a persisted index. The vectors live beside it in a raw
/// `f32` file, because a JSON array of floats costs several times the bytes and
/// is markedly slower to parse.
#[derive(Serialize, Deserialize)]
struct PersistedIndex {
    model: String,
    dimensions: usize,
    chunks: Vec<Chunk>,
}

fn index_paths(worktree_root: &Path) -> (PathBuf, PathBuf) {
    let mut hasher = Sha256::new();
    hasher.update(worktree_root.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    let directory = paths::data_dir().join("codebase_index");
    (
        directory.join(format!("{key}.json")),
        directory.join(format!("{key}.vec")),
    )
}

/// An index over one worktree.
pub struct CodebaseIndex {
    worktree_root: PathBuf,
    config: EmbeddingConfig,
    http_client: Arc<dyn HttpClient>,
    chunks: Vec<Chunk>,
    /// Flattened row-major matrix of `chunks.len()` rows by `dimensions`
    /// columns. Kept flat rather than as a `Vec<Vec<f32>>` so scoring walks one
    /// contiguous allocation.
    vectors: Vec<f32>,
    dimensions: usize,
    indexing: bool,
}

impl CodebaseIndex {
    pub fn new(
        worktree_root: PathBuf,
        config: EmbeddingConfig,
        http_client: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            worktree_root,
            config,
            http_client,
            chunks: Vec::new(),
            vectors: Vec::new(),
            dimensions: 0,
            indexing: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// The folder this index covers.
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    pub fn is_indexing(&self) -> bool {
        self.indexing
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Reads a previously written index, discarding one built by a different
    /// model: vectors from two models are not comparable, and silently mixing
    /// them would return confident nonsense.
    pub fn load(&mut self) -> Result<bool> {
        let (metadata_path, vector_path) = index_paths(&self.worktree_root);
        if !metadata_path.exists() || !vector_path.exists() {
            return Ok(false);
        }

        let metadata: PersistedIndex = serde_json::from_str(&std::fs::read_to_string(
            &metadata_path,
        )?)
        .with_context(|| format!("could not parse index at {}", metadata_path.display()))?;

        if metadata.model != self.config.model.as_ref() {
            log::info!(
                "codebase index was built with {}, now using {}; discarding",
                metadata.model,
                self.config.model
            );
            return Ok(false);
        }

        let mut raw = Vec::new();
        std::fs::File::open(&vector_path)?.read_to_end(&mut raw)?;

        let expected = metadata.chunks.len() * metadata.dimensions;
        if raw.len() != expected * std::mem::size_of::<f32>() {
            log::warn!("codebase index vectors do not match its metadata; discarding");
            return Ok(false);
        }

        let mut vectors = Vec::with_capacity(expected);
        for bytes in raw.chunks_exact(std::mem::size_of::<f32>()) {
            // `chunks_exact` guarantees the length, so this cannot fail; the
            // fallible form avoids an indexing panic if that ever changes.
            let array: [u8; 4] = bytes.try_into()?;
            vectors.push(f32::from_le_bytes(array));
        }

        self.chunks = metadata.chunks;
        self.vectors = vectors;
        self.dimensions = metadata.dimensions;
        Ok(true)
    }

    /// Writes the index to disk off the main thread: the vectors run to
    /// megabytes, and the index is saved after every refresh.
    fn save(&self, cx: &App) -> Task<Result<()>> {
        let (metadata_path, vector_path) = index_paths(&self.worktree_root);
        let metadata = PersistedIndex {
            model: self.config.model.to_string(),
            dimensions: self.dimensions,
            chunks: self.chunks.clone(),
        };
        let vectors = self.vectors.clone();
        cx.background_spawn(async move {
            if let Some(parent) = metadata_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&metadata_path, serde_json::to_string(&metadata)?)?;

            let mut bytes = Vec::with_capacity(vectors.len() * std::mem::size_of::<f32>());
            for value in &vectors {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            let mut file = std::fs::File::create(&vector_path)?;
            file.write_all(&bytes)?;
            file.flush()?;
            Ok(())
        })
    }

    /// Embeds a query and returns the closest chunks.
    pub fn search(
        &self,
        query: String,
        limit: usize,
        cx: &App,
    ) -> Task<Result<Vec<SearchResult>>> {
        if self.chunks.is_empty() {
            return Task::ready(Err(anyhow!(
                "the codebase index is empty; it is built the first time semantic search runs"
            )));
        }

        let http_client = self.http_client.clone();
        let config = self.config.clone();
        let snapshot = IndexSnapshot {
            chunks: self.chunks.clone(),
            vectors: self.vectors.clone(),
            dimensions: self.dimensions,
            worktree_root: self.worktree_root.clone(),
        };

        cx.background_spawn(async move {
            let embedded = embed_batch(
                http_client,
                &config,
                std::slice::from_ref(&query),
                InputKind::Query,
            )
            .await?;
            let query_vector = embedded
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("embedding provider returned nothing for the query"))?;
            Ok(rank(&snapshot, &query_vector, limit))
        })
    }

    /// Walks the worktree, embeds anything new or changed, and persists.
    ///
    /// Chunks whose digest is already present keep their existing vector, so a
    /// rebuild after editing a handful of files costs a handful of files rather
    /// than the project.
    pub fn rebuild(
        &mut self,
        files: Vec<(PathBuf, String)>,
        cx: &mut Context<Self>,
    ) -> Task<Result<usize>> {
        if self.indexing {
            return Task::ready(Err(anyhow!("an index rebuild is already running")));
        }
        self.indexing = true;
        cx.notify();

        let http_client = self.http_client.clone();
        let config = self.config.clone();

        let mut existing: HashMap<String, Vec<f32>> = HashMap::new();
        if self.dimensions > 0 {
            for (row, chunk) in self.chunks.iter().enumerate() {
                let start = row * self.dimensions;
                let Some(slice) = self.vectors.get(start..start + self.dimensions) else {
                    continue;
                };
                existing.insert(chunk.digest.clone(), slice.to_vec());
            }
        }

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    let mut chunks = Vec::new();
                    let mut bodies = Vec::new();
                    for (path, text) in &files {
                        for (chunk, body) in chunk_text(path, text) {
                            chunks.push(chunk);
                            bodies.push(body);
                        }
                    }

                    let mut vectors: Vec<Vec<f32>> = vec![Vec::new(); chunks.len()];
                    let mut pending_indices = Vec::new();
                    let mut pending_texts = Vec::new();

                    for (index, chunk) in chunks.iter().enumerate() {
                        match existing.get(&chunk.digest) {
                            Some(cached) => {
                                if let Some(slot) = vectors.get_mut(index) {
                                    *slot = cached.clone();
                                }
                            }
                            None => {
                                pending_indices.push(index);
                                if let Some(body) = bodies.get(index) {
                                    pending_texts.push(body.clone());
                                }
                            }
                        }
                    }

                    let mut dimensions = existing.values().next().map_or(0, |vector| vector.len());

                    for (batch_number, batch) in
                        pending_texts.chunks(EMBED_BATCH_SIZE).enumerate()
                    {
                        let embedded =
                            embed_batch(http_client.clone(), &config, batch, InputKind::Passage)
                                .await?;
                        for (offset, vector) in embedded.into_iter().enumerate() {
                            if dimensions == 0 {
                                dimensions = vector.len();
                            } else if vector.len() != dimensions {
                                bail!(
                                    "embedding provider returned {} dimensions after {}",
                                    vector.len(),
                                    dimensions
                                );
                            }
                            let flat = batch_number * EMBED_BATCH_SIZE + offset;
                            let Some(&target) = pending_indices.get(flat) else {
                                continue;
                            };
                            if let Some(slot) = vectors.get_mut(target) {
                                *slot = vector;
                            }
                        }
                    }

                    if dimensions == 0 {
                        bail!("nothing was embedded; the project has no indexable text");
                    }

                    // A chunk whose vector never arrived is dropped rather than
                    // padded: a zero row would rank against every query.
                    let mut kept_chunks = Vec::with_capacity(chunks.len());
                    let mut flat = Vec::with_capacity(chunks.len() * dimensions);
                    for (chunk, vector) in chunks.into_iter().zip(vectors) {
                        if vector.len() != dimensions {
                            continue;
                        }
                        kept_chunks.push(chunk);
                        flat.extend_from_slice(&vector);
                    }

                    anyhow::Ok((kept_chunks, flat, dimensions))
                })
                .await;

            this.update(cx, |this, cx| {
                this.indexing = false;
                cx.notify();
                match outcome {
                    Ok((chunks, vectors, dimensions)) => {
                        let count = chunks.len();
                        this.chunks = chunks;
                        this.vectors = vectors;
                        this.dimensions = dimensions;
                        this.save(cx).detach_and_log_err(cx);
                        Ok(count)
                    }
                    Err(error) => Err(error),
                }
            })?
        })
    }
}

/// An immutable copy of the index, so ranking can run on a background thread
/// without holding the entity.
struct IndexSnapshot {
    chunks: Vec<Chunk>,
    vectors: Vec<f32>,
    dimensions: usize,
    worktree_root: PathBuf,
}

/// Ranks indexed chunks against a query embedding.
///
/// Chunk text is read from disk at query time rather than stored: the file may
/// have changed since indexing, and handing the model stale text that it then
/// edits against is worse than handing it text that moved by a line or two.
fn rank(snapshot: &IndexSnapshot, query: &[f32], limit: usize) -> Vec<SearchResult> {
    if snapshot.dimensions == 0 || query.len() != snapshot.dimensions {
        return Vec::new();
    }

    let mut scored: Vec<(usize, f32)> = snapshot
        .vectors
        .chunks_exact(snapshot.dimensions)
        .enumerate()
        .map(|(row, vector)| {
            let score = vector
                .iter()
                .zip(query)
                .map(|(left, right)| left * right)
                .sum::<f32>();
            (row, score)
        })
        .collect();

    scored.sort_unstable_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    scored
        .into_iter()
        .take(limit)
        .filter_map(|(row, score)| {
            let chunk = snapshot.chunks.get(row)?;
            let absolute = snapshot.worktree_root.join(&chunk.path);
            let text = std::fs::read_to_string(&absolute).ok()?;
            let lines: Vec<&str> = text.lines().collect();
            let start = chunk.start_line.saturating_sub(1).min(lines.len());
            let end = chunk.end_line.min(lines.len());
            Some(SearchResult {
                path: chunk.path.clone(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                score,
                text: lines.get(start..end)?.join("
"),
            })
        })
        .collect()
}

/// Collects indexable file contents from a project's worktrees.
pub fn collect_project_files(
    project: &Entity<project::Project>,
    root: &Path,
    cx: &App,
) -> Task<Vec<(PathBuf, String)>> {
    // Only the folder the index is rooted at: its paths are stored relative to
    // that root, so a file from another folder could be embedded and paid for
    // but never found again.
    let mut paths = Vec::new();
    for worktree in project.read(cx).worktrees(cx) {
        let snapshot = worktree.read(cx).snapshot();
        if snapshot.abs_path().as_ref() != root {
            continue;
        }
        for entry in snapshot.files(false, 0) {
            if entry.size > MAX_FILE_BYTES {
                continue;
            }
            paths.push(entry.path.as_std_path().to_path_buf());
        }
    }
    let root = root.to_path_buf();
    // Read off the main thread: a whole project's files take long enough to
    // freeze the window.
    cx.background_spawn(async move {
        paths
            .into_iter()
            .filter_map(|relative| {
                let text = std::fs::read_to_string(root.join(&relative)).ok()?;
                Some((relative, text))
            })
            .collect()
    })
}

/// One index per worktree root, shared by everything that asks for it.
///
/// The index is expensive to build and pointless to duplicate, so it is held
/// globally rather than owned by whichever tool call happened to need it first.
#[derive(Default)]
pub struct CodebaseIndexes(HashMap<PathBuf, Entity<CodebaseIndex>>);

impl gpui::Global for CodebaseIndexes {}

/// Returns the index for a project's first worktree, creating and loading it on
/// first use.
///
/// Returns `None` when no embedding endpoint is configured, which is the signal
/// callers use to explain the feature is switched off rather than broken.
pub fn index_for_project(
    project: &Entity<project::Project>,
    cx: &mut App,
) -> Option<Entity<CodebaseIndex>> {
    let config = EmbeddingConfig::from_env()?;
    let worktree = project.read(cx).worktrees(cx).next()?;
    let root = worktree.read(cx).abs_path().to_path_buf();

    if let Some(existing) = cx
        .try_global::<CodebaseIndexes>()
        .and_then(|indexes| indexes.0.get(&root))
    {
        return Some(existing.clone());
    }

    let http_client = project.read(cx).client().http_client();
    let index = cx.new(|_| {
        let mut index = CodebaseIndex::new(root.clone(), config, http_client);
        if let Err(error) = index.load() {
            log::warn!("could not load the codebase index: {error:#}");
        }
        index
    });

    cx.default_global::<CodebaseIndexes>()
        .0
        .insert(root, index.clone());
    Some(index)
}
