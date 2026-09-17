use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use agentic_core::DatabaseBackend;
use agentic_core::config::{
    Config, DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS, DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
    DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS, DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS, DEFAULT_POSTGRES_MAX_CONNECTIONS,
    DEFAULT_POSTGRES_MAX_LIFETIME_SECONDS, DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS,
    DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS, DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES,
    DEFAULT_SQLITE_MAX_CONNECTIONS, DEFAULT_SQLITE_MMAP_SIZE_BYTES, PostgresConfig, SqliteConfig, SqliteTempStore,
    ToolRuntimeConfig, WebSearchProviderConfig, WebSearchProviderKind, default_database_url, ensure_agentic_api_home,
    normalize_base_url,
};
use agentic_core::error::Error;
use agentic_server::app::DEFAULT_MAX_REQUEST_BODY_SIZE;
use agentic_server::auth::OidcConfig;

mod config_file;
mod server;

use config_file::{
    FileConfig, McpFileConfig, MessagesGatewayFileConfig, ServerFileConfig, ToolsFileConfig, WebSearchFileConfig,
};
use server::GatewayOptions;

/// Environment override for the serialized request-size ceiling.
const MAX_REQUEST_BODY_SIZE_ENV: &str = "AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES";

/// Environment override for the `web_search` backend (`you` or `brave`).
const WEB_SEARCH_PROVIDER_ENV: &str = "AGENTIC_WEB_SEARCH_PROVIDER";
/// Provider-neutral environment override for the `web_search` endpoint.
const WEB_SEARCH_BASE_URL_ENV: &str = "AGENTIC_WEB_SEARCH_BASE_URL";
/// Legacy You.com endpoint override, honored only when You.com is selected.
const YOU_API_BASE_URL_ENV: &str = "YOU_API_BASE_URL";
/// Environment override for the concurrent-query ceiling of one batched search.
const WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV: &str = "AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES";

#[derive(Args, Clone)]
struct CommonArgs {
    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true, global = true)]
    openai_api_key: Option<String>,

    /// OIDC issuer for optional inbound bearer-token authentication.
    #[arg(long, env = "OIDC_ISSUER", global = true)]
    oidc_issuer: Option<String>,

    /// Required bearer-token audience when `OIDC_ISSUER` is configured.
    #[arg(long, env = "OIDC_AUDIENCE", global = true)]
    oidc_audience: Option<String>,

    #[arg(long, env = "GATEWAY_HOST", default_value = "0.0.0.0", global = true)]
    gateway_host: String,

    #[arg(long, env = "GATEWAY_PORT", default_value_t = 9000, global = true)]
    gateway_port: u16,

    #[arg(long, default_value_t = 600.0, global = true)]
    llm_ready_timeout_s: f64,

    #[arg(long, default_value_t = 2.0, global = true)]
    llm_ready_interval_s: f64,

    /// Skip the upstream /health readiness probe. Useful for hosted OpenAI-compatible providers.
    #[arg(long, env = "SKIP_LLM_READY_CHECK", default_value_t = false, global = true)]
    skip_llm_ready_check: bool,

    /// Maximum serialized request size in bytes for HTTP bodies and WebSocket messages.
    /// Covers JSON overhead, replayed history, and base64 attachments; unrelated to the
    /// upstream token context limit. Overrides `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES` and
    /// `server.max_request_body_size_bytes` in the configuration file.
    #[arg(long, global = true)]
    max_request_body_size_bytes: Option<NonZeroUsize>,

    /// `SQLite` or `PostgreSQL` URL for conversation and response storage.
    /// Defaults to `agentic_api.db` in the Agentic API home directory.
    #[arg(
        long,
        visible_alias = "database-url",
        env = "DATABASE_URL",
        hide_env_values = true,
        global = true
    )]
    db_url: Option<String>,
}

fn oidc_config_from_values(
    issuer: Option<&str>,
    audience: Option<&str>,
) -> Result<Option<OidcConfig>, server::ServerError> {
    match (issuer, audience) {
        (None, None) => Ok(None),
        (Some(issuer), Some(audience)) => Ok(Some(OidcConfig::new(issuer, audience)?)),
        _ => Err(Error::Config("OIDC_ISSUER and OIDC_AUDIENCE must be configured together".to_owned()).into()),
    }
}

#[derive(Parser)]
#[command(
    name = "agentic-server",
    about = "Stateful API gateway for vLLM Responses API",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Base URL for the standalone OpenAI-compatible inference server.
    #[arg(long, env = "LLM_API_BASE")]
    llm_api_base: Option<String>,

    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Subcommand)]
