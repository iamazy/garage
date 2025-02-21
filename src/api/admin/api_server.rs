use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use http::header::{ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ALLOW};
use hyper::{body::Incoming as IncomingBody, Request, Response, StatusCode};
use tokio::sync::watch;

use opentelemetry::trace::SpanRef;

#[cfg(feature = "metrics")]
use opentelemetry_prometheus::PrometheusExporter;
#[cfg(feature = "metrics")]
use prometheus::{Encoder, TextEncoder};

use garage_model::garage::Garage;
use garage_rpc::system::ClusterHealthStatus;
use garage_util::error::Error as GarageError;
use garage_util::socket_address::UnixOrTCPSocketAddress;

use crate::generic_server::*;

use crate::admin::bucket::*;
use crate::admin::cluster::*;
use crate::admin::error::*;
use crate::admin::key::*;
use crate::admin::router_v0;
use crate::admin::router_v1::{Authorization, Endpoint};
use crate::helpers::*;

pub type ResBody = BoxBody<Error>;

pub struct AdminApiServer {
	garage: Arc<Garage>,
	#[cfg(feature = "metrics")]
	exporter: PrometheusExporter,
	metrics_token: Option<String>,
	admin_token: Option<String>,
}

impl AdminApiServer {
	pub fn new(
		garage: Arc<Garage>,
		#[cfg(feature = "metrics")] exporter: PrometheusExporter,
	) -> Self {
		let cfg = &garage.config.admin;
		let metrics_token = cfg
			.metrics_token
			.as_ref()
			.map(|tok| format!("Bearer {}", tok));
		let admin_token = cfg
			.admin_token
			.as_ref()
			.map(|tok| format!("Bearer {}", tok));
		Self {
			garage,
			#[cfg(feature = "metrics")]
			exporter,
			metrics_token,
			admin_token,
		}
	}

	pub async fn run(
		self,
		bind_addr: UnixOrTCPSocketAddress,
		must_exit: watch::Receiver<bool>,
	) -> Result<(), GarageError> {
		let region = self.garage.config.s3_api.s3_region.clone();
		ApiServer::new(region, self)
			.run_server(bind_addr, Some(0o220), must_exit)
			.await
	}

	fn handle_options(&self, _req: &Request<IncomingBody>) -> Result<Response<ResBody>, Error> {
		Ok(Response::builder()
			.status(StatusCode::NO_CONTENT)
			.header(ALLOW, "OPTIONS, GET, POST")
			.header(ACCESS_CONTROL_ALLOW_METHODS, "OPTIONS, GET, POST")
			.header(ACCESS_CONTROL_ALLOW_ORIGIN, "*")
			.body(empty_body())?)
	}

	async fn handle_check_domain(
		&self,
		req: Request<IncomingBody>,
	) -> Result<Response<ResBody>, Error> {
		let query_params: HashMap<String, String> = req
			.uri()
			.query()
			.map(|v| {
				url::form_urlencoded::parse(v.as_bytes())
					.into_owned()
					.collect()
			})
			.unwrap_or_else(HashMap::new);

		let has_domain_key = query_params.contains_key("domain");

		if !has_domain_key {
			return Err(Error::bad_request("No domain query string found"));
		}

		let domain = query_params
			.get("domain")
			.ok_or_internal_error("Could not parse domain query string")?;

		if self.check_domain(domain).await? {
			Ok(Response::builder()
				.status(StatusCode::OK)
				.body(string_body(format!(
					"Domain '{domain}' is managed by Garage"
				)))?)
		} else {
			Err(Error::bad_request(format!(
				"Domain '{domain}' is not managed by Garage"
			)))
		}
	}

	async fn check_domain(&self, domain: &str) -> Result<bool, Error> {
		// Resolve bucket from domain name, inferring if the website must be activated for the
		// domain to be valid.
		let (bucket_name, must_check_website) = if let Some(bname) = self
			.garage
			.config
			.s3_api
			.root_domain
			.as_ref()
			.and_then(|rd| host_to_bucket(domain, rd))
		{
			(bname.to_string(), false)
		} else if let Some(bname) = self
			.garage
			.config
			.s3_web
			.as_ref()
			.and_then(|sw| host_to_bucket(domain, sw.root_domain.as_str()))
		{
			(bname.to_string(), true)
		} else {
			(domain.to_string(), true)
		};

		let bucket_id = match self
			.garage
			.bucket_helper()
			.resolve_global_bucket_name(&bucket_name)
			.await?
		{
			Some(bucket_id) => bucket_id,
			None => return Ok(false),
		};

		if !must_check_website {
			return Ok(true);
		}

		let bucket = self
			.garage
			.bucket_helper()
			.get_existing_bucket(bucket_id)
			.await?;

		let bucket_state = bucket.state.as_option().unwrap();
		let bucket_website_config = bucket_state.website_config.get();

		match bucket_website_config {
			Some(_v) => Ok(true),
			None => Ok(false),
		}
	}

	fn handle_health(&self) -> Result<Response<ResBody>, Error> {
		let health = self.garage.system.health();

		let (status, status_str) = match health.status {
			ClusterHealthStatus::Healthy => (StatusCode::OK, "Garage is fully operational"),
			ClusterHealthStatus::Degraded => (
				StatusCode::OK,
				"Garage is operational but some storage nodes are unavailable",
			),
			ClusterHealthStatus::Unavailable => (
				StatusCode::SERVICE_UNAVAILABLE,
				"Quorum is not available for some/all partitions, reads and writes will fail",
			),
		};
		let status_str = format!(
			"{}\nConsult the full health check API endpoint at /v1/health for more details\n",
			status_str
		);

		Ok(Response::builder()
			.status(status)
			.header(http::header::CONTENT_TYPE, "text/plain")
			.body(string_body(status_str))?)
	}

