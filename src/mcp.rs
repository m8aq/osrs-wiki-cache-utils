use std::sync::{Arc, Mutex};

use anyhow::Result;
use rmcp::{
    Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::index::{
    CacheEntryOutput, CodeEntryOutput, PageOutput, SearchEngine, SearchOutput, SectionOutput,
    SectionsOutput, UnifiedSearchOutput,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    pub limit: Option<usize>,
}

/// Searches repository records, optionally restricting their kind.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepositorySearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageRequest {
    pub title: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SectionRequest {
    pub title: String,
    pub section: i64,
}

/// Identifies one repository record returned by search.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepositoryEntryRequest {
    pub kind: String,
    pub id: String,
}

#[derive(Clone)]
pub struct OfflineWiki {
    engine: Arc<Mutex<SearchEngine>>,
    tool_router: ToolRouter<Self>,
}

impl OfflineWiki {
    pub fn new(engine: SearchEngine) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
            tool_router: Self::tool_router(),
        }
    }

    fn with_engine<T>(
        &self,
        operation: impl FnOnce(&mut SearchEngine) -> Result<T>,
    ) -> Result<T, String> {
        let mut engine = self
            .engine
            .lock()
            .map_err(|_| "search index lock failed".to_string())?;
        operation(&mut engine).map_err(|error| error.to_string())
    }
}

#[tool_router(router = tool_router)]
impl OfflineWiki {
    #[tool(
        name = "search_unified",
        description = "Search all available OSRS Wiki and decoded game-cache content in one BM25 ranking with exact title, alias, symbol, and ID boosts. Each result identifies its source."
    )]
    pub async fn search_unified(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<UnifiedSearchOutput>, String> {
        self.with_engine(|engine| engine.search_unified(&request.query, request.limit.unwrap_or(5)))
            .map(Json)
    }

    #[tool(
        name = "search_wiki",
        description = "Search the offline Old School RuneScape Wiki snapshot using stemmed SQLite full-text search with exact title and redirect boosts."
    )]
    pub async fn search_wiki(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<SearchOutput>, String> {
        self.with_engine(|engine| engine.search(&request.query, request.limit.unwrap_or(5)))
            .map(Json)
    }

    #[tool(
        name = "search_cache",
        description = "Search revision-pinned decoded OSRS game-cache definitions, interfaces, scripts, and symbols. Use kind to restrict results, for example config/loc, config/varbit, interface, or script. Results include the kind and ID needed for get_cache_entry."
    )]
    pub async fn search_cache(
        &self,
        Parameters(request): Parameters<RepositorySearchRequest>,
    ) -> Result<Json<SearchOutput>, String> {
        self.with_engine(|engine| {
            engine.search_cache(
                &request.query,
                request.limit.unwrap_or(5),
                request.kind.as_deref(),
            )
        })
        .map(Json)
    }

    #[tool(
        name = "get_cache_entry",
        description = "Get one decoded cache record using the kind and ID returned by search_cache."
    )]
    pub async fn get_cache_entry(
        &self,
        Parameters(request): Parameters<RepositoryEntryRequest>,
    ) -> Result<Json<CacheEntryOutput>, String> {
        self.with_engine(|engine| engine.cache_entry(&request.kind, &request.id))
            .map(Json)
    }

    #[tool(
        name = "search_code",
        description = "Search revision-pinned Java from RuneLite API/client, HTTP API, Plugin Hub plugins, and Plugin Hub tooling. Restrict by runelite-api, runelite-client, runelite-http-api, pluginhub-tooling, or pluginhub/<internalName>. Results include the exact kind and ID needed for get_code_entry."
    )]
    pub async fn search_code(
        &self,
        Parameters(request): Parameters<RepositorySearchRequest>,
    ) -> Result<Json<SearchOutput>, String> {
        self.with_engine(|engine| {
            engine.search_code(
                &request.query,
                request.limit.unwrap_or(5),
                request.kind.as_deref(),
            )
        })
        .map(Json)
    }

    #[tool(
        name = "get_code_entry",
        description = "Get one exact RuneLite, Plugin Hub, or plugin repository source file using the case-sensitive kind and ID returned by search_code."
    )]
    pub async fn get_code_entry(
        &self,
        Parameters(request): Parameters<RepositoryEntryRequest>,
    ) -> Result<Json<CodeEntryOutput>, String> {
        self.with_engine(|engine| engine.code_entry(&request.kind, &request.id))
            .map(Json)
    }

    #[tool(
        name = "get_wiki_page",
        description = "Get a bounded readable page and section index from the offline Wiki snapshot."
    )]
    pub async fn get_wiki_page(
        &self,
        Parameters(request): Parameters<PageRequest>,
    ) -> Result<Json<PageOutput>, String> {
        self.with_engine(|engine| engine.page(&request.title))
            .map(Json)
    }

    #[tool(
        name = "get_wiki_sections",
        description = "List sections for a page in the offline Wiki snapshot."
    )]
    pub async fn get_wiki_sections(
        &self,
        Parameters(request): Parameters<PageRequest>,
    ) -> Result<Json<SectionsOutput>, String> {
        self.with_engine(|engine| engine.sections(&request.title))
            .map(Json)
    }

    #[tool(
        name = "get_wiki_section",
        description = "Get one bounded page section by the index returned from get_wiki_sections."
    )]
    pub async fn get_wiki_section(
        &self,
        Parameters(request): Parameters<SectionRequest>,
    ) -> Result<Json<SectionOutput>, String> {
        self.with_engine(|engine| engine.section(&request.title, request.section))
            .map(Json)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OfflineWiki {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("osrs-wiki-cache-utils", env!("CARGO_PKG_VERSION"))
                    .with_title("Offline OSRS Wiki, Cache, and RuneLite Code"),
            )
            .with_instructions(
                "Search is lexical: translate natural-language requests into short, focused queries and try multiple vocabulary variants when needed. Use search_unified for Wiki and game-cache questions. For plugin implementation questions, combine search_wiki for gameplay context, search_cache for raw client definitions, and search_code for RuneLite or Plugin Hub implementations. Use get_cache_entry or get_code_entry for the complete revision-pinned record.",
            )
    }
}
