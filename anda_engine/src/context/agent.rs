//! Agent Context Implementation
//!
//! This module provides the core implementation of the Agent context ([`AgentCtx`]) which serves as
//! the primary execution environment for agents in the Anda system. The context provides:
//!
//! - Access to AI models for completions and embeddings
//! - Tool execution capabilities
//! - Agent-to-agent communication
//! - Cryptographic operations
//! - Storage and caching facilities
//! - Canister interaction capabilities
//! - HTTP communication features
//!
//! The [`AgentCtx`] implements multiple traits that provide different sets of functionality:
//! - [`AgentContext`]: Core agent operations and tool/agent management
//! - [`CompletionFeatures`]: AI model completion capabilities
//! - [`EmbeddingFeatures`]: Text embedding generation
//! - [`StateFeatures`]: Context state management
//! - [`KeysFeatures`]: Cryptographic key operations
//! - [`StoreFeatures`]: Persistent storage operations
//! - [`CacheFeatures`]: Caching mechanisms
//! - [`CanisterCaller`]: Canister interaction capabilities
//! - [`HttpFeatures`]: HTTPs communication features
//!
//! The context is designed to be hierarchical, allowing creation of child contexts for specific
//! agents or tools while maintaining access to the core functionality.

use anda_core::{
    AgentArgs, AgentContext, AgentOutput, AgentSet, BaseContext, BoxError, CacheExpiry,
    CacheFeatures, CancellationToken, CanisterCaller, CompletionFeatures, CompletionRequest,
    Embedding, EmbeddingFeatures, FunctionDefinition, HttpFeatures, KeysFeatures, Message,
    ObjectMeta, Path, PutMode, PutResult, StateFeatures, StoreFeatures, ToolCall, ToolSet, Usage,
    Value,
};
use bytes::Bytes;
use candid::{CandidType, Principal, utils::ArgumentEncoder};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_bytes::ByteBuf;
use serde_json::json;
use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use super::base::{BaseCtx, CacheStoreFeatures};
use crate::model::Model;

/// Context for agent operations, providing access to models, tools, and other agents
#[derive(Clone)]
pub struct AgentCtx {
    /// Base context providing fundamental operations
    pub base: BaseCtx,
    /// AI model used for completions and embeddings
    pub(crate) model: Model,
    /// Set of available tools that can be called
    pub(crate) tools: Arc<ToolSet<BaseCtx>>,
    /// Set of available agents that can be invoked
    pub(crate) agents: Arc<AgentSet<AgentCtx>>,
    /// Registered remote engines for tool and agent execution
    pub(crate) remote_engines: Arc<BTreeMap<String, EngineInformation>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EngineInformation {
    pub id: Principal,
    pub name: String,
    pub endpoint: String,
    pub agent_definitions: Vec<FunctionDefinition>,
    pub tool_definitions: Vec<FunctionDefinition>,
}

impl AgentCtx {
    /// Creates a new AgentCtx instance
    ///
    /// # Arguments
    /// * `base` - Base context
    /// * `model` - AI model instance
    /// * `tools` - Set of available tools
    /// * `agents` - Set of available agents
    pub(crate) fn new(
        base: BaseCtx,
        model: Model,
        tools: Arc<ToolSet<BaseCtx>>,
        agents: Arc<AgentSet<AgentCtx>>,
        remote_engines: Arc<BTreeMap<String, EngineInformation>>,
    ) -> Self {
        Self {
            base,
            model,
            tools,
            agents,
            remote_engines,
        }
    }

    /// Creates a child context for a specific agent
    ///
    /// # Arguments
    /// * `agent_name` - Name of the agent to create context for
    pub(crate) fn child(&self, agent_name: &str) -> Result<Self, BoxError> {
        Ok(Self {
            base: self.base.child(format!("A:{}", agent_name))?,
            model: self.model.clone(),
            tools: self.tools.clone(),
            agents: self.agents.clone(),
            remote_engines: self.remote_engines.clone(),
        })
    }

