//! OpenAPI document assembly.
//!
//! The `ApiDoc` derive pulls the `#[utoipa::path]` annotations from every
//! handler and the `ToSchema` derives from the DTO modules into one OpenAPI
//! document, served raw at `/api-docs/openapi.json` and rendered by the
//! embedded Swagger UI at `/swagger-ui`. `SecurityAddon` registers the two
//! auth schemes (Basic for admin routes, Bearer for proxy routes) so the UI
//! can drive its Authorize dialog; the schemes document how to call the API,
//! they do not add any enforcement, which stays with the extractors.

use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::RefOr;
use utoipa::{Modify, OpenApi, ToSchema};

/// Plain error body returned by every admin route: `{"error": "..."}`.
/// Proxy routes use the OpenAI shape ([`OpenAiError`]) instead.
#[derive(Serialize, ToSchema)]
pub struct AdminErrorBody {
    /// Human-readable description of what failed.
    pub error: String,
}

/// Error body used by every `/v1/*` route, in the OpenAI shape so existing
/// client libraries can parse it.
#[derive(Serialize, ToSchema)]
pub struct OpenAiError {
    pub error: OpenAiErrorDetail,
}

/// The `error` object inside an [`OpenAiError`] body.
#[derive(Serialize, ToSchema)]
pub struct OpenAiErrorDetail {
    /// What went wrong, phrased for display to a client.
    pub message: String,
    /// OpenAI error family, `invalid_request_error` or `api_error`.
    #[schema(example = "invalid_request_error")]
    pub r#type: String,
    /// Machine-readable code, `null` where OpenAI sends no code.
    #[schema(example = "invalid_api_key")]
    pub code: Option<String>,
}

