use anyhow::Result;
use either::Either;
use futures::stream::Stream;
use futures::FutureExt;
use reqwest::header::HeaderMap;
use reqwest::{IntoUrl, StatusCode, Url};
use scraper::Html;
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as StdContext, Poll};
use std::time::{Duration, Instant};
use thiserror::Error;

pub struct Collector<T: Scraper> {
    crawler: Crawler<T>,
    pub scraper: T,
}

impl<T: Scraper> Collector<T> {
    pub fn scraper(&self) -> &T {
        &self.scraper
    }

    pub fn scraper_mut(&mut self) -> &mut T {
        &mut self.scraper
    }

    pub fn crawler(&self) -> &Crawler<T> {
        &self.crawler
    }

    pub fn crawler_mut(&mut self) -> &mut Crawler<T> {
        &mut self.crawler
    }

    pub fn stats(&self) -> &Stats {
        &self.crawler.stats
    }
}

impl<T> Stream for Collector<T>
where
    T: Scraper + Unpin + 'static,
    <T as Scraper>::State: Unpin + fmt::Debug + 'static,
{
    type Item = Result<T::Output>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        let r: Response<T::State> = Response {
            depth: 0,
            request_url: "".parse().unwrap(),
            response_url: "".parse().unwrap(),
            response_status: Default::default(),
            response_headers: Default::default(),
            text: "".to_string(),
            state: None,
        };

        // pin.scraper.scrape(r, &mut pin.crawler);

        Poll::Pending
    }
}

type CrawlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>>>>>;

/// The crawler that is responsible for driving the requests to completion and
/// providing the crawl response for the `Scraper`.
pub struct Crawler<T: Scraper> {
    /// Futures that eventually return a http response that is passed to the
    /// scraper
    crawl_requests: Vec<CrawlRequest<T::State>>,
    /// Futures that eventually result in `T::Output` and are piped directly to
    /// caller
    complete_requests: Vec<Pin<Box<dyn Future<Output = Result<Option<T::Output>>> + 'static>>>,
    /// Requests that are currently waiting to be executed
    request_queue: VecDeque<(reqwest::RequestBuilder, Option<T::State>)>,
    /// The timestamp the latest request was send, polled for the first time
    latest_request_send: Instant,
    /// The client that issues all the requests
    client: Arc<reqwest::Client>,
    /// Domain whitelist, if empty any domains are allowed to visit
    allowed_domains: Vec<String>,
    /// Domain blacklist
    disallowed_domains: HashSet<String>,
    /// used to track the depth of submitted requests
    current_depth: usize,
    /// The configured delays for domains
    ///
    /// Either:
    ///  * apply no delay for `None`,
    ///  * apply a delay to every request for `Some(Either::Left)`,
    ///  * apply a delay only to requests for specific domains
    ///    `Some(Either::Right)`
    request_delay: Option<Either<RequestDelay, DomainDelayMap>>,
    /// Stats about requests
    stats: Stats,
    /// Number of concurrent requests
    max_request: usize,
}

