use crate::{Command, Watcher};
use async_stream::try_stream;
use futures::stream::BoxStream;
use futures::{FutureExt, Stream, TryStreamExt};
use prost::bytes::BytesMut;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::utils::config::AppUrl;
use tauri::{AppHandle, Runtime, WindowUrl};
use tauri_devtools_wire_format as wire;
use tauri_devtools_wire_format::instrument::InstrumentRequest;
use tauri_devtools_wire_format::tauri::{Config, ConfigRequest, Metrics, MetricsRequest};
use tauri_devtools_wire_format::workspace::{Chunk, Entry, EntryRequest, FileType};
use tokio::io::AsyncRead;
use tokio::sync::{mpsc, RwLock};
use tonic::codegen::http::Method;
use tonic::codegen::tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::HealthReporter;
use tonic_health::ServingStatus;
use tower_http::cors::{AllowHeaders, CorsLayer};

/// Default maximum capacity for the channel of events sent from a
/// [`Server`] to each subscribed client.
///
/// When this capacity is exhausted, the client is assumed to be inactive,
/// and may be disconnected.
const DEFAULT_CLIENT_BUFFER_CAPACITY: usize = 1024 * 4;
pub const DEFAULT_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3000);

pub(crate) struct Server<R: Runtime> {
	instrument: InstrumentServer,
	tauri: TauriServer<R>,
	workspace: WorkspaceService<R>,
	health: HealthServer<tonic_health::server::HealthService>,
}

struct InstrumentServer {
	tx: mpsc::Sender<Command>,
	health_reporter: HealthReporter,
}

struct TauriServer<R: Runtime> {
	app_handle: AppHandle<R>,
	metrics: Arc<RwLock<Metrics>>,
}

struct WorkspaceService<R: Runtime> {
	app_handle: AppHandle<R>,
}
impl<R: Runtime> Server<R> {
	pub fn new(tx: mpsc::Sender<Command>, app_handle: AppHandle<R>, metrics: Arc<RwLock<Metrics>>) -> Self {
		let (mut health_reporter, health_service) = tonic_health::server::health_reporter();

		health_reporter
			.set_serving::<wire::instrument::instrument_server::InstrumentServer<InstrumentServer>>()
			.now_or_never()
			.unwrap();
		health_reporter
			.set_serving::<wire::tauri::tauri_server::TauriServer<TauriServer<R>>>()
			.now_or_never()
			.unwrap();

		Self {
			instrument: InstrumentServer { tx, health_reporter },
			tauri: TauriServer {
				app_handle: app_handle.clone(),
				metrics,
			}, // the TauriServer doesn't need a health_reporter. It can never fail.
			workspace: WorkspaceService { app_handle },
			health: unsafe { std::mem::transmute(health_service) },
		}
	}

	pub async fn run(self, addr: SocketAddr) -> crate::Result<()> {
		tracing::info!("Listening on {}", addr);

		let cors = CorsLayer::new()
			// allow `GET` and `POST` when accessing the resource
			.allow_methods([Method::GET, Method::POST])
			.allow_headers(AllowHeaders::any())
			// allow requests from any origin
			.allow_origin(tower_http::cors::Any);

		tonic::transport::Server::builder()
			.accept_http1(true)
			.layer(cors)
			.add_service(tonic_web::enable(
				wire::instrument::instrument_server::InstrumentServer::new(self.instrument),
			))
			.add_service(tonic_web::enable(wire::tauri::tauri_server::TauriServer::new(
				self.tauri,
			)))
			.add_service(tonic_web::enable(
				wire::workspace::workspace_server::WorkspaceServer::new(self.workspace),
			))
			.add_service(tonic_web::enable(self.health))
			.serve(addr)
			.await?;

		Ok(())
	}
}

impl InstrumentServer {
	async fn set_status(&self, status: ServingStatus) {
		let mut r = self.health_reporter.clone();
		r.set_service_status("rs.devtools.instrument.Instrument", status).await;
	}
}

#[tonic::async_trait]
impl wire::instrument::instrument_server::Instrument for InstrumentServer {
	type WatchUpdatesStream = BoxStream<'static, Result<wire::instrument::Update, Status>>;

	async fn watch_updates(
		&self,
		req: Request<InstrumentRequest>,
	) -> Result<Response<Self::WatchUpdatesStream>, Status> {
		match req.remote_addr() {
			Some(addr) => tracing::debug!(client.addr = %addr, "starting a new watch"),
			None => tracing::debug!(client.addr = %"<unknown>", "starting a new watch"),
		}

		// reserve capacity to message the broadcaster
		let Ok(permit) = self.tx.reserve().await else {
			self.set_status(ServingStatus::NotServing).await;
			return Err(Status::internal(
				"cannot start new watch, aggregation task is not running",
			));
		};

		// create output channel and send tx to the aggregator for tracking
		let (tx, rx) = mpsc::channel(DEFAULT_CLIENT_BUFFER_CAPACITY);

		let params = req.into_inner();

		permit.send(Command::Instrument(Watcher {
			log_filter: params.log_filter,
			span_filter: params.span_filter,
			tx,
		}));

		tracing::debug!("watch started");

		let stream = ReceiverStream::new(rx).or_else(|err| async move {
			tracing::error!("Aggregator failed with error {err:?}");

			// TODO set the health service status to NotServing here

			Err(Status::internal("boom"))
		});

		Ok(Response::new(Box::pin(stream)))
	}
}
#[tonic::async_trait]
impl<R: Runtime> wire::tauri::tauri_server::Tauri for TauriServer<R> {
	async fn get_config(&self, _req: Request<ConfigRequest>) -> Result<Response<Config>, Status> {
		let config: Config = (&*self.app_handle.config()).into();

		Ok(Response::new(config))
	}

