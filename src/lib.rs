#![deny(clippy::all)]
#![deny(rust_2018_idioms)]
pub use cache::Cache;
use hyper::{client::HttpConnector, Body, Client, Method, Request, Response, Server};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use http::{StatusCode, Uri};
use slog::{error, info, Logger};

pub mod cache;
pub mod config;
pub mod market;
pub mod sentry_api;
pub mod status;
pub mod util;

use market::MarketApi;

pub use config::{Config, Timeouts};
pub use sentry_api::SentryApi;

static ROUTE_UNITS_FOR_SLOT: &str = "/units-for-slot/";

#[derive(Debug)]
pub enum Error {
    Hyper(hyper::Error),
    Http(http::Error),
    Reqwest(reqwest::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hyper(e) => e.fmt(f),
            Error::Http(e) => e.fmt(f),
            Error::Reqwest(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<hyper::Error> for Error {
    fn from(e: hyper::Error) -> Error {
        Error::Hyper(e)
    }
}

impl From<http::Error> for Error {
    fn from(e: http::Error) -> Error {
        Error::Http(e)
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Error {
        Error::Reqwest(e)
    }
}

impl From<http::uri::InvalidUri> for Error {
    fn from(e: http::uri::InvalidUri) -> Error {
        Error::Http(e.into())
    }
}

pub async fn serve(
    addr: SocketAddr,
    logger: Logger,
    market_url: String,
    config: Config,
) -> Result<(), Error> {
    use hyper::service::{make_service_fn, service_fn};

    let client = Client::new();
    let market = Arc::new(MarketApi::new(market_url, logger.clone())?);

    let cache = spawn_fetch_campaigns(logger.clone(), config).await?;

    // And a MakeService to handle each connection...
    let make_service = make_service_fn(|_| {
        let client = client.clone();
        let cache = cache.clone();
        let logger = logger.clone();
        let market = market.clone();
        async move {
            Ok::<_, Error>(service_fn(move |req| {
                let client = client.clone();
                let cache = cache.clone();
                let market = market.clone();
                let logger = logger.clone();
                async move { handle(req, cache, client, logger, market).await }
            }))
        }
    });

    // Then bind and serve...
    let server = Server::bind(&addr).serve(make_service);

    // And run forever...
    if let Err(e) = server.await {
        error!(&logger, "server error: {}", e);
    }

    Ok(())
}

async fn handle(
    mut req: Request<Body>,
    _cache: Cache,
    client: Client<HttpConnector>,
    logger: Logger,
    market: Arc<MarketApi>,
) -> Result<Response<Body>, Error> {
    let is_units_for_slot = req.uri().path().starts_with(ROUTE_UNITS_FOR_SLOT);

    match (is_units_for_slot, req.method()) {
        (true, &Method::GET) => {
            let ipfs = req.uri().path().trim_start_matches(ROUTE_UNITS_FOR_SLOT);

            if ipfs.is_empty() {
                Ok(not_found())
            } else {
                let ad_slot_result = market.fetch_slot(&ipfs).await?;

                let ad_slot = match ad_slot_result {
                    Some(ad_slot) => {
                        info!(&logger, "Fetched AdSlot"; "AdSlot" => ipfs);

                        ad_slot
                    }
                    None => {
                        info!(
                            &logger,
                            "AdSlot ({}) not found in Market",
                            ipfs;
                            "AdSlot" => ipfs
                        );
                        return Ok(not_found());
                    }
                };

                let units = market.fetch_units(&ad_slot).await?;

                let units_ipfses: Vec<String> = units.iter().map(|au| au.ipfs.clone()).collect();

                info!(&logger, "Fetched AdUnits for AdSlot"; "AdSlot" => ipfs, "AdUnits" => ?&units_ipfses);

                // Applying targeting.
                // Optional but should always be applied unless there is a `?noTargeting` query parameter provided.
                // It should find matches between the unit targeting and the slot tags.
                // More details on how to implement after the targeting overhaul
                let _apply_targeting = req
                    .uri()
                    .query()
                    .map(|q| !q.contains("noTargeting"))
                    .unwrap_or(true);

                // @TODO: Apply trageting!
                // @TODO: https://github.com/AdExNetwork/adex-supermarket/issues/9

                Ok(Response::new(Body::from("")))
            }
        }
        (_, method) => {
            use http::uri::PathAndQuery;

            let method = method.clone();

            let path_and_query = req
                .uri()
                .path_and_query()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| PathAndQuery::from_static(""));

            let uri = format!("{}{}", market.market_url, path_and_query);

            *req.uri_mut() = uri.parse::<Uri>()?;

            let proxy_response = match client.request(req).await {
                Ok(response) => {
                    info!(&logger, "Proxied request to market"; "uri" => uri, "method" => %method);

                    response
                }
                Err(err) => {
                    error!(&logger, "Proxying request to market failed"; "uri" => uri, "method" => %method, "error" => ?&err);

                    service_unavaiable()
                }
            };

            Ok(proxy_response)
        }
    }
}

async fn spawn_fetch_campaigns(logger: Logger, config: Config) -> Result<Cache, reqwest::Error> {
    info!(
        &logger,
        "Initialize Cache"; "validators" => format_args!("{:?}", &config.validators)
    );
    let cache = Cache::initialize(logger.clone(), config.clone()).await?;

    let cache_spawn = cache.clone();
    // Every few minutes, we will get the non-finalized from the market,
    // in order to keep discovering new campaigns.
    tokio::spawn(async move {
        use futures::stream::{select, StreamExt};
        use tokio::time::{interval, timeout, Instant};
        info!(&logger, "Task for updating campaign has been spawned");

        // Every X seconds, we will update our active campaigns from the
        // validators (update their latest balance tree).
        let new_interval = interval(config.fetch_campaigns_every).map(TimeFor::New);
        let update_interval = interval(config.update_campaigns_every).map(TimeFor::Update);

        enum TimeFor {
            New(Instant),
            Update(Instant),
        }

        let mut select_time = select(new_interval, update_interval);

        while let Some(time_for) = select_time.next().await {
            // @TODO: Timeout the action
            match time_for {
                TimeFor::New(_) => {
                    let timeout_duration = config.timeouts.cache_fetch_campaigns_from_market;

                    match timeout(timeout_duration, cache_spawn.fetch_new_campaigns()).await {
                        Err(_elapsed) => error!(
                            &logger,
                            "Fetching new Campaigns timed out";
                            "allowed secs" => timeout_duration.as_secs()
                        ),
                        _ => info!(&logger, "New Campaigns fetched from Validators!"),
                    }
                }
                TimeFor::Update(_) => {
                    let timeout_duration = config.timeouts.cache_update_campaign_statuses;

                    match timeout(timeout_duration, cache_spawn.fetch_campaign_updates()).await {
                        Ok(_) => info!(&logger, "Campaigns statuses updated from Validators!"),
                        Err(_elapsed) => error!(
                                &logger,
                                "Updating Campaigns statuses timed out";
                                "allowed secs" => timeout_duration.as_secs()
                        ),
                    }
                }
            }
        }
    });

    Ok(cache)
}

fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("Not Found response should be valid")
}

fn service_unavaiable() -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(Body::empty())
        .expect("Bad Request response should be valid")
}