    /// Creates a child base context for a specific tool
    ///
    /// # Arguments
    /// * `tool_name` - Name of the tool to create context for
    pub(crate) fn child_base(&self, tool_name: &str) -> Result<BaseCtx, BoxError> {
        self.base.child(format!("T:{}", tool_name))
    }

    /// Creates a child context with additional user and caller information
    ///
    /// # Arguments
    /// * `agent_name` - Name of the agent
    /// * `user` - Optional user identifier
    /// * `caller` - Optional caller principal
    pub(crate) fn child_with(
        &self,
        agent_name: &str,
        caller: Principal,
        user: Option<String>,
    ) -> Result<Self, BoxError> {
        Ok(Self {
            base: self
                .base
                .child_with(format!("A:{}", agent_name), caller, user)?,
            model: self.model.clone(),
            tools: self.tools.clone(),
            agents: self.agents.clone(),
            remote_engines: self.remote_engines.clone(),
        })
    }

    /// Creates a child base context with additional user and caller information
    ///
    /// # Arguments
    /// * `tool_name` - Name of the tool
    /// * `caller` - Optional caller principal
    /// * `user` - Optional user identifier
    ///
    pub(crate) fn child_base_with(
        &self,
        tool_name: &str,
        caller: Principal,
        user: Option<String>,
    ) -> Result<BaseCtx, BoxError> {
        self.base
            .child_with(format!("T:{}", tool_name), caller, user)
    }
}

impl CacheStoreFeatures for AgentCtx {}

impl AgentContext for AgentCtx {
    /// Retrieves definitions for available tools
    ///
    /// # Arguments
    /// * `names` - Optional filter for specific tool names
    ///
    /// # Returns
    /// Vector of function definitions for the requested tools
    fn tool_definitions(&self, names: Option<&[&str]>) -> Vec<FunctionDefinition> {
        self.tools.definitions(names)
    }

    /// Retrieves definitions for available tools in the remote engines.
    ///
    /// # Arguments
    /// * `endpoint` - Optional filter for specific remote engine endpoint
    /// * `names` - Optional filter for specific tool names
    ///
    /// # Returns
    /// Vector of function definitions for the requested tools
    fn remote_tool_definitions(
        &self,
        endpoint: Option<&str>,
        names: Option<&[&str]>,
    ) -> Vec<FunctionDefinition> {
        if let Some(endpoint) = endpoint {
            for (prefix, engine) in self.remote_engines.iter() {
                if endpoint == engine.endpoint {
                    let prefix = format!("RT_{prefix}");
                    return engine
                        .tool_definitions
                        .iter()
                        .filter_map(|d| {
                            if let Some(names) = names {
                                if names.contains(&d.name.as_str()) {
                                    Some(d.clone().name_with_prefix(&prefix))
                                } else {
                                    None
                                }
                            } else {
                                Some(d.clone().name_with_prefix(&prefix))
                            }
                        })
                        .collect();
                }
            }
        }

        let mut definitions = Vec::with_capacity(
            self.remote_engines
                .iter()
                .map(|(_, e)| e.tool_definitions.len())
                .sum(),
        );
        for (prefix, engine) in self.remote_engines.iter() {
            let prefix = format!("RT_{prefix}");
            definitions.extend(engine.tool_definitions.iter().filter_map(|d| {
                if let Some(names) = names {
                    if names.contains(&d.name.as_str()) {
                        Some(d.clone().name_with_prefix(&prefix))
                    } else {
                        None
                    }
                } else {
                    Some(d.clone().name_with_prefix(&prefix))
                }
            }));
        }

        definitions
    }

    /// Retrieves definitions for available agents
    ///
    /// # Arguments
    /// * `names` - Optional filter for specific agent names
    /// * `with_prefix` - Flag to add the prefix `LA_` to agent names to distinguish from tools
    ///
    /// # Returns
    /// Vector of function definitions for the requested agents
    fn agent_definitions(
        &self,
        names: Option<&[&str]>,
        with_prefix: bool,
    ) -> Vec<FunctionDefinition> {
        let res = self.agents.definitions(names);
        if with_prefix {
            res.into_iter()
                .map(|mut d| {
                    d.name = format!("LA_{}", d.name);
                    d
                })
                .collect()
        } else {
            res
        }
    }

