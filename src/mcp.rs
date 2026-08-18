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
    CacheEntryOutput, PageOutput, SearchEngine, SearchOutput, SectionOutput, SectionsOutput,
    UnifiedSearchOutput,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Searches decoded cache records, optionally restricting the record kind.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CacheSearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
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

/// Identifies one decoded cache record returned by cache search.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CacheEntryRequest {
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
        self.with_engine(|engine| {
            engine.search_unified(
                &request.query,
                request.limit.unwrap_or(5),
                request.offset.unwrap_or(0),
            )
        })
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
        self.with_engine(|engine| {
            engine.search(
                &request.query,
                request.limit.unwrap_or(5),
                request.offset.unwrap_or(0),
            )
        })
        .map(Json)
    }

    #[tool(
        name = "search_cache",
        description = "Search revision-pinned decoded OSRS game-cache definitions, interfaces, scripts, and symbols. Use kind to restrict results, for example config/loc, config/varbit, interface, or script. Results include the kind and ID needed for get_cache_entry."
    )]
    pub async fn search_cache(
        &self,
        Parameters(request): Parameters<CacheSearchRequest>,
    ) -> Result<Json<SearchOutput>, String> {
        self.with_engine(|engine| {
            engine.search_cache(
                &request.query,
                request.limit.unwrap_or(5),
                request.offset.unwrap_or(0),
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
        Parameters(request): Parameters<CacheEntryRequest>,
    ) -> Result<Json<CacheEntryOutput>, String> {
        self.with_engine(|engine| engine.cache_entry(&request.kind, &request.id))
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
                Implementation::new("osrs-wiki-offline", env!("CARGO_PKG_VERSION"))
                    .with_title("Offline OSRS Wiki and Cache"),
            )
            .with_instructions(
                "Use search_unified when either source may answer the query. For implementation questions, use search_wiki for gameplay context, then make focused search_cache calls with a kind such as config/loc or config/varbit. Use get_cache_entry for the complete revision-pinned record. Cache data is raw client data, not player state or a gameplay explanation.",
            )
    }
}
