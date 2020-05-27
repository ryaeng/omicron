/*!
 * Interface for making API requests to the Oxide Control Plane at large from
 * within the control plane.  This should be replaced with a client generated
 * from the OpenAPI spec generated by the server.
 */

use crate::api_model::ApiDiskRuntimeState;
use crate::api_model::ApiDiskStateRequested;
use crate::api_model::ApiInstanceRuntimeState;
use crate::api_model::ApiInstanceRuntimeStateRequested;
use crate::api_error::ApiError;
use dropshot::HttpErrorResponseBody;
use http::Method;
use hyper::client::HttpConnector;
use hyper::Body;
use hyper::Client;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use slog::Logger;
use std::fmt::Display;
use std::net::SocketAddr;
use uuid::Uuid;

pub struct ControllerClient {
    server_addr: SocketAddr,
    log: Logger,
    http_client: Client<HttpConnector>,
}

impl ControllerClient {
    pub fn new(server_addr: SocketAddr, log: Logger) -> ControllerClient {
        ControllerClient {
            server_addr,
            log,
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Body,
    ) -> Result<Response<Body>, ApiError> {
        let error_message_base = format!(
            "client request to controller at {} ({} {})",
            self.server_addr, method, path
        );

        info!(self.log, "client request";
            "method" => %method,
            "uri" => %path,
            "body" => ?&body,
        );

        let uri = Uri::builder()
            .scheme("http")
            .authority(format!("{}", self.bind_address).as_str())
            .path_and_query(path)
            .build()
            .unwrap();
        let request =
            Request::builder().method(Method::PUT).uri(uri).body(body).unwrap();
        let result = self.http_client.request(request).await.map_err(|error| {
            convert_error(error_message_base, "making request", error)
        });

        info!(self.client_log, "client response"; "result" => ?result);
        let mut response = result?;
        let status = response.status_code;

        if !status.is_client_error() && !status.is_server_error() {
            return Ok(response);
        }

        /*
         * TODO-cleanup TODO-robustness commonize with dropshot and make
         * this more robust.
         */
        assert_eq!(
            dropshot::CONTENT_TYPE_JSON,
            response.headers().get(http::header::CONTENT_TYPE).unwrap()
        );
        let body_bytes =
            hyper::body::to_bytes(response.body_mut()).await.map_err(|error| {
                convert_error(error_message_base, "reading response", error)
            })?;
        let error_body: HttpErrorResponseBody =
            serde_json::from_slice(body_bytes.as_ref()).map_err(|error| {
                convert_error(error_message_base, "parsing response", error)
            })?;
        /* XXX Should this parse an error code out? */
        Err(ApiError::DependencyError {
            message: error_body.message,
        })
    }

    /**
     * Publish an updated runtime state for an Instance.
     */
    pub async fn notify_instance_updated(
        &self,
        id: &Uuid,
        new_runtime_state: &ApiInstanceRuntimeState,
    ) -> Result<(), ApiError> {
        let path = format!("/instances/{}", id);
        let body =
            Body::from(serde_json::to_string(new_runtime_state).unwrap());
        self.request(Method::PUT, path.as_str(), body).map(|_| ())
    }

    /**
     * Publish an updated runtime state for a Disk.
     */
    pub async fn notify_disk_updated(
        &self,
        id: &Uuid,
        new_state: &ApiDiskRuntimeState,
    ) -> Result<(), ApiError> {
        let path = format!("/disks/{}", id);
        let body = Body::from(serde_json::to_string(new_state).unwrap());
        self.request(Method::PUT, path.as_str(), body).map(|_| ())
    }
}

fn convert_error<E: Display>(
    error_message_base: &str,
    action: &str,
    error: E,
) -> ApiError {
    ApiError::ResourceNotAvailable {
        message: format!("{}: {}: {}", error_message_base, action, error),
    }
}