    /// Retrieves definitions for available agents in the remote engines.
    ///
    /// # Arguments
    /// * `endpoint` - Optional filter for specific remote engine endpoint
    /// * `names` - Optional filter for specific agent names
    ///
    /// # Returns
    /// Vector of function definitions for the requested agents
    fn remote_agent_definitions(
        &self,
        endpoint: Option<&str>,
        names: Option<&[&str]>,
    ) -> Vec<FunctionDefinition> {
        if let Some(endpoint) = endpoint {
            for (prefix, engine) in self.remote_engines.iter() {
                if endpoint == engine.endpoint {
                    let prefix = format!("RA_{prefix}");
                    return engine
                        .agent_definitions
                        .iter()
                        .filter_map(|d| {
                            if let Some(names) = names {
                                if names.contains(&d.name.as_str()) {
                                    Some(d.clone().name_with_prefix(&prefix))
                                } else {
                                    None
                                }
                            } else {
                                Some(d.clone().name_with_prefix(&prefix))
                            }
                        })
                        .collect();
                }
            }
        }

        let mut definitions = Vec::with_capacity(
            self.remote_engines
                .iter()
                .map(|(_, e)| e.agent_definitions.len())
                .sum(),
        );
        for (prefix, engine) in self.remote_engines.iter() {
            let prefix = format!("RA_{prefix}");
            definitions.extend(engine.agent_definitions.iter().filter_map(|d| {
                if let Some(names) = names {
                    if names.contains(&d.name.as_str()) {
                        Some(d.clone().name_with_prefix(&prefix))
                    } else {
                        None
                    }
                } else {
                    Some(d.clone().name_with_prefix(&prefix))
                }
            }));
        }

        definitions
    }

    /// Executes a tool call with the given arguments
    ///
    /// # Arguments
    /// * `name` - Name of the tool to call
    /// * `args` - Arguments for the tool call as a JSON string
    ///
    /// # Returns
    /// Tuple containing the result string and a boolean indicating if further processing is needed
    async fn tool_call(&self, name: &str, args: String) -> Result<String, BoxError> {
        // find registered remote tool and call it
        if let Some(name) = name.strip_prefix("RT_") {
            for (prefix, engine) in self.remote_engines.iter() {
                if let Some(tool_name) = name.strip_prefix(prefix) {
                    return self
                        .remote_tool_call(&engine.endpoint, tool_name, args)
                        .await;
                }
            }
        }

        if self.tools.contains(name) {
            let ctx = self.child_base(name)?;
            return self.tools.call(name, ctx, args).await;
        }

        Err(format!("tool {} not found", name).into())
    }

    /// Executes a remote tool call via HTTP RPC
    ///
    /// # Arguments
    /// * `endpoint` - Remote endpoint URL
    /// * `name` - Name of the tool to call
    /// * `args` - Arguments for the tool call as a JSON string
    async fn remote_tool_call(
        &self,
        endpoint: &str,
        name: &str,
        args: String,
    ) -> Result<String, BoxError> {
        self.https_signed_rpc(endpoint, "tool_call", &(name, args))
            .await
    }

    /// Runs an agent with the given prompt and optional attachment
    ///
    /// # Arguments
    /// * `name` - Name of the agent to run
    /// * `prompt` - Input prompt for the agent
    /// * `attachment` - Optional binary attachment
    ///
    /// # Returns
    /// [`AgentOutput`] containing the result of the agent execution
    async fn agent_run(
        &self,
        name: &str,
        prompt: String,
        attachment: Option<Vec<u8>>,
    ) -> Result<AgentOutput, BoxError> {
        // find registered remote agent and run it
        if let Some(name) = name.strip_prefix("RA_") {
            for (prefix, engine) in self.remote_engines.iter() {
                if let Some(agent_name) = name.strip_prefix(prefix) {
                    return self
                        .remote_agent_run(&engine.endpoint, agent_name, prompt, attachment)
                        .await;
                }
            }
        }

        let name = name.strip_prefix("LA_").unwrap_or(name);
        if self.agents.contains(name) {
            let ctx = self.child(name)?;
            return self.agents.run(name, ctx, prompt, attachment).await;
        }

        Err(format!("agent {} not found", name).into())
    }