impl<T> Crawler<T>
where
    T: Scraper + Unpin + 'static,
    <T as Scraper>::State: Unpin + fmt::Debug + 'static,
{
    /// Send an crawling request whose html response and context is returned to
    /// the scraper again
    pub fn crawl<TCrawlFunction, TCrawlFuture>(&mut self, fun: TCrawlFunction)
    where
        TCrawlFunction: FnOnce(Arc<reqwest::Client>) -> TCrawlFuture,
        TCrawlFuture: Future<Output = Result<(reqwest::Response, Option<T::State>)>> + 'static,
    {
        let depth = self.current_depth;
        let fut = (fun)(Arc::clone(&self.client));
        let fut = Box::pin(async move {
            let (mut resp, state) = fut.await?;
            let (status, url, headers) = response_info(&mut resp);
            let text = resp.text().await?;
            let html = Html::parse_document(&text);

            Ok(Response {
                depth,
                // Note: There is no way to determine the original url since only the response is
                // returned from the future So we set the `request_url = response_url`
                request_url: url.clone(),
                response_url: url,
                response_status: status,
                response_headers: headers,
                text,
                state,
            })
        });

        self.crawl_requests.push(fut)
    }

    /// Submit a complete crawling job that is driven to completion and directly
    /// returned once finished.
    pub fn complete<TCrawlFunction, TCrawlFuture>(&mut self, fun: TCrawlFunction)
    where
        TCrawlFunction: FnOnce(Arc<reqwest::Client>) -> TCrawlFuture,
        TCrawlFuture: Future<Output = Result<Option<T::Output>>> + 'static,
    {
        let fut = (fun)(Arc::clone(&self.client));
        self.complete_requests.push(Box::pin(fut))
    }

    /// This queues in a GET request for the `url`, without any state attached
    pub fn visit(&mut self, url: impl IntoUrl) {
        self.request(self.client.request(reqwest::Method::GET, url))
    }

    /// This queues in a GET request for the `url` with state attached
    pub fn visit_with_context(&mut self, url: impl IntoUrl, state: T::State) {
        self.request_with_state(self.client.request(reqwest::Method::GET, url), state)
    }

    /// This queues in a whole request with no state attached
    pub fn request(&mut self, req: reqwest::RequestBuilder) {
        self.queue_request(req, None)
    }

    /// This queues in a whole request with a state attached
    pub fn request_with_state(&mut self, req: reqwest::RequestBuilder, state: T::State) {
        self.queue_request(req, Some(state))
    }

    fn queue_request(&mut self, req: reqwest::RequestBuilder, state: Option<T::State>) {
        self.request_queue.push_back((req, state))
    }

    /// The client that performs all request
    pub fn client(&self) -> &reqwest::Client {
        self.client.as_ref()
    }

    /// The crawler's client is wrapped within an `Arc`, so this can be used to
    /// replace the client
    pub fn client_mut(&mut self) -> &mut Arc<reqwest::Client> {
        &mut self.client
    }

    /// The configured delay for requests
    pub fn request_delay(&self) -> Option<&Either<RequestDelay, DomainDelayMap>> {
        self.request_delay.as_ref()
    }

    /// The configured delay for requests
    pub fn request_delay_mut(&mut self) -> Option<&mut Either<RequestDelay, DomainDelayMap>> {
        self.request_delay.as_mut()
    }

    /// If configured, the configured delays for specific domains
    pub fn domain_delays(&self) -> Option<&DomainDelayMap> {
        if let Some(Either::Right(delays)) = self.request_delay() {
            Some(delays)
        } else {
            None
        }
    }

    /// If configured, the configured delays for specific domains
    pub fn domain_delays_mut(&mut self) -> Option<&mut DomainDelayMap> {
        if let Some(Either::Right(delays)) = self.request_delay_mut() {
            Some(delays)
        } else {
            None
        }
    }

    /// The duration how long to wait until a new request can be sent.
    pub fn remaining_delay(&self, domain: &str, now: Instant) -> Option<Duration> {
        if let Some(delay) = self.request_delay() {
            let mut dur = None;
            match delay {
                Either::Left(delay) => {
                    dur = Some(delay.delay());
                }
                Either::Right(domains) => {
                    todo!("impl timestamp when last request for a domain was sent");
                    dur = domains.get(domain).map(|d| d.delay());
                }
            }
            if let Some(dur) = dur {
                // let ready_in = self.latest_request_send + dur;
                // if ready_in > now {
                //     return Some(ready_in - now);
                // }
            }
        }
        None
    }

    fn poll(&mut self, cx: &mut StdContext<'_>, now: Instant) -> Poll<Option<CrawlEvent<T>>> {
        // drain submitted futures
        for n in (0..self.complete_requests.len()).rev() {
            let mut request = self.complete_requests.swap_remove(n);
            if let Poll::Ready(resp) = request.poll_unpin(cx) {
                match resp {
                    Ok(Some(output)) => return Poll::Ready(Some(CrawlEvent::Finished(Ok(output)))),
                    Err(err) => return Poll::Ready(Some(CrawlEvent::Finished(Err(err)))),
                    _ => {}
                }
            } else {
                self.complete_requests.push(request);
            }
        }

        while self.crawl_requests.len() < self.max_request {}

        // drain all crawl requests
        for n in (0..self.crawl_requests.len()).rev() {
            let mut request = self.crawl_requests.swap_remove(n);
            if let Poll::Ready(resp) = request.poll_unpin(cx) {
                return Poll::Ready(Some(CrawlEvent::Crawled(resp)));
            }
            self.crawl_requests.push(request);
        }

        Poll::Pending
    }
}

enum CrawlEvent<T: Scraper> {
    Finished(Result<T::Output>),
    Crawled(Result<Response<T::State>>),
}

fn response_info(resp: &mut reqwest::Response) -> (StatusCode, Url, HeaderMap) {
    let mut headers = HeaderMap::new();
    std::mem::swap(&mut headers, resp.headers_mut());
    (resp.status(), resp.url().clone(), headers)
}

/// A successful response for an issued request
pub struct Response<T> {
    /// The depth of the request that was issued for this
    pub depth: usize,
    /// The url of the request that was issued.
    pub request_url: Url,
    /// The url of the response as received
    pub response_url: Url,
    /// The status code of the response
    pub response_status: StatusCode,
    /// The headers of the received response
    pub response_headers: HeaderMap,
    /// The full response text.
    pub text: String,
    /// The attached state of the scraper
    pub state: Option<T>,
}

