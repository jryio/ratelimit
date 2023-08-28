use std::{
    collections::HashMap,
    error::Error,
    fmt::Display,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use axum::{
    error_handling::HandleErrorLayer,
    http::{header::AUTHORIZATION, Method, Request, StatusCode},
    routing::{get, post, put},
    BoxError, Router,
};
use tokio::time::Instant;
use tower::{Layer, Service, ServiceBuilder};

const MINUTE: u64 = 60;
const POST_LIMIT: usize = 3;
const GET_LIMIT: usize = 1200;
const PUT_LIMIT: usize = 3;
type Token = String;
type RateLimitState = Arc<RwLock<HashMap<Token, Arc<Mutex<usize>>>>>;

#[derive(Debug, Clone, Copy)]
// Rate is taken directly from tower::limit::Rate
struct Rate {
    num: usize,
    per: Duration,
}

// Rate is taken directly from tower::limit::Rate
impl Rate {
    pub fn new(num: usize, per: Duration) -> Self {
        assert!(num > 0);
        assert!(per > Duration::from_millis(0));

        Rate { num, per }
    }

    fn num(&self) -> usize {
        self.num
    }

    fn per(&self) -> Duration {
        self.per
    }
}

// ------------------------
//  LAYER
// ------------------------
#[derive(Clone)]
struct TokenRateLimitLayer {
    state: RateLimitState,
    rate: Rate,
}

impl TokenRateLimitLayer {
    pub fn new(state: RateLimitState, num: usize, per: Duration) -> Self {
        let rate = Rate::new(num, per);
        Self { state, rate }
    }
}

impl<S> Layer<S> for TokenRateLimitLayer
where
    S: Clone,
{
    type Service = TokenRateLimit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TokenRateLimit::new(inner, self.state.clone(), self.rate)
    }
}

// ------------------------
// SERVICE
// ------------------------
#[derive(Debug)]
// WARNING: I would have liked to have added a `time` field to this struct so we could have
// returned a timestamp in the response for when the API woudl become available.
struct RateLimitError();
impl Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rate limited")
    }
}
impl Error for RateLimitError {}

struct TokenRateLimit<S> {
    inner: S,
    state: RateLimitState,
    rate: Rate,
    last_time_renewed_reqs: Arc<Mutex<Instant>>,
    available_reqs: Arc<Mutex<usize>>,
}

impl<S> TokenRateLimit<S> {
    pub fn new(inner: S, state: RateLimitState, rate: Rate) -> Self {
        let max_reqs = rate.num();
        Self {
            inner,
            rate,
            state,
            last_time_renewed_reqs: Arc::new(Mutex::new(Instant::now())),
            available_reqs: Arc::new(Mutex::new(max_reqs)),
        }
    }
}

impl<S: Clone> Clone for TokenRateLimit<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            state: self.state.clone(),
            rate: self.rate,
            last_time_renewed_reqs: self.last_time_renewed_reqs.clone(),
            available_reqs: self.available_reqs.clone(),
        }
    }
}