    /// Runs a remote agent via HTTP RPC
    ///
    /// # Arguments
    /// * `endpoint` - Remote endpoint URL
    /// * `agent_name` - Name of the agent to run
    /// * `prompt` - Input prompt for the agent
    /// * `attachment` - Optional binary attachment
    async fn remote_agent_run(
        &self,
        endpoint: &str,
        agent_name: &str,
        prompt: String,
        attachment: Option<Vec<u8>>,
    ) -> Result<AgentOutput, BoxError> {
        self.https_signed_rpc(
            endpoint,
            "agent_run",
            &(agent_name, prompt, attachment.map(ByteBuf::from)),
        )
        .await
    }
}

impl CompletionFeatures for AgentCtx {
    /// Executes a completion request with automatic tool call handling
    ///
    /// This method handles the completion request in a loop, automatically executing
    /// any tool calls that are returned by the model and feeding their results back
    /// into the model until no more tool calls need to be processed.
    ///
    /// # Arguments
    /// * `req` - [`CompletionRequest`] containing the input parameters
    ///
    /// # Returns
    /// [`AgentOutput`] containing the final completion result
    ///
    /// # Process Flow
    /// 1. Makes initial completion request to the model
    /// 2. If tool calls are returned:
    ///    - Executes each tool call
    ///    - Adds tool results to the chat history
    ///    - Repeats the completion with updated history
    /// 3. Returns final result when no more tool calls need processing
    async fn completion(&self, mut req: CompletionRequest) -> Result<AgentOutput, BoxError> {
        let mut tool_calls_result: Vec<ToolCall> = Vec::new();
        let mut usage = Usage::default();
        loop {
            let mut output = self.model.completion(req.clone()).await?;
            usage.accumulate(&output.usage);
            // automatically executes tools calls
            let mut tool_calls_continue: Vec<Value> = Vec::new();
            if let Some(tool_calls) = &mut output.tool_calls {
                for tool in tool_calls.iter_mut() {
                    if !req.tools.iter().any(|t| t.name == tool.name) {
                        // tool already called, skip
                        continue;
                    }

                    // remove called tool from req.tools
                    req.tools.retain(|t| t.name != tool.name);
                    if self.tools.contains(&tool.name) || tool.name.starts_with("RT_") {
                        match self.tool_call(&tool.name, tool.args.clone()).await {
                            Ok(result) => {
                                tool_calls_continue.push(json!(Message {
                                    role: "tool".to_string(),
                                    content: result.clone().into(),
                                    name: None,
                                    tool_call_id: Some(tool.id.clone()),
                                }));

                                tool.result = Some(result);
                            }
                            Err(err) => {
                                output.failed_reason = Some(err.to_string());
                                output.usage = usage;
                                return Ok(output);
                            }
                        }
                    } else if self.agents.contains(&tool.name)
                        || tool.name.starts_with("LA_")
                        || tool.name.starts_with("RA_")
                    {
                        let args: AgentArgs = serde_json::from_str(&tool.args)?;
                        match self.agent_run(&tool.name, args.prompt, None).await {
                            Ok(res) => {
                                usage.accumulate(&res.usage);
                                if res.failed_reason.is_some() {
                                    output.failed_reason = res.failed_reason;
                                    return Ok(output);
                                }

                                tool_calls_continue.push(json!(Message {
                                    role: "tool".to_string(),
                                    content: res.content.clone().into(),
                                    name: None,
                                    tool_call_id: Some(tool.id.clone()),
                                }));
                                tool.result = Some(res.content);
                            }
                            Err(err) => {
                                output.failed_reason = Some(err.to_string());
                                output.usage = usage;
                                return Ok(output);
                            }
                        }
                    }
                    // ignore unknown tool
                }

                tool_calls_result.append(tool_calls);
            }

            if tool_calls_continue.is_empty() {
                output.tool_calls = if tool_calls_result.is_empty() {
                    None
                } else {
                    Some(tool_calls_result)
                };
                output.usage = usage;
                return Ok(output);
            }

            req.system = None;
            req.documents.clear();
            req.prompt = "".to_string();
            req.chat_history = output.full_history.unwrap_or_default();
            req.chat_history.append(&mut tool_calls_continue);
        }
    }
}

impl EmbeddingFeatures for AgentCtx {
    /// Gets the number of dimensions for the embedding model
    fn ndims(&self) -> usize {
        self.model.ndims()
    }

