//! Sequencer-specific API options and initialization.

use super::{
    data_source::SequencerDataSource, endpoints, fs, sql, update::update_loop, AppState, Consensus,
    NodeIndex, SequencerNode,
};
use crate::network;
use async_std::{
    sync::{Arc, RwLock},
    task::spawn,
};
use clap::Parser;
use futures::future::{BoxFuture, TryFutureExt};
use hotshot_query_service::{data_source::ExtensibleDataSource, status, Error};
use hotshot_types::traits::metrics::{Metrics, NoMetrics};
use std::path::PathBuf;
use tide_disco::App;

#[derive(Clone, Debug)]
pub struct Options {
    pub http: Http,
    pub query_sql: Option<Sql>,
    pub query_fs: Option<Fs>,
    pub submit: Option<Submit>,
}

impl From<Http> for Options {
    fn from(http: Http) -> Self {
        Self {
            http,
            query_sql: None,
            query_fs: None,
            submit: None,
        }
    }
}

impl Options {
    /// Add a query API module backed by a Postgres database.
    pub fn query_sql(mut self, opt: Sql) -> Self {
        self.query_sql = Some(opt);
        self
    }

    /// Add a query API module backed by the file system.
    pub fn query_fs(mut self, opt: Fs) -> Self {
        self.query_fs = Some(opt);
        self
    }

    /// Add a submit API module.
    pub fn submit(mut self, opt: Submit) -> Self {
        self.submit = Some(opt);
        self
    }

    /// Whether these options will run the query API.
    pub fn has_query_module(&self) -> bool {
        self.query_sql.is_some() || self.query_fs.is_some()
    }

    /// Start the server.
    ///
    /// The function `init_handle` is used to create a consensus handle from a metrics object. The
    /// metrics object is created from the API data source, so that consensus will populuate metrics
    /// that can then be read and served by the API.
    pub async fn serve<N, F>(self, init_handle: F) -> anyhow::Result<SequencerNode<N>>
    where
        N: network::Type,
        F: FnOnce(Box<dyn Metrics>) -> BoxFuture<'static, (Consensus<N>, NodeIndex)>,
    {
        // The server state type depends on whether we are running a query API or not, so we handle
        // the two cases differently.
        let node = if let Some(opt) = self.query_sql {
            init_with_query_module::<N, sql::DataSource<N>>(
                opt,
                init_handle,
                self.submit.is_some(),
                self.http.port,
            )
            .await?
        } else if let Some(opt) = self.query_fs {
            init_with_query_module::<N, fs::DataSource<N>>(
                opt,
                init_handle,
                self.submit.is_some(),
                self.http.port,
            )
            .await?
        } else {
            let (handle, node_index) = init_handle(Box::new(NoMetrics)).await;
            let mut app = App::<_, Error>::with_state(RwLock::new(handle.clone()));

            // Initialize submit API
            if self.submit.is_some() {
                let submit_api = endpoints::submit::<N, RwLock<Consensus<N>>>()?;
                app.register_module("submit", submit_api)?;
            }

            SequencerNode {
                handle,
                node_index,
                update_task: spawn(
                    app.serve(format!("0.0.0.0:{}", self.http.port))
                        .map_err(anyhow::Error::from),
                ),
            }
        };

        // Start consensus.
        node.handle.hotshot.start_consensus().await;
        Ok(node)
    }
}

/// The minimal HTTP API.
///
/// The API automatically includes health and version endpoints. Additional API modules can be
/// added by including the query-api or submit-api modules.
#[derive(Parser, Clone, Debug)]
pub struct Http {
    /// Port that the HTTP API will use.
    #[clap(long, env = "ESPRESSO_SEQUENCER_API_PORT")]
    pub port: u16,
}

/// Options for the submission API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Submit;

/// Options for the query API module backed by a Postgres database.
#[derive(Parser, Clone, Debug)]
pub struct Sql {
    /// Hostname for the remote Postgres database server.
    #[clap(long, env = "ESPRESSO_SEQUENCER_POSTGRES_HOST")]
    pub host: Option<String>,

    /// Port for the remote Postgres database server.
    #[clap(long, env = "ESPRESSO_SEQUENCER_POSTGRES_PORT")]
    pub port: Option<u16>,

    /// Name of database to connect to.
    #[clap(long, env = "ESPRESSO_SEQUENCER_POSTGRES_DATABASE")]
    pub database: Option<String>,

    /// Postgres user to connect as.
    #[clap(long, env = "ESPRESSO_SEQUENCER_POSTGRES_USER")]
    pub user: Option<String>,

    /// Password for Postgres user.
    #[clap(long, env = "ESPRESSO_SEQUENCER_POSTGRES_PASSWORD")]
    pub password: Option<String>,
}

/// Options for the query API module backed by the file system.
#[derive(Parser, Clone, Debug)]
pub struct Fs {
    /// Storage path for HotShot query service data.
    #[clap(long, env = "ESPRESSO_SEQUENCER_STORAGE_PATH")]
    pub storage_path: PathBuf,

    /// Create new query storage instead of opening existing one.
    #[clap(long, env = "ESPRESSO_SEQUENCER_RESET_STORE")]
    pub reset_store: bool,
}

async fn init_with_query_module<N, D>(
    opt: D::Options,
    init_handle: impl FnOnce(Box<dyn Metrics>) -> BoxFuture<'static, (Consensus<N>, NodeIndex)>,
    submit: bool,
    port: u16,
) -> anyhow::Result<SequencerNode<N>>
where
    N: network::Type,
    D: SequencerDataSource<N> + Send + Sync + 'static,
{
    type State<N, D> = Arc<RwLock<AppState<N, D>>>;

    let ds = D::create(opt).await?;
    let metrics = ds.populate_metrics();

    // Start up handle
    let (mut handle, node_index) = init_handle(metrics).await;

    // Get an event stream from the handle to use for populating the query data with
    // consensus events.
    //
    // We must do this _before_ starting consensus on the handle, otherwise we could miss
    // the first events emitted by consensus.
    let events = handle.get_event_stream(Default::default()).await.0;

    let state: State<N, D> = Arc::new(RwLock::new(ExtensibleDataSource::new(ds, handle.clone())));
    let mut app = App::<_, Error>::with_state(state.clone());

    // Initialize submit API
    if submit {
        let submit_api = endpoints::submit::<N, State<N, D>>()?;
        app.register_module("submit", submit_api)?;
    }

    // Initialize availability and status APIs
    let availability_api = endpoints::availability::<N, D>()?;
    let status_api = status::define_api::<State<N, D>>(&Default::default())?;

    // Register modules in app
    app.register_module("availability", availability_api)?
        .register_module("status", status_api)?;

    let update_task = spawn(async move {
        futures::join!(
            app.serve(format!("0.0.0.0:{port}"))
                .map_err(anyhow::Error::from),
            update_loop(state, events),
        )
        .0
    });

    Ok(SequencerNode {
        handle,
        node_index,
        update_task,
    })
}