impl<S, Body> Service<Request<Body>> for TokenRateLimit<S>
where
    S: Service<Request<Body>> + Send,
    S::Error: Into<BoxError>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // We need to look into the Request to determine if we have an Authorization header
        let auth = req.headers().get(AUTHORIZATION).unwrap();
        let mut auth = auth.to_str().unwrap().to_string();

        // Create a new key for PUT /vault/:id concatenating the token and the vault id
        // This is not an optimized key...
        let method = req.method();
        if method == Method::PUT {
            let uri = req.uri().path();
            auth = format!("{auth}+{uri}");
        }
        println!("TokenRateLimit -> call -> Bearer Token = {auth}");

        let state = self
            .state
            .read()
            .expect("Poisioned: The last writer panicked without releasing the write lock");

        if let Some(available_reqs) = state.get(auth.as_str()) {
            // Cloning an Arc<Mutex<usize>>
            self.available_reqs = available_reqs.clone();
            {
                let x = *available_reqs.lock().unwrap();
                println!("TokenRateLimit -> call -> We found an existing COUNTER for this bearer token = {auth} AVAILABLE = {x}");
            }
            // Release the read lock
            drop(state);
        } else {
            // At this point there should have been no match into state.get(auth)
            // But we still have a read lock open. Drop it to prevent deadlocking
            // when we try to get a write lock.
            drop(state);
            let new_available_req = Arc::new(Mutex::new(self.rate.num()));
            println!("TokenRateLimit -> call -> This is the first time we're seeing this bearer token = {auth} AVAILABLE = {}", self.rate.num());
            let mut state = self
                .state
                .write()
                .expect("Poisioned: The last writer panicked without releasing the write lock ");
            state.insert(auth.to_string(), new_available_req.clone());
            self.available_reqs = new_available_req;
            println!("TokenRateLimit -> call -> New available_reqs for bearer = {auth}");
            // Release the write lock so other threads can read
            drop(state);
        }
        // Run the handler
        let fut = self.inner.call(req);
        let available_reqs = Arc::clone(&self.available_reqs);
        let last_time_renewed_reqs = Arc::clone(&self.last_time_renewed_reqs);
        let rate = self.rate;

        let renew_available_reqs = move || {
            println!("TokenRateLimit -> renew_available_reqs");
            let mut reqs = available_reqs.lock().unwrap();
            let mut last_time_renewed_reqs = last_time_renewed_reqs.lock().unwrap();
            // Compute the duration between our last timestamp and NOW
            let duration_since_last_renew = last_time_renewed_reqs.elapsed();

            // When we've exceeded the duration of rate limiting, we can add new available requests
            if duration_since_last_renew > rate.per() {
                let secs_over: u64 = duration_since_last_renew.as_secs() % rate.per().as_secs();
                // Refill available requests for this Bearer token
                *reqs = rate.num();
                // Set last renewal timestamp to NOW
                *last_time_renewed_reqs = Instant::now();
                // Time inaccuracies
                if let Some(new_time) =
                    last_time_renewed_reqs.checked_sub(Duration::from_secs(secs_over))
                {
                    *last_time_renewed_reqs = new_time;
                }
            }
        };

        // Pin our future as the return value
        let available_reqs = Arc::clone(&self.available_reqs);
        Box::pin(async move {
            // Renew available reqs if possible
            renew_available_reqs();
            {
                let mut available_reqs = available_reqs.lock().unwrap();
                if *available_reqs > 0 {
                    *available_reqs -= 1;
                } else {
                    // No tokens, this is an error
                    return Err(Box::new(RateLimitError()).into());
                }
            }

            fut.await.map_err(|err| err.into())
        })
    }
}

async fn always_200() -> StatusCode {
    StatusCode::OK
}

#[tokio::main]
async fn main() {
    // Duration for all rate limited endpoints
    let minute = Duration::from_secs(MINUTE);

    // Generic error handling for all rate limiters
    //
    // This is necessary because axum::route_layer requires that: the Layer L we provide wraps a Service whose associated Error type is Infallible.
    // Since TokenRateLimit::Error is not Infallible, we can wrap it using HandleErrorLayer to make route_layer happy.
    let unhandled_error = HandleErrorLayer::new(|err: BoxError| async move {
        (
            StatusCode::TOO_MANY_REQUESTS,
            format!("Too many requests: {err}"),
        )
    });

    let state: RateLimitState = Arc::new(RwLock::new(HashMap::new()));
    let post_vault_ratelimited = post(always_200).route_layer(
        ServiceBuilder::new()
            .layer(unhandled_error.clone())
            .layer(TokenRateLimitLayer::new(state.clone(), POST_LIMIT, minute)),
    );

    let get_vault_ratelimited = get(always_200).route_layer(
        ServiceBuilder::new()
            .layer(unhandled_error.clone())
            .layer(TokenRateLimitLayer::new(state.clone(), GET_LIMIT, minute)),
    );

    let put_vault_id_ratelimited = put(always_200).route_layer(
        ServiceBuilder::new()
            .layer(unhandled_error.clone())
            .layer(TokenRateLimitLayer::new(state.clone(), PUT_LIMIT, minute)),
    );

    let app = Router::new()
        .route("/vault", post_vault_ratelimited)
        .route("/vault", get_vault_ratelimited)
        .route("/vault/:id", put_vault_id_ratelimited);

    println!("Listening on localhost:3000");
    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}