    /// Generates embeddings for a collection of texts
    ///
    /// # Arguments
    /// * `texts` - Collection of text strings to embed
    ///
    /// # Returns
    /// Vector of embeddings, one for each input text
    async fn embed(
        &self,
        texts: impl IntoIterator<Item = String> + Send,
    ) -> Result<(Vec<Embedding>, Usage), BoxError> {
        self.model.embed(texts).await
    }

    /// Generates an embedding for a single query text
    ///
    /// # Arguments
    /// * `text` - Input text to embed
    ///
    /// # Returns
    /// Embedding vector for the input text
    async fn embed_query(&self, text: &str) -> Result<(Embedding, Usage), BoxError> {
        self.model.embed_query(text).await
    }
}

impl BaseContext for AgentCtx {}

impl StateFeatures for AgentCtx {
    /// agent ID
    fn id(&self) -> Principal {
        self.base.id()
    }

    /// caller ID
    fn caller(&self) -> Principal {
        self.base.caller()
    }

    /// Gets the current user identifier, if available
    fn user(&self) -> Option<String> {
        self.base.user()
    }

    /// Gets the cancellation token for the current context
    fn cancellation_token(&self) -> CancellationToken {
        self.base.cancellation_token()
    }

    /// Gets the elapsed time since the context was created
    fn time_elapsed(&self) -> Duration {
        self.base.time_elapsed()
    }
}

impl KeysFeatures for AgentCtx {
    /// Derives a 256-bit AES-GCM key from the given derivation path
    async fn a256gcm_key(&self, derivation_path: &[&[u8]]) -> Result<[u8; 32], BoxError> {
        self.base.a256gcm_key(derivation_path).await
    }

    /// Signs a message using Ed25519 signature scheme from the given derivation path
    async fn ed25519_sign_message(
        &self,
        derivation_path: &[&[u8]],
        message: &[u8],
    ) -> Result<[u8; 64], BoxError> {
        self.base
            .ed25519_sign_message(derivation_path, message)
            .await
    }

    /// Verifies an Ed25519 signature from the given derivation path
    async fn ed25519_verify(
        &self,
        derivation_path: &[&[u8]],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), BoxError> {
        self.base
            .ed25519_verify(derivation_path, message, signature)
            .await
    }

    /// Gets the public key for Ed25519 from the given derivation path
    async fn ed25519_public_key(&self, derivation_path: &[&[u8]]) -> Result<[u8; 32], BoxError> {
        self.base.ed25519_public_key(derivation_path).await
    }

    /// Signs a message using Secp256k1 BIP340 Schnorr signature from the given derivation path
    async fn secp256k1_sign_message_bip340(
        &self,
        derivation_path: &[&[u8]],
        message: &[u8],
    ) -> Result<[u8; 64], BoxError> {
        self.base
            .secp256k1_sign_message_bip340(derivation_path, message)
            .await
    }

    /// Verifies a Secp256k1 BIP340 Schnorr signature from the given derivation path
    async fn secp256k1_verify_bip340(
        &self,
        derivation_path: &[&[u8]],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), BoxError> {
        self.base
            .secp256k1_verify_bip340(derivation_path, message, signature)
            .await
    }

