/*!
 * Library interfaces for this crate, intended for use only by the automated
 * test suite.  This crate does not define a Rust library API that's intended to
 * be consumed from the outside.
 *
 * TODO-cleanup is there a better way to do this?
 */

mod api_config;
mod api_error;
mod api_http_entrypoints;
mod rack;
pub mod api_model;
mod datastore;

pub use api_config::ApiServerConfig;
use rack::OxideRack;
use api_model::ApiIdentityMetadataCreateParams;
use api_model::ApiName;
use api_model::ApiProjectCreateParams;
use dropshot::ApiDescription;
use dropshot::RequestContext;
use std::convert::TryFrom;
use std::sync::Arc;
use uuid::Uuid;

#[macro_use]
extern crate slog;

/**
 * Returns a Dropshot `ApiDescription` for our API.
 */
pub fn dropshot_api() -> ApiDescription {
    let mut api = ApiDescription::new();
    api_http_entrypoints::api_register_entrypoints(&mut api);
    api
}

/**
 * Run the OpenAPI generator, which emits the OpenAPI spec to stdout.
 */
pub fn run_openapi() {
    dropshot_api().print_openapi();
}

/**
 * Run an instance of the API server.
 */
pub async fn run_server(config: &ApiServerConfig) -> Result<(), String> {
    let log = config
        .log
        .to_logger("oxide-api")
        .map_err(|message| format!("initializing logger: {}", message))?;
    info!(log, "starting server");

    let apictx = ApiContext::new();

    populate_initial_data(&apictx).await;

    let mut http_server = dropshot::HttpServer::new(
        &config.dropshot,
        dropshot_api(),
        apictx,
        &log,
    )
    .map_err(|error| format!("initializing server: {}", error))?;

    let join_handle = http_server.run().await;
    let server_result = join_handle
        .map_err(|error| format!("waiting for server: {}", error))?;
    server_result.map_err(|error| format!("server stopped: {}", error))
}

/**
 * API-specific state that we'll associate with the server and make available to
 * API request handler functions.
 */
pub struct ApiContext {
    pub rack: Arc<OxideRack>,
}

impl ApiContext {
    pub fn new() -> Arc<ApiContext> {
        Arc::new(ApiContext {
            rack: Arc::new(OxideRack::new()),
        })
    }

    /**
     * Retrieves our API-specific context out of the generic RequestContext
     * structure.  It should not be possible for this downcast to fail unless
     * the caller has passed us a RequestContext from a totally different
     * HttpServer created with a different type for its private data.  This
     * should not happen in practice.
     * TODO-cleanup: can we make this API statically type-safe?
     */
    fn from_request(rqctx: &Arc<RequestContext>) -> Arc<ApiContext> {
        let maybectx = Arc::clone(&rqctx.server.private);
        maybectx
            .downcast::<ApiContext>()
            .expect("ApiContext: wrong type for private data")
    }
}

/*
 * This is a one-off for prepopulating some useful data in a freshly-started
 * server.  This should be replaced with a config file or a data backend with a
 * demo initialization script or the like.
 */
pub async fn populate_initial_data(apictx: &Arc<ApiContext>) {
    let rack = &apictx.rack;
    let demo_projects: Vec<(&str, &str)> = vec![
        ("1eb2b543-b199-405f-b705-1739d01a197c", "simproject1"),
        ("4f57c123-3bda-4fae-94a2-46a9632d40b6", "simproject2"),
        ("4aac89b0-df9a-441d-b050-f953476ea290", "simproject3"),
    ];

    for (new_uuid, new_name) in demo_projects {
        let name_validated = ApiName::try_from(new_name).unwrap();
        rack.project_create_with_id(
            Uuid::parse_str(new_uuid).unwrap(),
            &ApiProjectCreateParams {
                identity: ApiIdentityMetadataCreateParams {
                    name: name_validated,
                    description: "<auto-generated at server startup>"
                        .to_string(),
                },
            },
        )
        .await
        .unwrap();
    }
}