impl<T> Response<T> {
    /// Returns the parsed Html document
    pub fn html(&self) -> Html {
        Html::parse_document(&self.text)
    }
}

/// A trait that is takes in successfully fetched responses, scrapes the
/// valuable content from the responses html document and provides the crawler
/// with additional requests to visit and drive the scraper's model to
/// completion.
pub trait Scraper: Sized {
    /// The type this scraper eventually produces
    type Output;
    /// The type used to track the progress of a scraping task that needs
    /// several consecutive request.
    type State;

    /// Is called after the `Crawler` successfully receives a response for an
    /// issued request.
    ///
    /// This function can return a finished `Output` object.
    /// The crawler accepts additional requests. To advance the scraper's
    /// `State`, pass the state of the `response` along with the request to the
    /// `Crawler`.
    fn scrape(
        &mut self,
        response: Response<Self::State>,
        crawler: &mut Crawler<Self>,
    ) -> Option<Self::Output>;

    /// This checks whether a submitted url should in fact be requested
    fn is_valid_url(&self, url: &Url) -> bool {
        true
    }
}

pub struct Reddit;

impl Scraper for Reddit {
    type Output = String;
    type State = ();

    fn scrape(
        &mut self,
        response: Response<Self::State>,
        crawler: &mut Crawler<Self>,
    ) -> Option<Self::Output> {
        crawler.complete(move |client| async move {
            dbg!(response.state);
            client.get("").send().await?;
            Ok(Some("".to_string()))
        });

        unimplemented!()
    }
}

pub struct UrlValidator {
    allowed_domains: Vec<String>,
    filters: Vec<Box<dyn UrlFilter>>,
    robots_map: Vec<()>,
}

impl UrlValidator {
    fn is_valid_url(&self, url: &Url) -> bool {
        true
    }
}

pub trait UrlFilter {
    fn is_valid(&self, url: &Url) -> bool;
}

impl<F> UrlFilter for F
where
    F: Fn(&Url) -> bool,
{
    fn is_valid(&self, url: &Url) -> bool {
        (self)(url)
    }
}

#[cfg(feature = "re")]
impl UrlFilter for regex::Regex {
    fn is_valid(&self, url: &Url) -> bool {
        self.is_match(url.as_str())
    }
}

/// Stats about sent requests and received responses
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// number of sent requests
    pub request_count: usize,
    /// number of received 2xx responses
    pub success_count: usize,
    /// Failed and non 2xx responses
    pub error_count: usize,
}

#[derive(Debug)]
pub struct CollectorConfig {
    /// Limits the recursion depth of visited URLs.
    max_depth: Option<usize>,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_http_error_response: bool,
    /// allows the Collector to ignore any restrictions set by the target host's
    /// robots.txt file.
    ignore_robots_txt: bool,
    /// Delay a request
    request_delay: Option<RequestDelay>,
}

/// How to delay request
#[derive(Debug, Clone, Copy)]
pub enum RequestDelay {
    /// Apply a fixed delay to request
    Fixed(Duration),
    /// Apply a random delay to a request that is in the range of (`min`..`max`)
    Random {
        /// minimum delay duration to apply
        min: Duration,
        /// maximum delay duration to apply
        max: Duration,
    },
}

/// A set of delays for specific domains
pub type DomainDelayMap = HashMap<String, RequestDelay>;

impl RequestDelay {
    /// Use a fixed delay
    pub fn fixed(delay: Duration) -> Self {
        RequestDelay::Fixed(delay)
    }

    /// Use a random delay of range `0`..max`
    pub fn random(max: Duration) -> Self {
        RequestDelay::Random {
            min: Duration::from_millis(0),
            max,
        }
    }

    /// Use a random delay of range `min`..max`
    pub fn random_in_range(min: Duration, max: Duration) -> Self {
        RequestDelay::Random { min, max }
    }

    pub fn delay(&self) -> Duration {
        use rand::Rng;

        match self {
            RequestDelay::Fixed(delay) => *delay,
            RequestDelay::Random { min, max } => Duration::from_millis(
                rand::thread_rng().gen_range(min.as_millis() as u64..=max.as_millis() as u64),
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum CrawlError<T: fmt::Debug> {
    #[error("Received response with non 2xx status {:?} carrying state {:?}", .response, .state)]
    NoSuccessResponse {
        /// The received response
        response: reqwest::Response,
        /// The attached state to this request, if any
        state: Option<T>,
    },
}