/// Registers the security schemes referenced by the per-operation
/// `security(...)` annotations in the handler modules.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.security_schemes.insert(
            "basic_auth".to_string(),
            RefOr::T(SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Basic)
                    .description(Some(
                        "Admin credentials: the values of HYDRA_ADMIN_LOGIN and \
                         HYDRA_ADMIN_PASSWORD, compared in constant time.",
                    ))
                    .build(),
            )),
        );
        components.security_schemes.insert(
            "bearer_auth".to_string(),
            RefOr::T(SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("hyd- key")
                    .description(Some(
                        "A Hydra client key created via POST /admin/keys; the \
                         plaintext is shown only once, in that creation response.",
                    ))
                    .build(),
            )),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hydra",
        version = "0.1.0",
        description = "Self-hosted, OpenAI-compatible API router. An admin \
            configures upstream channels (base URL, upstream key, optional \
            model whitelist) and issues `hyd-` client keys; clients then call \
            the OpenAI-compatible /v1 endpoints while the upstream secret \
            never leaves the server."
    ),
    paths(
        super::health,
        super::admin_channels::create,
        super::admin_channels::list,
        super::admin_channels::get_one,
        super::admin_channels::patch,
        super::admin_channels::delete,
        super::admin_keys::create,
        super::admin_keys::list,
        super::admin_keys::patch,
        super::admin_keys::delete,
        super::admin_logs::list,
        super::admin_logs::get_one,
        super::proxy::chat_completions,
        super::proxy::models
    ),
    components(
        schemas(
            AdminErrorBody,
            OpenAiError,
            OpenAiErrorDetail,
            super::admin_channels::CreateChannelRequest,
            super::admin_channels::PatchChannelRequest,
            super::admin_channels::ChannelView,
            super::admin_channels::ModelPriceView,
            super::admin_keys::CreateKeyRequest,
            super::admin_keys::PatchKeyRequest,
            super::admin_keys::KeyView,
            super::admin_keys::CreatedKeyView,
            super::admin_logs::RequestLogView,
            super::admin_logs::LogListResponse,
            super::admin_logs::RequestLogDetailView,
            super::proxy::ModelList,
            super::proxy::ModelEntry
        )
    ),
    tags(
        (name = "Admin", description = "Router management over HTTP Basic \
            Auth: channels, client keys, request logs. GET /admin/health is \
            public."),
        (name = "Proxy", description = "OpenAI-compatible endpoints for \
            clients calling with a `hyd-` key: /v1/chat/completions and \
            /v1/models.")
    ),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use sqlx::sqlite::SqlitePoolOptions;
    use tower::ServiceExt;

    use crate::http::{router, AppState};

    async fn test_state() -> AppState {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        AppState {
            db: pool.into(),
            admin_login: "admin".to_string(),
            admin_password: "secret".to_string(),
            client: crate::http::upstream_client(),
            log_bodies: false,
            log_tracker: crate::logging::LogTracker::default(),
            cache: crate::cache::ResolutionCache::default(),
            trusted_proxies: crate::config::TrustedProxies::default(),
            flood: crate::flood::FloodCap::default(),
        }
    }

    #[tokio::test]
    async fn swagger_ui_serves_the_embedded_html_page() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/swagger-ui/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response.headers()[axum::http::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .to_string();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        let html = String::from_utf8(bytes).unwrap();
        assert!(
            content_type.starts_with("text/html"),
            "expected an HTML page, got {content_type}"
        );
        assert!(
            html.contains("swagger-ui"),
            "expected swagger-ui markers in the served page"
        );
    }

    #[tokio::test]
    async fn openapi_json_documentes_every_route_with_examples_and_schemes() {
        let state = test_state().await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api-docs/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let paths = doc["paths"].as_object().unwrap();
        for path in [
            "/admin/health",
            "/admin/channels",
            "/admin/channels/{id}",
            "/admin/keys",
            "/admin/keys/{id}",
            "/admin/logs",
            "/admin/logs/{id}",
            "/v1/chat/completions",
            "/v1/models",
        ] {
            assert!(
                paths.contains_key(path),
                "openapi document is missing path {path}"
            );
        }

        // Spot-checks that annotations carried examples and security along.
        let chat = &doc["paths"]["/v1/chat/completions"]["post"];
        assert!(
            chat["responses"]["413"].is_object(),
            "413 must be documented"
        );
        assert!(
            chat["security"][0]["bearer_auth"].is_array(),
            "chat must require the bearer scheme"
        );

        // The chat request example must relate its model to the channel
        // whitelist, so executing the example against a channel that does
        // not admit the example model is a documented 404, not a surprise.
        let request_body = &chat["requestBody"];
        let description = request_body["description"].as_str().unwrap();
        assert!(
            description.contains("whitelist"),
            "the request body must document the whitelist relation: {description}"
        );
        assert!(
            description.contains("/v1/models"),
            "the request body must point to the model listing: {description}"
        );
        let examples = &request_body["content"]["application/json"]["examples"];
        for name in ["plain", "streaming"] {
            assert!(
                examples[name]["value"]["model"].as_str().is_some(),
                "the {name} example must carry a string model"
            );
            assert!(
                examples[name]["value"]["messages"][0]["role"] == "user",
                "the {name} example must show a well-shaped message"
            );
        }
        let not_found = chat["responses"]["404"]["content"]["application/json"]["example"]["error"]
            ["message"]
            .as_str()
            .unwrap();
        assert!(
            not_found.contains("Allowed models for this channel"),
            "the 404 example must show the allowed-models hint: {not_found}"
        );
        let create_key = &doc["paths"]["/admin/keys"]["post"];
        assert!(
            create_key["responses"]["201"]["content"]["application/json"]["example"]["key"]
                .as_str()
                .unwrap()
                .starts_with("hyd-"),
            "the 201 example must show a full hyd- key"
        );
        assert!(
            create_key["security"][0]["basic_auth"].is_array(),
            "admin routes must require the basic scheme"
        );

        // The monthly limit must be documentable from the UI: the request
        // schemas carry the USD input field, the response schemas carry both
        // the canonical micro-USD integer and the rendered USD string, and
        // at least one example shows the field in use.
        let schemas = &doc["components"]["schemas"];
        for (schema, field) in [
            ("CreateKeyRequest", "monthly_limit_usd"),
            ("PatchKeyRequest", "monthly_limit_usd"),
            ("KeyView", "monthly_limit_micro_usd"),
            ("KeyView", "monthly_limit_usd"),
            ("CreatedKeyView", "monthly_limit_micro_usd"),
            ("CreatedKeyView", "monthly_limit_usd"),
        ] {
            assert!(
                schemas[schema]["properties"][field].is_object(),
                "{schema}.{field} must be in the OpenAPI document"
            );
        }
        assert_eq!(
            create_key["requestBody"]["content"]["application/json"]["examples"]["limited"]
                ["value"]["monthly_limit_usd"],
            serde_json::json!("50.00"),
            "the limited example must set the USD limit"
        );
        let patch_key = &doc["paths"]["/admin/keys/{id}"]["patch"];
        assert_eq!(
            patch_key["requestBody"]["content"]["application/json"]["examples"]["set_limit"]
                ["value"]["monthly_limit_usd"],
            serde_json::json!("50.00"),
            "the patch example must set the USD limit"
        );
        assert!(
            patch_key["requestBody"]["content"]["application/json"]["examples"]["clear_limit"]
                ["value"]["monthly_limit_usd"]
                .is_null(),
            "the patch example must clear the limit with an explicit null"
        );

        // The /admin/logs route template has no {param} placeholders, so a
        // filter emitted as a path parameter is unusable from Swagger UI and
        // a required one misrepresents the handler. Location and
        // optionality are exactly what the route is about.
        let logs = &doc["paths"]["/admin/logs"]["get"];
        let params = logs["parameters"].as_array().unwrap();
        assert_eq!(params.len(), 8, "all eight log filters must be documented");
        for param in params {
            assert_eq!(
                param["in"], "query",
                "log filters must be query parameters, not path"
            );
            assert_ne!(
                param["required"], true,
                "log filters must be optional: {}",
                param["name"]
            );
        }
        let schemes = &doc["components"]["securitySchemes"];
        assert_eq!(schemes["basic_auth"]["scheme"], "basic");
        assert_eq!(schemes["bearer_auth"]["scheme"], "bearer");
    }
}