    /// Signs a message using Secp256k1 ECDSA signature from the given derivation path
    async fn secp256k1_sign_message_ecdsa(
        &self,
        derivation_path: &[&[u8]],
        message: &[u8],
    ) -> Result<[u8; 64], BoxError> {
        self.base
            .secp256k1_sign_message_ecdsa(derivation_path, message)
            .await
    }

    /// Verifies a Secp256k1 ECDSA signature from the given derivation path
    async fn secp256k1_verify_ecdsa(
        &self,
        derivation_path: &[&[u8]],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), BoxError> {
        self.base
            .secp256k1_verify_ecdsa(derivation_path, message, signature)
            .await
    }

    /// Gets the compressed SEC1-encoded public key for Secp256k1 from the given derivation path
    async fn secp256k1_public_key(&self, derivation_path: &[&[u8]]) -> Result<[u8; 33], BoxError> {
        self.base.secp256k1_public_key(derivation_path).await
    }
}

impl StoreFeatures for AgentCtx {
    /// Retrieves data from storage at the specified path
    async fn store_get(&self, path: &Path) -> Result<(bytes::Bytes, ObjectMeta), BoxError> {
        self.base.store_get(path).await
    }

    /// Lists objects in storage with optional prefix and offset filters
    ///
    /// # Arguments
    /// * `prefix` - Optional path prefix to filter results
    /// * `offset` - Optional path to start listing from (exclude)
    async fn store_list(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> Result<Vec<ObjectMeta>, BoxError> {
        self.base.store_list(prefix, offset).await
    }

    /// Stores data at the specified path with a given write mode
    ///
    /// # Arguments
    /// * `path` - Target storage path
    /// * `mode` - Write mode (Create, Overwrite, etc.)
    /// * `val` - Data to store as bytes
    async fn store_put(
        &self,
        path: &Path,
        mode: PutMode,
        val: bytes::Bytes,
    ) -> Result<PutResult, BoxError> {
        self.base.store_put(path, mode, val).await
    }

    /// Renames a storage object if the target path doesn't exist
    ///
    /// # Arguments
    /// * `from` - Source path
    /// * `to` - Destination path
    async fn store_rename_if_not_exists(&self, from: &Path, to: &Path) -> Result<(), BoxError> {
        self.base.store_rename_if_not_exists(from, to).await
    }

    /// Deletes data at the specified path
    ///
    /// # Arguments
    /// * `path` - Path of the object to delete
    async fn store_delete(&self, path: &Path) -> Result<(), BoxError> {
        self.base.store_delete(path).await
    }
}

impl CacheFeatures for AgentCtx {
    /// Checks if a key exists in the cache
    fn cache_contains(&self, key: &str) -> bool {
        self.base.cache_contains(key)
    }

    /// Gets a cached value by key, returns error if not found or deserialization fails
    async fn cache_get<T>(&self, key: &str) -> Result<T, BoxError>
    where
        T: DeserializeOwned,
    {
        self.base.cache_get(key).await
    }

    /// Gets a cached value or initializes it if missing
    ///
    /// If key doesn't exist, calls init function to create value and cache it
    async fn cache_get_with<T, F>(&self, key: &str, init: F) -> Result<T, BoxError>
    where
        T: Sized + DeserializeOwned + Serialize + Send,
        F: Future<Output = Result<(T, Option<CacheExpiry>), BoxError>> + Send + 'static,
    {
        // futures_util::pin_mut!(init);
        self.base.cache_get_with(key, init).await
    }

    /// Sets a value in cache with optional expiration policy
    async fn cache_set<T>(&self, key: &str, val: (T, Option<CacheExpiry>))
    where
        T: Sized + Serialize + Send,
    {
        self.base.cache_set(key, val).await
    }

    /// Sets a value in cache if key doesn't exist, returns true if set
    async fn cache_set_if_not_exists<T>(&self, key: &str, val: (T, Option<CacheExpiry>)) -> bool
    where
        T: Sized + Serialize + Send,
    {
        self.base.cache_set_if_not_exists(key, val).await
    }