	fn handle_metrics(&self) -> Result<Response<ResBody>, Error> {
		#[cfg(feature = "metrics")]
		{
			use opentelemetry::trace::Tracer;

			let mut buffer = vec![];
			let encoder = TextEncoder::new();

			let tracer = opentelemetry::global::tracer("garage");
			let metric_families = tracer.in_span("admin/gather_metrics", |_| {
				self.exporter.registry().gather()
			});

			encoder
				.encode(&metric_families, &mut buffer)
				.ok_or_internal_error("Could not serialize metrics")?;

			Ok(Response::builder()
				.status(StatusCode::OK)
				.header(http::header::CONTENT_TYPE, encoder.format_type())
				.body(bytes_body(buffer.into()))?)
		}
		#[cfg(not(feature = "metrics"))]
		Err(Error::bad_request(
			"Garage was built without the metrics feature".to_string(),
		))
	}
}

#[async_trait]
impl ApiHandler for AdminApiServer {
	const API_NAME: &'static str = "admin";
	const API_NAME_DISPLAY: &'static str = "Admin";

	type Endpoint = Endpoint;
	type Error = Error;

	fn parse_endpoint(&self, req: &Request<IncomingBody>) -> Result<Endpoint, Error> {
		if req.uri().path().starts_with("/v0/") {
			let endpoint_v0 = router_v0::Endpoint::from_request(req)?;
			Endpoint::from_v0(endpoint_v0)
		} else {
			Endpoint::from_request(req)
		}
	}

	async fn handle(
		&self,
		req: Request<IncomingBody>,
		endpoint: Endpoint,
	) -> Result<Response<ResBody>, Error> {
		let expected_auth_header =
			match endpoint.authorization_type() {
				Authorization::None => None,
				Authorization::MetricsToken => self.metrics_token.as_ref(),
				Authorization::AdminToken => match &self.admin_token {
					None => return Err(Error::forbidden(
						"Admin token isn't configured, admin API access is disabled for security.",
					)),
					Some(t) => Some(t),
				},
			};

		if let Some(h) = expected_auth_header {
			match req.headers().get("Authorization") {
				None => return Err(Error::forbidden("Authorization token must be provided")),
				Some(v) => {
					let authorized = v.to_str().map(|hv| hv.trim() == h).unwrap_or(false);
					if !authorized {
						return Err(Error::forbidden("Invalid authorization token provided"));
					}
				}
			}
		}

		match endpoint {
			Endpoint::Options => self.handle_options(&req),
			Endpoint::CheckDomain => self.handle_check_domain(req).await,
			Endpoint::Health => self.handle_health(),
			Endpoint::Metrics => self.handle_metrics(),
			Endpoint::GetClusterStatus => handle_get_cluster_status(&self.garage).await,
			Endpoint::GetClusterHealth => handle_get_cluster_health(&self.garage).await,
			Endpoint::ConnectClusterNodes => handle_connect_cluster_nodes(&self.garage, req).await,
			// Layout
			Endpoint::GetClusterLayout => handle_get_cluster_layout(&self.garage).await,
			Endpoint::UpdateClusterLayout => handle_update_cluster_layout(&self.garage, req).await,
			Endpoint::ApplyClusterLayout => handle_apply_cluster_layout(&self.garage, req).await,
			Endpoint::RevertClusterLayout => handle_revert_cluster_layout(&self.garage, req).await,
			// Keys
			Endpoint::ListKeys => handle_list_keys(&self.garage).await,
			Endpoint::GetKeyInfo {
				id,
				search,
				show_secret_key,
			} => {
				let show_secret_key = show_secret_key.map(|x| x == "true").unwrap_or(false);
				handle_get_key_info(&self.garage, id, search, show_secret_key).await
			}
			Endpoint::CreateKey => handle_create_key(&self.garage, req).await,
			Endpoint::ImportKey => handle_import_key(&self.garage, req).await,
			Endpoint::UpdateKey { id } => handle_update_key(&self.garage, id, req).await,
			Endpoint::DeleteKey { id } => handle_delete_key(&self.garage, id).await,
			// Buckets
			Endpoint::ListBuckets => handle_list_buckets(&self.garage).await,
			Endpoint::GetBucketInfo { id, global_alias } => {
				handle_get_bucket_info(&self.garage, id, global_alias).await
			}
			Endpoint::CreateBucket => handle_create_bucket(&self.garage, req).await,
			Endpoint::DeleteBucket { id } => handle_delete_bucket(&self.garage, id).await,
			Endpoint::UpdateBucket { id } => handle_update_bucket(&self.garage, id, req).await,
			// Bucket-key permissions
			Endpoint::BucketAllowKey => {
				handle_bucket_change_key_perm(&self.garage, req, true).await
			}
			Endpoint::BucketDenyKey => {
				handle_bucket_change_key_perm(&self.garage, req, false).await
			}
			// Bucket aliasing
			Endpoint::GlobalAliasBucket { id, alias } => {
				handle_global_alias_bucket(&self.garage, id, alias).await
			}
			Endpoint::GlobalUnaliasBucket { id, alias } => {
				handle_global_unalias_bucket(&self.garage, id, alias).await
			}
			Endpoint::LocalAliasBucket {
				id,
				access_key_id,
				alias,
			} => handle_local_alias_bucket(&self.garage, id, access_key_id, alias).await,
			Endpoint::LocalUnaliasBucket {
				id,
				access_key_id,
				alias,
			} => handle_local_unalias_bucket(&self.garage, id, access_key_id, alias).await,
		}
	}
}

impl ApiEndpoint for Endpoint {
	fn name(&self) -> &'static str {
		Endpoint::name(self)
	}

	fn add_span_attributes(&self, _span: SpanRef<'_>) {}
}