enum Commands {
    /// Spawn vLLM and run the gateway in the foreground
    Serve {
        /// Model name or path
        model: String,

        /// vLLM server port
        #[arg(long, default_value_t = 8000)]
        port: u16,

        /// Additional arguments passed through to vLLM
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        llm_args: Vec<String>,
    },
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64, Error> {
    parse_env_u64_value(name, std::env::var(name), default)
}

fn parse_env_u64_value(name: &str, value: Result<String, std::env::VarError>, default: u64) -> Result<u64, Error> {
    match value {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|e| Error::Config(format!("{name} must be an unsigned integer: {e}"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(Error::Config(format!("failed to read {name}: {e}"))),
    }
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32, Error> {
    parse_env_u32_value(name, std::env::var(name), default)
}

fn parse_env_u32_value(name: &str, value: Result<String, std::env::VarError>, default: u32) -> Result<u32, Error> {
    match value {
        Ok(value) => {
            let parsed = value
                .parse::<u32>()
                .map_err(|e| Error::Config(format!("{name} must be a positive integer: {e}")))?;
            if parsed == 0 {
                return Err(Error::Config(format!("{name} must be greater than 0")));
            }
            Ok(parsed)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(Error::Config(format!("failed to read {name}: {e}"))),
    }
}

fn parse_env_nonzero_usize(name: &str, default: NonZeroUsize) -> Result<NonZeroUsize, Error> {
    parse_env_nonzero_usize_value(name, std::env::var(name), default)
}

fn parse_env_nonzero_usize_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default: NonZeroUsize,
) -> Result<NonZeroUsize, Error> {
    match value {
        Ok(value) => value
            .parse::<NonZeroUsize>()
            .map_err(|error| Error::Config(format!("{name} must be a positive integer: {error}"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(Error::Config(format!("failed to read {name}: {error}"))),
    }
}

/// Resolves the request-size ceiling as CLI argument > environment variable >
/// configuration file > default.
///
/// An explicit CLI argument short-circuits the lower-priority sources, so a
/// stale or malformed `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES` inherited from the
/// environment cannot block startup when the operator names a valid value.
fn resolve_max_request_body_size(cli: Option<NonZeroUsize>, file: Option<NonZeroUsize>) -> Result<NonZeroUsize, Error> {
    resolve_max_request_body_size_value(cli, file, std::env::var(MAX_REQUEST_BODY_SIZE_ENV))
}

fn resolve_max_request_body_size_value(
    cli: Option<NonZeroUsize>,
    file: Option<NonZeroUsize>,
    value: Result<String, std::env::VarError>,
) -> Result<NonZeroUsize, Error> {
    if let Some(cli) = cli {
        return Ok(cli);
    }
    parse_env_nonzero_usize_value(
        MAX_REQUEST_BODY_SIZE_ENV,
        value,
        file.unwrap_or(DEFAULT_MAX_REQUEST_BODY_SIZE),
    )
}

fn parse_env_duration(name: &str, default_seconds: u64) -> Result<Duration, Error> {
    parse_env_duration_value(name, std::env::var(name), default_seconds)
}

fn parse_env_duration_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default_seconds: u64,
) -> Result<Duration, Error> {
    let seconds = parse_env_u64_value(name, value, default_seconds)?;
    if seconds == 0 {
        return Err(Error::Config(format!("{name} must be greater than 0")));
    }
    Ok(Duration::from_secs(seconds))
}

fn parse_env_optional_duration(name: &str, default_seconds: u64) -> Result<Option<Duration>, Error> {
    parse_env_optional_duration_value(name, std::env::var(name), default_seconds)
}

fn parse_env_optional_duration_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default_seconds: u64,
) -> Result<Option<Duration>, Error> {
    let seconds = parse_env_u64_value(name, value, default_seconds)?;
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

fn parse_env_temp_store() -> Result<SqliteTempStore, Error> {
    parse_env_temp_store_value(std::env::var("SQLITE_TEMP_STORE"))
}

fn parse_env_temp_store_value(value: Result<String, std::env::VarError>) -> Result<SqliteTempStore, Error> {
    match value {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "default" | "0" => Ok(SqliteTempStore::Default),
            "file" | "1" => Ok(SqliteTempStore::File),
            "memory" | "2" => Ok(SqliteTempStore::Memory),
            _ => Err(Error::Config(
                "SQLITE_TEMP_STORE must be one of default, file, memory, 0, 1, or 2".to_owned(),
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(SqliteTempStore::default()),
        Err(e) => Err(Error::Config(format!("failed to read SQLITE_TEMP_STORE: {e}"))),
    }
}

fn sqlite_config_from_env() -> Result<SqliteConfig, Error> {
    Ok(SqliteConfig {
        max_connections: parse_env_u32("SQLITE_MAX_CONNECTIONS", DEFAULT_SQLITE_MAX_CONNECTIONS)?,
        journal_size_limit_bytes: parse_env_u64(
            "SQLITE_JOURNAL_SIZE_LIMIT_BYTES",
            DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES,
        )?,
        temp_store: parse_env_temp_store()?,
        mmap_size_bytes: parse_env_u64("SQLITE_MMAP_SIZE_BYTES", DEFAULT_SQLITE_MMAP_SIZE_BYTES)?,
    })
}

fn postgres_config_from_env() -> Result<PostgresConfig, Error> {
    Ok(PostgresConfig {
        max_connections: parse_env_u32("POSTGRES_MAX_CONNECTIONS", DEFAULT_POSTGRES_MAX_CONNECTIONS)?,
        acquire_timeout: parse_env_duration(
            "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
        )?,
        lock_timeout: parse_env_duration("POSTGRES_LOCK_TIMEOUT_SECONDS", DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS)?,
        migration_timeout: parse_env_duration(
            "POSTGRES_MIGRATION_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS,
        )?,
        statement_timeout: parse_env_duration(
            "POSTGRES_STATEMENT_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
        )?,
        idle_timeout: parse_env_optional_duration(
            "POSTGRES_IDLE_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
        )?,
        max_lifetime: parse_env_optional_duration(
            "POSTGRES_MAX_LIFETIME_SECONDS",
            DEFAULT_POSTGRES_MAX_LIFETIME_SECONDS,
        )?,
    })
}

fn database_configs_from_env(database_url: &str) -> Result<(PostgresConfig, SqliteConfig), Error> {
    let backend = DatabaseBackend::from_url(database_url)
        .map_err(|error| Error::Config(format!("invalid DATABASE_URL: {error}")))?;
    match backend {
        DatabaseBackend::Postgres => Ok((postgres_config_from_env()?, SqliteConfig::default())),
        DatabaseBackend::Sqlite => Ok((PostgresConfig::default(), sqlite_config_from_env()?)),
        DatabaseBackend::Other => Ok((PostgresConfig::default(), SqliteConfig::default())),
    }
}

fn build_config(llm_api_base: String, common: &CommonArgs, file: &FileConfig) -> Result<Config, Error> {
    let db_url = common
        .db_url
        .clone()
        .or_else(|| file.database_url.clone())
        .map_or_else(default_database_url, Ok)?;
    let (postgres, sqlite) = database_configs_from_env(&db_url)?;
    let web_search = resolve_web_search_config(&file.web_search, environment_value)?;
    let mcp_allowed_hosts = environment_value("AGENTIC_MCP_ALLOWED_HOSTS")
        .map_or_else(|| file.mcp.allowed_hosts.clone(), |value| parse_comma_separated(&value));
    let max_concurrent_gateway_calls_default = file
        .tools
        .max_concurrent_gateway_calls
        .unwrap_or(DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS);
    let max_concurrent_gateway_calls = parse_env_nonzero_usize(
        "AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS",
        max_concurrent_gateway_calls_default,
    )?;
    Ok(Config {
        llm_api_base,
        openai_api_key: common.openai_api_key.clone(),
        llm_ready_timeout_s: common.llm_ready_timeout_s,
        llm_ready_interval_s: common.llm_ready_interval_s,
        skip_llm_ready_check: common.skip_llm_ready_check,
        db_url: Some(db_url),
        postgres,
        sqlite,
        tools: ToolRuntimeConfig {
            web_search,
            mcp_servers: file.mcp_servers.clone(),
            mcp_allowed_hosts,
            messages_gateway_tool_aliases: file.messages_gateway.tool_aliases.clone(),
            max_concurrent_gateway_calls,
        },
    })
}

/// Resolves the `web_search` provider settings as environment variable >
/// configuration file > provider default.
///
/// The provider comes from `AGENTIC_WEB_SEARCH_PROVIDER` or `[web_search]
/// provider`. The API key is read from the variable named by `api_key_env`,
/// else the provider's conventional variable. The endpoint prefers
/// `AGENTIC_WEB_SEARCH_BASE_URL`, then `YOU_API_BASE_URL` (You.com only), then
/// the file, then the provider default. `max_concurrent_queries` is left unset
/// so the provider's own default applies.
fn resolve_web_search_config(
    file: &WebSearchFileConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<WebSearchProviderConfig, Error> {
    let provider = match env(WEB_SEARCH_PROVIDER_ENV) {
        Some(value) => value
            .parse::<WebSearchProviderKind>()
            .map_err(|error| Error::Config(format!("{WEB_SEARCH_PROVIDER_ENV}: {error}")))?,
        None => file.provider.unwrap_or_default(),
    };
    let api_key_env = file
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| provider.default_api_key_env());
    let api_key = env(api_key_env);
    let base_url = env(WEB_SEARCH_BASE_URL_ENV)
        .or_else(|| provider.is_you().then(|| env(YOU_API_BASE_URL_ENV)).flatten())
        .or_else(|| file.base_url.clone())
        .or_else(|| provider.default_base_url().map(str::to_owned));
    let max_concurrent_queries = match env(WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV) {
        Some(value) => Some(value.parse::<NonZeroUsize>().map_err(|error| {
            Error::Config(format!(
                "{WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV} must be a positive integer: {error}"
            ))
        })?),
        None => file.max_concurrent_queries,
    };
    Ok(WebSearchProviderConfig::new(api_key, base_url)
        .with_provider(provider)
        .with_max_concurrent_queries(max_concurrent_queries))
}

fn gateway_options<'a>(
    common: &'a CommonArgs,
    file: &FileConfig,
    oidc: Option<OidcConfig>,
) -> Result<GatewayOptions<'a>, Error> {
    Ok(GatewayOptions {
        host: &common.gateway_host,
        port: common.gateway_port,
        max_request_body_size: resolve_max_request_body_size(
            common.max_request_body_size_bytes,
            file.server.max_request_body_size_bytes,
        )?,
        oidc,
    })
}

fn generated_file_config(llm_api_base: String) -> FileConfig {
    FileConfig {
        llm_api_base: Some(llm_api_base),
        web_search: generated_web_search_file_config(environment_value),
        mcp: McpFileConfig {
            allowed_hosts: environment_value("AGENTIC_MCP_ALLOWED_HOSTS")
                .map_or_else(Vec::new, |value| parse_comma_separated(&value)),
        },
        server: ServerFileConfig {
            max_request_body_size_bytes: environment_value(MAX_REQUEST_BODY_SIZE_ENV)
                .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        },
        tools: ToolsFileConfig {
            max_concurrent_gateway_calls: environment_value("AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS")
                .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        },
        messages_gateway: MessagesGatewayFileConfig {
            tool_aliases: environment_value("MESSAGES_GATEWAY_TOOL_ALIASES"),
        },
        mcp_servers: HashMap::new(),
        ..FileConfig::default()
    }
}

/// Seeds `[web_search]` in a generated configuration file from the current
/// environment. The selected provider and its conventional key variable are
/// written so the file documents which variable the gateway reads; a malformed
/// provider value is ignored here and rejected at startup by
/// [`resolve_web_search_config`].
fn generated_web_search_file_config(env: impl Fn(&str) -> Option<String>) -> WebSearchFileConfig {
    let provider = env(WEB_SEARCH_PROVIDER_ENV)
        .and_then(|value| value.parse::<WebSearchProviderKind>().ok())
        .unwrap_or_default();
    WebSearchFileConfig {
        provider: Some(provider),
        base_url: env(WEB_SEARCH_BASE_URL_ENV)
            .or_else(|| provider.is_you().then(|| env(YOU_API_BASE_URL_ENV)).flatten()),
        api_key_env: Some(provider.default_api_key_env().to_owned()),
        max_concurrent_queries: env(WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV)
            .and_then(|value| value.parse::<NonZeroUsize>().ok()),
    }
}

fn environment_value(name: &str) -> Option<String> {
    clean_value(std::env::var(name).ok())
}

fn clean_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_comma_separated(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), server::ServerError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentic_server=info,agentic_core=info".parse().expect("valid filter")),
        )
        .init();

    let Cli {
        command,
        llm_api_base,
        common,
    } = Cli::parse();
    let agentic_home = ensure_agentic_api_home()?;
    let loaded_file_config = FileConfig::load(&agentic_home)?;
    let config_file_missing = loaded_file_config.is_none();
    let mut file_config = loaded_file_config.unwrap_or_default();
    let oidc_config = oidc_config_from_values(common.oidc_issuer.as_deref(), common.oidc_audience.as_deref())?;

    match command {
        None => {
            let base = llm_api_base
                .or_else(|| file_config.llm_api_base.clone())
                .ok_or_else(|| {
                Error::Config(
                    "standalone mode requires llm_api_base in config.toml, LLM_API_BASE, or --llm-api-base; use `agentic-server serve <model>` for integrated mode"
                        .to_owned(),
                )
            })?;
            if config_file_missing {
                file_config = generated_file_config(base.clone()).create_or_load(&agentic_home)?;
            }
            let config = build_config(normalize_base_url(&base), &common, &file_config)?;
            let gateway = gateway_options(&common, &file_config, oidc_config)?;
            server::run(config, gateway).await
        }
        Some(Commands::Serve { model, port, llm_args }) => {
            if llm_api_base.is_some() {
                return Err(Error::Config(
                    "--llm-api-base is only valid in standalone mode; remove it when using `serve`".to_owned(),
                )
                .into());
            }
            if config_file_missing {
                file_config =
                    generated_file_config(format!("http://127.0.0.1:{port}")).create_or_load(&agentic_home)?;
            }
            let config = build_config(
                normalize_base_url(&format!("http://127.0.0.1:{port}")),
                &common,
                &file_config,
            )?;
            let mut args = vec!["--model".to_owned(), model];
            args.push("--port".to_owned());
            args.push(port.to_string());
            args.extend(llm_args);
            let gateway = gateway_options(&common, &file_config, oidc_config)?;
            server::run_with_llm(config, gateway, args).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use clap::{CommandFactory, Parser};

    use super::{
        Cli, Commands, WebSearchFileConfig, database_configs_from_env, generated_web_search_file_config,
        oidc_config_from_values, parse_env_duration_value, parse_env_nonzero_usize_value,
        parse_env_optional_duration_value, parse_env_temp_store_value, parse_env_u32_value, parse_env_u64_value,
        resolve_max_request_body_size_value, resolve_web_search_config,
    };
    use agentic_core::config::{
        DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS, DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
        DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS, DEFAULT_POSTGRES_MAX_CONNECTIONS,
        DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS, DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
        DEFAULT_SQLITE_MAX_CONNECTIONS, SqliteTempStore, WebSearchProviderKind,
    };
    use agentic_server::app::DEFAULT_MAX_REQUEST_BODY_SIZE;

    /// Environment lookup over a fixed set of variables, mirroring `environment_value`.
    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        }
    }

    #[test]
    fn web_search_config_defaults_to_you_with_legacy_variables() {
        let file = WebSearchFileConfig {
            base_url: Some("https://file.example".to_owned()),
            ..WebSearchFileConfig::default()
        };
        let config = resolve_web_search_config(
            &file,
            env_from(&[
                ("YOU_API_KEY", "you-secret"),
                ("YOU_API_BASE_URL", "https://you.example"),
            ]),
        )
        .expect("resolve you.com");
        assert_eq!(config.provider, WebSearchProviderKind::You);
        assert_eq!(config.api_key.as_deref(), Some("you-secret"));
        assert_eq!(config.base_url.as_deref(), Some("https://you.example"));
        assert_eq!(config.max_concurrent_queries, None);

        // Without any endpoint variable the file value is used; You.com has no default.
        let config = resolve_web_search_config(&file, env_from(&[])).expect("resolve from file");
        assert_eq!(config.base_url.as_deref(), Some("https://file.example"));
        assert_eq!(config.api_key, None);
        let config = resolve_web_search_config(&WebSearchFileConfig::default(), env_from(&[])).expect("resolve empty");
        assert_eq!(config.base_url, None);
    }

    #[test]
    fn web_search_config_selects_brave_from_environment_or_file() {
        let config = resolve_web_search_config(
            &WebSearchFileConfig::default(),
            env_from(&[
                ("AGENTIC_WEB_SEARCH_PROVIDER", "Brave"),
                ("BRAVE_API_KEY", "brave-secret"),
                ("YOU_API_KEY", "you-secret"),
                ("YOU_API_BASE_URL", "https://you.example"),
            ]),
        )
        .expect("resolve brave");
        assert_eq!(config.provider, WebSearchProviderKind::Brave);
        assert_eq!(config.api_key.as_deref(), Some("brave-secret"));
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://api.search.brave.com"),
            "YOU_API_BASE_URL must not leak into the Brave endpoint"
        );

        let file = WebSearchFileConfig {
            provider: Some(WebSearchProviderKind::Brave),
            api_key_env: Some("MY_BRAVE_KEY".to_owned()),
            max_concurrent_queries: NonZeroUsize::new(2),
            ..WebSearchFileConfig::default()
        };
        let config = resolve_web_search_config(&file, env_from(&[("MY_BRAVE_KEY", "custom-secret")]))
            .expect("resolve brave from file");
        assert_eq!(config.provider, WebSearchProviderKind::Brave);
        assert_eq!(config.api_key.as_deref(), Some("custom-secret"));
        assert_eq!(config.max_concurrent_queries.map(NonZeroUsize::get), Some(2));

        // The environment variable wins over the file for the provider itself.
        let config = resolve_web_search_config(&file, env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", "you")]))
            .expect("resolve override");
        assert_eq!(config.provider, WebSearchProviderKind::You);
    }

    #[test]
    fn web_search_config_applies_environment_precedence_for_endpoint_and_concurrency() {
        let file = WebSearchFileConfig {
            base_url: Some("https://file.example".to_owned()),
            max_concurrent_queries: NonZeroUsize::new(2),
            ..WebSearchFileConfig::default()
        };
        let config = resolve_web_search_config(
            &file,
            env_from(&[
                ("AGENTIC_WEB_SEARCH_BASE_URL", "https://generic.example"),
                ("YOU_API_BASE_URL", "https://legacy.example"),
                ("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES", "4"),
            ]),
        )
        .expect("resolve overrides");
        assert_eq!(config.base_url.as_deref(), Some("https://generic.example"));
        assert_eq!(config.max_concurrent_queries.map(NonZeroUsize::get), Some(4));

        let config = resolve_web_search_config(&file, env_from(&[("YOU_API_BASE_URL", "https://legacy.example")]))
            .expect("legacy endpoint");
        assert_eq!(config.base_url.as_deref(), Some("https://legacy.example"));
        assert_eq!(config.max_concurrent_queries.map(NonZeroUsize::get), Some(2));
    }

    #[test]
    fn web_search_config_rejects_invalid_environment_values() {
        let error = resolve_web_search_config(
            &WebSearchFileConfig::default(),
            env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", "bing")]),
        )
        .expect_err("unknown provider");
        assert_eq!(
            error.to_string(),
            "AGENTIC_WEB_SEARCH_PROVIDER: unknown web_search provider \"bing\"; expected one of: you, brave"
        );

        let error = resolve_web_search_config(
            &WebSearchFileConfig::default(),
            env_from(&[("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES", "0")]),
        )
        .expect_err("zero concurrency");
        assert!(
            error
                .to_string()
                .starts_with("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES must be a positive integer"),
            "{error}"
        );
    }

    #[test]
    fn generated_web_search_config_documents_the_selected_provider() {
        let generated = generated_web_search_file_config(env_from(&[("YOU_API_BASE_URL", "https://you.example")]));
        assert_eq!(generated.provider, Some(WebSearchProviderKind::You));
        assert_eq!(generated.api_key_env.as_deref(), Some("YOU_API_KEY"));
        assert_eq!(generated.base_url.as_deref(), Some("https://you.example"));
        assert_eq!(generated.max_concurrent_queries, None);

        let generated = generated_web_search_file_config(env_from(&[
            ("AGENTIC_WEB_SEARCH_PROVIDER", "brave"),
            ("YOU_API_BASE_URL", "https://you.example"),
            ("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES", "3"),
        ]));
        assert_eq!(generated.provider, Some(WebSearchProviderKind::Brave));
        assert_eq!(generated.api_key_env.as_deref(), Some("BRAVE_API_KEY"));
        assert_eq!(
            generated.base_url, None,
            "the Brave default endpoint is not pinned into the file"
        );
        assert_eq!(generated.max_concurrent_queries.map(NonZeroUsize::get), Some(3));

        let generated = generated_web_search_file_config(env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", "bing")]));
        assert_eq!(generated.provider, Some(WebSearchProviderKind::You));
    }

    #[test]
    fn serve_uses_common_args_before_subcommand() {
        let cli = Cli::parse_from(["agentic-server", "--llm-ready-timeout-s", "0.1", "serve", "model-a"]);
        assert!((cli.common.llm_ready_timeout_s - 0.1).abs() < f64::EPSILON);
        assert!(matches!(cli.command, Some(Commands::Serve { .. })));
    }

    #[test]
    fn serve_uses_common_args_after_subcommand() {
        let cli = Cli::parse_from(["agentic-server", "serve", "--llm-ready-timeout-s", "0.1", "model-a"]);
        assert!((cli.common.llm_ready_timeout_s - 0.1).abs() < f64::EPSILON);
        assert!(matches!(cli.command, Some(Commands::Serve { .. })));
    }

    #[test]
    fn skip_llm_ready_check_can_be_set_from_cli() {
        let cli = Cli::parse_from([
            "agentic-server",
            "--llm-api-base",
            "http://localhost:8000",
            "--skip-llm-ready-check",
        ]);
        assert!(cli.common.skip_llm_ready_check);
    }

    #[test]
    fn standalone_base_url_uses_llm_api_base_flag() {
        let cli = Cli::parse_from(["agentic-server", "--llm-api-base", "http://localhost:8000"]);

        assert_eq!(cli.llm_api_base.as_deref(), Some("http://localhost:8000"));
    }

    #[test]
    fn oidc_configuration_requires_issuer_and_audience_together() {
        assert!(oidc_config_from_values(None, None).expect("disabled OIDC").is_none());
        assert!(oidc_config_from_values(Some("https://issuer.example"), None).is_err());
        assert!(oidc_config_from_values(None, Some("agentic-api")).is_err());
        assert!(
            oidc_config_from_values(Some("https://issuer.example"), Some("agentic-api"))
                .expect("complete OIDC configuration")
                .is_some()
        );
    }

    #[test]
    fn container_runtime_options_are_bound_to_environment_variables() {
        let command = Cli::command();

        for (argument, expected_env) in [
            ("llm_api_base", "LLM_API_BASE"),
            ("gateway_host", "GATEWAY_HOST"),
            ("gateway_port", "GATEWAY_PORT"),
            ("oidc_issuer", "OIDC_ISSUER"),
            ("oidc_audience", "OIDC_AUDIENCE"),
        ] {
            let env = command
                .get_arguments()
                .find(|arg| arg.get_id() == argument)
                .and_then(clap::Arg::get_env)
                .unwrap_or_else(|| panic!("{argument} must be configurable through {expected_env}"));

            assert_eq!(env, expected_env);
        }
    }

    #[test]
    fn sqlite_tuning_is_env_only_not_cli() {
        let mut help = Vec::new();
        Cli::command().write_long_help(&mut help).expect("render help");
        let help = String::from_utf8(help).expect("help is utf8");

        assert!(!help.contains("--sqlite-journal-size-limit-bytes"));
        assert!(!help.contains("--sqlite-max-connections"));
        assert!(!help.contains("--sqlite-temp-store"));
        assert!(!help.contains("--sqlite-mmap-size-bytes"));

        assert!(
            Cli::try_parse_from([
                "agentic-server",
                "--llm-api-base",
                "http://localhost:8000",
                "--sqlite-temp-store",
                "memory",
            ])
            .is_err()
        );
    }

    #[test]
    fn sqlite_tuning_env_parser_uses_defaults_and_rejects_invalid_values() {
        assert_eq!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .expect("default value"),
            DEFAULT_SQLITE_MAX_CONNECTIONS
        );
        assert_eq!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Ok("6".to_owned()),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .expect("parsed value"),
            6
        );
        assert!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Ok("0".to_owned()),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .is_err()
        );
        assert!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Ok("not-a-number".to_owned()),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .is_err()
        );

        assert_eq!(
            parse_env_u64_value("SQLITE_MMAP_SIZE_BYTES", Err(std::env::VarError::NotPresent), 1_024)
                .expect("default value"),
            1_024
        );
        assert_eq!(
            parse_env_u64_value("SQLITE_MMAP_SIZE_BYTES", Ok("4096".to_owned()), 1_024).expect("parsed value"),
            4_096
        );
        assert!(parse_env_u64_value("SQLITE_MMAP_SIZE_BYTES", Ok("not-a-number".to_owned()), 1_024).is_err());

        assert_eq!(
            parse_env_temp_store_value(Err(std::env::VarError::NotPresent)).expect("default temp store"),
            SqliteTempStore::Memory
        );
        assert_eq!(
            parse_env_temp_store_value(Ok("file".to_owned())).expect("file temp store"),
            SqliteTempStore::File
        );
        assert_eq!(
            parse_env_temp_store_value(Ok("2".to_owned())).expect("memory temp store"),
            SqliteTempStore::Memory
        );
        assert!(parse_env_temp_store_value(Ok("invalid".to_owned())).is_err());
    }

    #[test]
    fn gateway_concurrency_env_parser_requires_a_nonzero_value() {
        let default = agentic_core::config::DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS;
        assert_eq!(
            parse_env_nonzero_usize_value(
                "AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS",
                Err(std::env::VarError::NotPresent),
                default,
            )
            .expect("default value"),
            default
        );
        assert_eq!(
            parse_env_nonzero_usize_value("AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS", Ok("3".to_owned()), default,)
                .expect("positive value")
                .get(),
            3
        );
        assert!(
            parse_env_nonzero_usize_value("AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS", Ok("0".to_owned()), default,)
                .is_err()
        );
    }

    #[test]
    fn postgres_timeout_parser_uses_defaults_and_allows_disabling_recycling() {
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
            )
            .expect("default acquire timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS)
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
                Ok("9".to_owned()),
                DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
            )
            .expect("explicit acquire timeout"),
            Duration::from_secs(9)
        );
        assert!(
            parse_env_duration_value(
                "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
                Ok("0".to_owned()),
                DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
            )
            .is_err()
        );
        assert_eq!(
            parse_env_optional_duration_value(
                "POSTGRES_IDLE_TIMEOUT_SECONDS",
                Ok("0".to_owned()),
                DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
            )
            .expect("disabled idle timeout"),
            None
        );
        assert_eq!(
            parse_env_optional_duration_value(
                "POSTGRES_IDLE_TIMEOUT_SECONDS",
                Ok("45".to_owned()),
                DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
            )
            .expect("explicit idle timeout"),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_LOCK_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS,
            )
            .expect("default lock timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS)
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_MIGRATION_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS,
            )
            .expect("default migration timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS)
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_STATEMENT_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
            )
            .expect("default statement timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS)
        );
        assert!(
            parse_env_u32_value(
                "POSTGRES_MAX_CONNECTIONS",
                Ok("0".to_owned()),
                DEFAULT_POSTGRES_MAX_CONNECTIONS,
            )
            .is_err()
        );
    }

    #[test]
    fn max_request_body_size_is_configurable_from_cli_env_and_file() {
        let cli = NonZeroUsize::new(4_096);
        let file = NonZeroUsize::new(2_048);
        let missing = || Err(std::env::VarError::NotPresent);

        assert_eq!(
            resolve_max_request_body_size_value(None, None, missing()).expect("default value"),
            DEFAULT_MAX_REQUEST_BODY_SIZE
        );
        assert_eq!(
            resolve_max_request_body_size_value(None, file, missing()).expect("file value"),
            file.expect("nonzero")
        );
        assert_eq!(
            resolve_max_request_body_size_value(None, file, Ok("8192".to_owned()))
                .expect("environment overrides the file")
                .get(),
            8_192
        );
        assert_eq!(
            resolve_max_request_body_size_value(cli, file, Ok("8192".to_owned()))
                .expect("CLI overrides the environment")
                .get(),
            4_096
        );
    }

    #[test]
    fn max_request_body_size_rejects_invalid_environment_overrides() {
        let cli = NonZeroUsize::new(4_096);

        assert!(resolve_max_request_body_size_value(None, None, Ok("0".to_owned())).is_err());
        assert!(resolve_max_request_body_size_value(None, None, Ok("-1".to_owned())).is_err());
        assert!(resolve_max_request_body_size_value(None, None, Ok("not-a-number".to_owned())).is_err());

        // An explicit CLI argument outranks the environment, so a stale or malformed
        // inherited value cannot block startup.
        assert_eq!(
            resolve_max_request_body_size_value(cli, None, Ok("0".to_owned()))
                .expect("CLI argument overrides a malformed environment value")
                .get(),
            4_096
        );
        assert_eq!(
            resolve_max_request_body_size_value(cli, None, Ok("not-a-number".to_owned()))
                .expect("CLI argument overrides an unparsable environment value")
                .get(),
            4_096
        );
    }

    #[test]
    fn max_request_body_size_argument_is_global() {
        let before = Cli::parse_from([
            "agentic-server",
            "--max-request-body-size-bytes",
            "4096",
            "serve",
            "model-a",
        ]);
        let after = Cli::parse_from([
            "agentic-server",
            "serve",
            "model-a",
            "--max-request-body-size-bytes",
            "4096",
        ]);

        assert_eq!(
            before.common.max_request_body_size_bytes.map(NonZeroUsize::get),
            Some(4_096)
        );
        assert_eq!(
            after.common.max_request_body_size_bytes.map(NonZeroUsize::get),
            Some(4_096)
        );
        assert!(
            Cli::try_parse_from([
                "agentic-server",
                "--llm-api-base",
                "http://localhost:8000",
                "--max-request-body-size-bytes",
                "0",
            ])
            .is_err()
        );
    }

    #[test]
    fn database_config_rejects_an_invalid_url() {
        let error = database_configs_from_env("not a database URL").expect_err("invalid URL must be rejected");
        assert!(error.to_string().contains("invalid DATABASE_URL"));
    }
}