    /// Deletes a cached value by key, returns true if key existed
    async fn cache_delete(&self, key: &str) -> bool {
        self.base.cache_delete(key).await
    }

    /// Returns an iterator over all cached items with raw value
    fn cache_raw_iter(
        &self,
    ) -> impl Iterator<Item = (Arc<String>, Arc<(Bytes, Option<CacheExpiry>)>)> {
        self.base.cache_raw_iter()
    }
}

impl CanisterCaller for AgentCtx {
    /// Performs a query call to a canister (read-only, no state changes)
    ///
    /// # Arguments
    /// * `canister` - Target canister principal
    /// * `method` - Method name to call
    /// * `args` - Input arguments encoded in Candid format
    async fn canister_query<
        In: ArgumentEncoder + Send,
        Out: CandidType + for<'a> candid::Deserialize<'a>,
    >(
        &self,
        canister: &Principal,
        method: &str,
        args: In,
    ) -> Result<Out, BoxError> {
        self.base.canister_query(canister, method, args).await
    }

    /// Performs an update call to a canister (may modify state)
    ///
    /// # Arguments
    /// * `canister` - Target canister principal
    /// * `method` - Method name to call
    /// * `args` - Input arguments encoded in Candid format
    async fn canister_update<
        In: ArgumentEncoder + Send,
        Out: CandidType + for<'a> candid::Deserialize<'a>,
    >(
        &self,
        canister: &Principal,
        method: &str,
        args: In,
    ) -> Result<Out, BoxError> {
        self.base.canister_update(canister, method, args).await
    }
}

impl HttpFeatures for AgentCtx {
    /// Makes an HTTPs request
    ///
    /// # Arguments
    /// * `url` - Target URL, should start with `https://`
    /// * `method` - HTTP method (GET, POST, etc.)
    /// * `headers` - Optional HTTP headers
    /// * `body` - Optional request body (default empty)
    async fn https_call(
        &self,
        url: &str,
        method: http::Method,
        headers: Option<http::HeaderMap>,
        body: Option<Vec<u8>>, // default is empty
    ) -> Result<reqwest::Response, BoxError> {
        self.base.https_call(url, method, headers, body).await
    }

    /// Makes a signed HTTPs request with message authentication
    ///
    /// # Arguments
    /// * `url` - Target URL
    /// * `method` - HTTP method (GET, POST, etc.)
    /// * `message_digest` - 32-byte message digest for signing
    /// * `headers` - Optional HTTP headers
    /// * `body` - Optional request body (default empty)
    async fn https_signed_call(
        &self,
        url: &str,
        method: http::Method,
        message_digest: [u8; 32],
        headers: Option<http::HeaderMap>,
        body: Option<Vec<u8>>, // default is empty
    ) -> Result<reqwest::Response, BoxError> {
        self.base
            .https_signed_call(url, method, message_digest, headers, body)
            .await
    }

    /// Makes a signed CBOR-encoded RPC call
    ///
    /// # Arguments
    /// * `endpoint` - URL endpoint to send the request to
    /// * `method` - RPC method name to call
    /// * `args` - Arguments to serialize as CBOR and send with the request
    async fn https_signed_rpc<T>(
        &self,
        endpoint: &str,
        method: &str,
        args: impl Serialize + Send,
    ) -> Result<T, BoxError>
    where
        T: DeserializeOwned,
    {
        self.base.https_signed_rpc(endpoint, method, args).await
    }
}

#[cfg(test)]
mod tests {
    use ciborium::from_reader;
    use ic_cose_types::to_cbor_bytes;
    use serde_json::json;

    #[test]
    fn json_in_cbor_works() {
        let json = json!({
            "level": "info",
            "message": "Hello, world!",
            "timestamp": "2021-09-01T12:00:00Z",
            "data": {
                "key": "value",
                "number": 42,
                "flag": true
            }
        });
        let data = to_cbor_bytes(&json);
        let val: serde_json::Value = from_reader(&data[..]).unwrap();
        assert_eq!(json, val);
    }
}
