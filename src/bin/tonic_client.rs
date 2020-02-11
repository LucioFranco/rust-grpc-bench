#[macro_use]
extern crate log;

use futures::future::try_join_all;
use hello_world::greeter_client::GreeterClient;
use hello_world::HelloRequest;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::spawn;
use tokio::time::delay_for;

pub mod hello_world {
    tonic::include_proto!("helloworld");
}

#[derive(Default)]
struct State {
    request_count: AtomicUsize,
    failed_requests: AtomicUsize,
    in_flight: AtomicUsize,
    max_age: AtomicUsize,
    request_time: AtomicUsize,
}

type Client = GreeterClient<AddOrigin<hyper::client::conn::Http2SendRequest<tonic::body::BoxBody>>>;

async fn do_work(mut client: Client, state: Arc<State>) {
    let start = Instant::now();
    state.in_flight.fetch_add(1, Ordering::SeqCst);

    let request = tonic::Request::new(HelloRequest {
        name: "Tonic".into(),
    });

    let response = client.say_hello(request).await;
    match response {
        Ok(_response) => {
            state.request_count.fetch_add(1, Ordering::SeqCst);
        }
        Err(e) => {
            state.failed_requests.fetch_add(1, Ordering::SeqCst);
            println!("{}", e);
        }
    }

    let age = Instant::now().duration_since(start).as_micros() as usize;
    state.request_time.fetch_add(age, Ordering::SeqCst);

    if age > state.max_age.load(Ordering::SeqCst) {
        state.max_age.store(age, Ordering::SeqCst)
    }

    state.in_flight.fetch_sub(1, Ordering::SeqCst);
}

async fn work_loop(client: Client, state: Arc<State>) {
    loop {
        do_work(client.clone(), state.clone()).await;
    }
}

async fn log_loop(state: Arc<State>) {
    let mut last_request_count = 0;
    let mut last = Instant::now();
    let start = Instant::now();
    loop {
        delay_for(Duration::from_secs(1)).await;
        let now = Instant::now();
        let elapsed = now.duration_since(last).as_millis() as f64 / 1000.0;
        let request_count = state.request_count.load(Ordering::SeqCst);
        let req_sec = (request_count - last_request_count) as f64 / elapsed;
        let start_elapsed = now.duration_since(start).as_secs();
        let total_req_sec = request_count / start_elapsed as usize;
        let avg_time = if request_count > last_request_count {
            state.request_time.load(Ordering::SeqCst) / (request_count - last_request_count)
        } else {
            0
        };

        let failed_requests = state.failed_requests.load(Ordering::SeqCst);
        let in_flight = state.in_flight.load(Ordering::SeqCst);
        let max_age = state.max_age.load(Ordering::SeqCst);
        info!(
            "{} total requests ({}/sec last 1 sec) ({}/sec total). last log {} sec ago. {} failed, {} in flight, {} µs max, {} µs avg response time",
            request_count, req_sec, total_req_sec, elapsed, failed_requests, in_flight, max_age, avg_time
        );
        last_request_count = request_count;
        last = now;
        state.max_age.store(0, Ordering::SeqCst);
        state.request_time.store(0, Ordering::SeqCst);
    }
}

pub fn configure_logging() -> Result<(), Box<dyn std::error::Error>> {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}:{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S]"),
                record.level(),
                record.target(),
                record.line().unwrap_or(0),
                message
            ));
        })
        .level(log::LevelFilter::Info)
        .level_for("discovery", log::LevelFilter::Trace)
        .level_for("hyper", log::LevelFilter::Warn)
        .level_for("tokio_core", log::LevelFilter::Warn)
        .level_for("tokio_reactor", log::LevelFilter::Warn)
        .level_for("h2", log::LevelFilter::Warn)
        .level_for("tower_buffer", log::LevelFilter::Warn)
        .chain(std::io::stdout())
        .apply()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(State::default());
    let log_state = state.clone();
    configure_logging()?;

    spawn(async move {
        log_loop(log_state).await;
    });

    let settings = hyper::client::conn::Builder::new().http2_only(true).clone();
    let connector = hyper::client::connect::HttpConnector::new();
    let mut connector = hyper::client::service::Connect::new(connector, settings);
    use tower::Service;
    let uri = "http://[::1]:50051".parse::<http::Uri>().unwrap();
    let conn = connector.call(uri.clone()).await.unwrap();

    let add_origin = AddOrigin::new(conn.into_http2(), uri);

    let client = GreeterClient::new(add_origin);
    let mut futs = vec![];
    for _ in 0..100 {
        futs.push(spawn(work_loop(client.clone(), state.clone())));
    }

    try_join_all(futs).await?;

    Ok(())
}

use http::{Request, Uri};
use std::task::{Context, Poll};
use tower::Service;

#[derive(Clone, Debug)]
pub(crate) struct AddOrigin<T> {
    inner: T,
    origin: Uri,
}

impl<T> AddOrigin<T> {
    pub(crate) fn new(inner: T, origin: Uri) -> Self {
        Self { inner, origin }
    }
}

impl<T, ReqBody> Service<Request<ReqBody>> for AddOrigin<T>
where
    T: Service<Request<ReqBody>>,
{
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        // Split the request into the head and the body.
        let (mut head, body) = req.into_parts();

        // Split the request URI into parts.
        let mut uri: http::uri::Parts = head.uri.into();
        let set_uri = self.origin.clone().into_parts();

        // Update the URI parts, setting hte scheme and authority
        uri.scheme = Some(set_uri.scheme.expect("expected scheme").clone());
        uri.authority = Some(set_uri.authority.expect("expected authority").clone());

        // Update the the request URI
        head.uri = http::Uri::from_parts(uri).expect("valid uri");

        let request = Request::from_parts(head, body);

        self.inner.call(request)
    }
}