	async fn get_metrics(&self, _req: Request<MetricsRequest>) -> Result<Response<Metrics>, Status> {
		let metrics = self.metrics.read().await;

		Ok(Response::new(metrics.clone()))
	}
}

#[tonic::async_trait]
impl<R: Runtime> wire::workspace::workspace_server::Workspace for WorkspaceService<R> {
	type ListEntriesStream = BoxStream<'static, Result<Entry, Status>>;
	async fn list_entries(&self, req: Request<EntryRequest>) -> Result<Response<Self::ListEntriesStream>, Status> {
		tracing::debug!("list entries");
		let mut cwd = std::env::current_dir()?;
		cwd.push(req.into_inner().path);

		let stream = self.list_entries_inner(cwd).or_else(|err| async move {
			tracing::error!("Aggregator failed with error {err:?}");

			// TODO set the health service status to NotServing here

			Err(Status::internal("boom"))
		});

		Ok(Response::new(Box::pin(stream)))
	}

	type GetEntryBytesStream = BoxStream<'static, Result<Chunk, Status>>;

	async fn get_entry_bytes(&self, req: Request<EntryRequest>) -> Result<Response<Self::GetEntryBytesStream>, Status> {
		let mut path = std::env::current_dir()?;
		path.push(req.into_inner().path);

		let stream = try_stream! {
			use tokio::io::AsyncReadExt;
			let mut file = tokio::fs::File::open(path).await?;
			let mut buf = BytesMut::with_capacity(512);

			while let Ok(n) = file.read_buf(&mut buf).await {
				if n == 0 {
					break;
				}
				yield Chunk { bytes: buf.split().freeze() };
			}
		};

		Ok(Response::new(Box::pin(stream)))
	}
}

impl<R: Runtime> WorkspaceService<R> {
	fn list_entries_inner(&self, root: PathBuf) -> impl Stream<Item = crate::Result<Entry>> {
		let app_handle = self.app_handle.clone();

		try_stream! {
			let mut entries = tokio::fs::read_dir(&root).await?;

			while let Some(entry) = entries.next_entry().await? {
				let raw_file_type = entry.file_type().await?;
				let mut file_type = FileType::empty();
				if raw_file_type.is_dir() {
					file_type |= FileType::DIR;
				}
				if raw_file_type.is_file() {
					file_type |= FileType::FILE;
				}
				if raw_file_type.is_symlink() {
					file_type |= FileType::SYMLINK;
				}

				let path = entry.path();
				let path = path.strip_prefix(&root)?;

				if is_asset(path, &app_handle.config().build.dist_dir) {
					file_type |= FileType::ASSET;
				}

				yield Entry {
					path: path.to_string_lossy().to_string(),
					size: entry.metadata().await?.len(),
					file_type: file_type.bits(),
				};
			}
		}
	}
}

fn is_asset(path: &Path, app_url: &AppUrl) -> bool {
	match app_url {
		AppUrl::Url(WindowUrl::External(_)) => false,
		AppUrl::Url(WindowUrl::App(p)) => p == path,
		AppUrl::Files(files) => files.iter().any(|p| p == path),
		_ => unreachable!(),
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use std::time::SystemTime;
	use tauri_devtools_wire_format as wire;
	use tauri_devtools_wire_format::instrument::instrument_server::Instrument;
	use tauri_devtools_wire_format::instrument::Filter;
	use tauri_devtools_wire_format::metadata::Level;
	use wire::tauri::tauri_server::Tauri;

	#[tokio::test]
	async fn tauri_get_config() {
		let tauri = TauriServer {
			app_handle: tauri::test::mock_app().handle(),
			metrics: Default::default(),
		};

		let cfg = tauri.get_config(Request::new(ConfigRequest {})).await.unwrap();

		assert_eq!(cfg.into_inner(), wire::tauri::Config::from(&*tauri.app_handle.config()));
	}

	#[tokio::test]
	async fn tauri_get_metrics() {
		let srv = TauriServer {
			app_handle: tauri::test::mock_app().handle(),
			metrics: Default::default(),
		};

		let metrics = srv.get_metrics(Request::new(MetricsRequest {})).await.unwrap();
		assert_eq!(metrics.into_inner(), *srv.metrics.read().await);

		let mut m = srv.metrics.write().await;
		m.initialized_at = Some(SystemTime::now().into());
		drop(m);

		let metrics = srv.get_metrics(Request::new(MetricsRequest {})).await.unwrap();
		assert_eq!(metrics.into_inner(), *srv.metrics.read().await);
	}

	#[tokio::test]
	async fn subscription() {
		let (health_reporter, _) = tonic_health::server::health_reporter();
		let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
		let srv = InstrumentServer {
			tx: cmd_tx,
			health_reporter,
		};

		let _stream = srv
			.watch_updates(Request::new(InstrumentRequest {
				log_filter: Some(Filter {
					level: Some(Level::Error as i32),
					file: None,
					text: None,
				}),
				span_filter: None,
			}))
			.await
			.unwrap();

		let cmd = cmd_rx.recv().await.unwrap();

		assert!(matches!(
			cmd,
			Command::Instrument(Watcher {
				log_filter: Some(Filter { level: Some(0), .. }),
				..
			})
		));
	}
}
