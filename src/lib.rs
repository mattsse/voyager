use crate::robots::{RobotsData, RobotsHandler};
use anyhow::Result;
use futures::stream::Stream;
use futures::{FutureExt, TryFutureExt};
use reqwest::header::HeaderMap;
use reqwest::{IntoUrl, Request, StatusCode, Url};
use scraper::Html;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as StdContext, Poll};
use std::time::{Duration, Instant};
use thiserror::Error;

/// Contains the robots txt types
pub mod robots;
/// Reexport all the scraper types
pub use scraper;

/// Collector controls the `Crawler` and forwards the successful requests to the
/// `Scraper`.
#[must_use = "Collector does nothing until polled."]
pub struct Collector<T: Scraper> {
    /// The crawler that requests all the pages
    crawler: Crawler<T>,
    /// The scraper that extracts the information from a `Response`
    pub scraper: T,
}

impl<T: Scraper> Collector<T> {
    /// Create a new `Collector` that uses the `scraper` for content extraction
    pub fn new(scraper: T, config: CollectorConfig) -> Result<Self> {
        // do all head requests etc
        todo!()
    }

    /// The scraper of this collector
    pub fn scraper(&self) -> &T {
        &self.scraper
    }

    /// Mutable access to the inner scraper
    pub fn scraper_mut(&mut self) -> &mut T {
        &mut self.scraper
    }

    /// The crawler that handles the requests
    pub fn crawler(&self) -> &Crawler<T> {
        &self.crawler
    }

    /// Mutable access to the crawler that handles the requests
    pub fn crawler_mut(&mut self) -> &mut Crawler<T> {
        &mut self.crawler
    }

    /// Stats about the executed requests
    pub fn stats(&self) -> &Stats {
        &self.crawler.stats
    }
}

impl<T> Stream for Collector<T>
where
    T: Scraper + Unpin + 'static,
    <T as Scraper>::State: Unpin + Send + Sync + fmt::Debug + 'static,
    <T as Scraper>::Output: Unpin,
{
    type Item = Result<T::Output>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        loop {
            match pin.crawler.poll(cx) {
                Poll::Ready(Some(result)) => match result {
                    CrawlResult::Finished(Ok(output)) => return Poll::Ready(Some(Ok(output))),
                    CrawlResult::Finished(Err(err)) => return Poll::Ready(Some(Err(err))),
                    CrawlResult::Crawled(Ok(response)) => {
                        // TODO set depth
                        match pin.scraper.scrape(response, &mut pin.crawler) {
                            Ok(Some(output)) => return Poll::Ready(Some(Ok(output))),
                            Err(err) => return Poll::Ready(Some(Err(err))),
                            _ => {}
                        }
                    }
                    CrawlResult::Crawled(Err(err)) => return Poll::Ready(Some(Err(err))),
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

type CrawlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>>>>>;

/// The crawler that is responsible for driving the requests to completion and
/// providing the crawl response for the `Scraper`.
pub struct Crawler<T: Scraper> {
    /// All queued results that are gathered from all the different futures
    ///
    /// polling them all consecutively and buffering all the results prevents
    /// bias
    queued_results: VecDeque<CrawlResult<T>>,
    /// Futures that eventually return a http response that is passed to the
    /// scraper
    in_progress_crawl_requests: Vec<CrawlRequest<T::State>>,
    /// Futures that eventually result in `T::Output` and are piped directly to
    /// caller
    in_progress_complete_requests: Vec<Pin<Box<dyn Future<Output = Result<Option<T::Output>>>>>>,
    /// stores the futures that request the robots txt for a host
    in_progress_robots_txt_crawls: Vec<Pin<Box<dyn Future<Output = Result<RobotsData, String>>>>>,
    /// Requests that are currently waiting to be executed
    request_queue: VecDeque<(reqwest::RequestBuilder, Option<T::State>)>,
    /// Buffer for requests that wait until robots txt is finished
    buffered_until_robots: VecDeque<(reqwest::Request, Option<T::State>)>,
    /// The timestamp the latest request was send, polled for the first time
    latest_request_send: Instant,
    /// The client that issues all the requests
    client: Arc<reqwest::Client>,
    /// Domain whitelist, if empty any domains are allowed to visit
    allowed_domains: DomainDelayMap<T::State>,
    /// Domain blacklist
    disallowed_domains: HashSet<String>,
    /// parsed robots.txt infos hashed by domain
    robots_map: HashSet<RobotsData>,
    /// Currently in progress robot txt crawl request
    in_progress_robots_txt_crawl_hosts: HashSet<String>,
    /// used to track the depth of submitted requests
    current_depth: usize,
    /// The configured delay to use when no specific delay is configured for a
    /// domain
    request_delay: Option<RequestDelay>,
    /// Stats about requests
    stats: Stats,
    /// Number of concurrent requests
    max_request: usize,
    /// Respect any restrictions set by the target host's robots.txt file
    respect_robots_txt: bool,
}

impl<T> Crawler<T>
where
    T: Scraper + Unpin + 'static,
    <T as Scraper>::State: Unpin + Send + Sync + fmt::Debug + 'static,
    <T as Scraper>::Output: Unpin,
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

        self.in_progress_crawl_requests.push(fut)
    }

    /// Submit a complete crawling job that is driven to completion and directly
    /// returned once finished.
    pub fn complete<TCrawlFunction, TCrawlFuture>(&mut self, fun: TCrawlFunction)
    where
        TCrawlFunction: FnOnce(Arc<reqwest::Client>) -> TCrawlFuture,
        TCrawlFuture: Future<Output = Result<Option<T::Output>>> + 'static,
    {
        let fut = (fun)(Arc::clone(&self.client));
        self.in_progress_complete_requests.push(Box::pin(fut))
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
    pub fn request_delay(&self) -> Option<&RequestDelay> {
        self.request_delay.as_ref()
    }

    /// The configured delay for requests
    pub fn request_delay_mut(&mut self) -> Option<&mut RequestDelay> {
        self.request_delay.as_mut()
    }

    // /// If configured, the configured delays for specific domains
    // pub fn allowed_domains(&self) -> impl Iterator<Item = &String> + '_ {
    //     &self.allowed_domains.values()
    // }

    /// The duration how long to wait until a new request can be sent.
    pub fn remaining_delay(&self, domain: &str, now: Instant) -> Option<Duration> {
        if let Some(domain_delay) = self.allowed_domains.get(domain) {
            return domain_delay.next_delay(now);
        }
        self.request_delay().map(|d| d.delay())
    }

    fn get_response(
        &self,
        request: reqwest::Request,
        state: Option<T::State>,
    ) -> CrawlRequest<T::State> {
        let depth = self.current_depth + 1;
        let request_url = request.url().clone();

        let request = self.client.execute(request);

        Box::pin(async move {
            let mut resp = request.await?;
            let (status, url, headers) = response_info(&mut resp);
            let text = resp.text().await?;

            Ok(Response {
                depth,
                request_url,
                response_url: url,
                response_status: status,
                response_headers: headers,
                text,
                state,
            })
        })
    }

    /// Create a future that executes the request
    ///
    /// If a delay was set, the request will wait for
    fn execute_request(
        &self,
        request: reqwest::Request,
        state: Option<T::State>,
        delay: Option<Duration>,
    ) -> CrawlRequest<T::State> {
        let request = self.get_response(request, state);

        if let Some(delay) = delay {
            Box::pin(async move {
                futures_timer::Delay::new(delay).await;
                Ok(request.await?)
            })
        } else {
            request
        }
    }

    /// queue in a request to fetch the robots txt from the url's host
    fn get_robots_txt(&mut self, mut url: Url, host: String) {
        self.in_progress_robots_txt_crawl_hosts.insert(host.clone());

        url.set_path("robots.txt");
        let err = host.clone();
        let fut = self
            .client
            .get(url)
            .send()
            .map_err(move |_| err)
            .and_then(|resp| RobotsHandler::from_response(resp).map_err(move |_| host));

        self.in_progress_robots_txt_crawls.push(Box::pin(fut));
    }

    fn handle_not_whitelisted_and_not_blacklisted(
        &mut self,
        request: Request,
        state: Option<T::State>,
        host: String,
    ) -> Option<(reqwest::Request, Option<T::State>, Option<Duration>)> {
        let mut next_request = None;
        if self.respect_robots_txt {
            if let Some(robots) = self.robots_map.get(host.as_str()) {
                if robots.is_not_disallowed(&request) {
                    // not explicitly disallowed
                    next_request = Some((request, state, self.request_delay().map(|d| d.delay())))
                } else {
                    self.queued_results.push_back(CrawlResult::Crawled(Err(
                        CrawlError::DisallowedRequest {
                            request,
                            state,
                            reason: DisallowReason::RobotsTxt,
                        }
                        .into(),
                    )));
                }
            } else if self.in_progress_robots_txt_crawl_hosts.contains(&host) {
                self.buffered_until_robots.push_back((request, state));
            } else {
                // robots txt not requested yet for this
                // request's host
                self.get_robots_txt(request.url().clone(), host);
                self.buffered_until_robots.push_back((request, state));
            }
        } else {
            next_request = Some((request, state, self.request_delay().map(|d| d.delay())))
        }
        next_request
    }

    fn handle_request_for_whitelisted_domain(
        &mut self,
        mut domain: DomainDelay<T::State>,
        request: Request,
        state: Option<T::State>,
        key: String,
        host: String,
        now: Instant,
    ) -> Option<(reqwest::Request, Option<T::State>, Option<Duration>)> {
        let mut next_request = None;
        // the domain is whitelisted
        if self.respect_robots_txt {
            // check if robots txt was already fetched, or fetching is in
            // progress
            if let Some(robots) = self.robots_map.get(host.as_str()) {
                if robots.is_not_disallowed(&request) {
                    // not explicitly disallowed
                    next_request = Some(domain.queue_or_pop_front(request, state, now));
                } else {
                    self.queued_results.push_back(CrawlResult::Crawled(Err(
                        CrawlError::DisallowedRequest {
                            request,
                            state,
                            reason: DisallowReason::RobotsTxt,
                        }
                        .into(),
                    )));
                }
            } else if self.in_progress_robots_txt_crawl_hosts.contains(&host) {
                // robots txt request currently in progress
                domain.queue_mut().push_back((request, state));
            } else {
                // robots txt not requested yet for this
                // request's host
                self.get_robots_txt(request.url().clone(), host);
                domain.queue_mut().push_back((request, state));
            }
        } else {
            next_request = Some(domain.queue_or_pop_front(request, state, now));
        }

        // The `next_request` will be send immediately, so we track the timestamp to
        // delay any further requests to this domain
        if next_request.is_some() {
            domain.set_requested_timestamp(now);
        }

        self.allowed_domains.insert(key, domain);
        next_request
    }

    /// advance all requests
    fn poll(&mut self, cx: &mut StdContext<'_>) -> Poll<Option<CrawlResult<T>>> {
        loop {
            // drain all queued results first
            if let Some(result) = self.queued_results.pop_front() {
                return Poll::Ready(Some(result));
            }

            // drain submitted futures by removing them one by one and add them back if not
            // ready
            for n in (0..self.in_progress_complete_requests.len()).rev() {
                let mut request = self.in_progress_complete_requests.swap_remove(n);
                if let Poll::Ready(resp) = request.poll_unpin(cx) {
                    match resp {
                        Ok(Some(output)) => {
                            self.queued_results
                                .push_back(CrawlResult::Finished(Ok(output)));
                        }
                        Err(err) => {
                            self.queued_results
                                .push_back(CrawlResult::Finished(Err(err)));
                        }
                        _ => {}
                    }
                } else {
                    self.in_progress_complete_requests.push(request);
                }
            }

            // drain the crawl futures by removing them one by one and add them back if not
            // ready
            for n in (0..self.in_progress_crawl_requests.len()).rev() {
                let mut request = self.in_progress_crawl_requests.swap_remove(n);
                if let Poll::Ready(resp) = request.poll_unpin(cx) {
                    self.queued_results.push_back(CrawlResult::Crawled(resp));
                }
                self.in_progress_crawl_requests.push(request);
            }

            // drain all in progress robots.txt lookups
            for n in (0..self.in_progress_robots_txt_crawls.len()).rev() {
                let mut request = self.in_progress_robots_txt_crawls.swap_remove(n);
                if let Poll::Ready(resp) = request.poll_unpin(cx) {
                    match resp {
                        Ok(robots) => {
                            self.in_progress_robots_txt_crawl_hosts.remove(&robots.host);
                            self.robots_map.insert(robots);
                        }
                        Err(host) => {
                            self.in_progress_robots_txt_crawl_hosts.remove(&host);
                            self.queued_results.push_back(CrawlResult::Crawled(Err(
                                CrawlError::<T::State>::RobotsTxtError { host }.into(),
                            )));
                        }
                    }
                } else {
                    self.in_progress_robots_txt_crawls.push(request);
                }
            }

            // queue in pending request
            while self.in_progress_crawl_requests.len() < self.max_request {
                let now = Instant::now();
                if let Some((request, state)) = self.request_queue.pop_front() {
                    match request.build() {
                        Ok(request) => {
                            let mut next_request = None;
                            let url = request.url();
                            if let Some((domain, host)) = url.domain().and_then(|d| {
                                url.host_str().map(|h| (d.to_lowercase(), h.to_lowercase()))
                            }) {
                                if let Some((key, domain)) =
                                    self.allowed_domains.remove_entry(&domain)
                                {
                                    next_request = self.handle_request_for_whitelisted_domain(
                                        domain,
                                        request,
                                        state,
                                        key,
                                        host.clone(),
                                        now,
                                    );
                                } else if self.allowed_domains.is_empty() {
                                    // no domains whitelisted, all domains that are not blacklisted
                                    // are allowed
                                    if self.disallowed_domains.contains(&domain) {
                                        // domain is blacklisted
                                        self.queued_results.push_back(CrawlResult::Crawled(Err(
                                            CrawlError::DisallowedRequest {
                                                request,
                                                state,
                                                reason: DisallowReason::User,
                                            }
                                            .into(),
                                        )));
                                    } else {
                                        // whitelist empty and domain not on
                                        // blacklist
                                        next_request = self
                                            .handle_not_whitelisted_and_not_blacklisted(
                                                request, state, host,
                                            );
                                    }
                                } else {
                                    // the domain is not whitelisted and the request therefore
                                    // rejected
                                    self.queued_results.push_back(CrawlResult::Crawled(Err(
                                        CrawlError::DisallowedRequest {
                                            request,
                                            state,
                                            reason: DisallowReason::User,
                                        }
                                        .into(),
                                    )));
                                }
                            } else {
                                self.queued_results.push_back(CrawlResult::Crawled(Err(
                                    CrawlError::InvalidRequest { request, state }.into(),
                                )));
                            }

                            // queue in the next request
                            if let Some((request, state, delay)) = next_request {
                                let mut fut = self.execute_request(request, state, delay);
                                if let Poll::Ready(resp) = fut.poll_unpin(cx) {
                                    self.queued_results.push_back(CrawlResult::Crawled(resp));
                                } else {
                                    self.in_progress_crawl_requests.push(fut);
                                }
                            }
                        }
                        Err(error) => {
                            // failed to create a request
                            self.queued_results.push_back(CrawlResult::Crawled(Err(
                                CrawlError::FailedToBuildRequest { error, state }.into(),
                            )));
                        }
                    }
                } else {
                    break;
                }
            }
            // If no new results have been queued either, signal `NotReady` to
            // be polled again later.
            if self.queued_results.is_empty() {
                return Poll::Pending;
            }
        }
    }
}

/// The result type a `Crawler` produces
enum CrawlResult<T: Scraper> {
    /// A submitted request to produce the `Scraper::Output` type has finished
    Finished(Result<T::Output>),
    /// A page was crawled
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
    ) -> Result<Option<Self::Output>>;

    /// This checks whether a submitted url should in fact be requested
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

/// Configure a `Collector` and its `Crawler`
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Limits the recursion depth of visited URLs.
    max_depth: Option<usize>,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_http_error_response: bool,
    /// respects the any restrictions set by the target host's
    /// robots.txt file. See (http://www.robotstxt.org/)[http://www.robotstxt.org/] for more information.
    respect_robots_txt: bool,
    /// Delay a request
    request_delay: Option<RequestDelay>,
}

/// How to delay a request
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
pub type DomainDelayMap<T> = HashMap<String, DomainDelay<T>>;

pub struct DomainDelay<T> {
    /// Currently queued requests to this domain
    in_progress_queue: VecDeque<(reqwest::Request, Option<T>)>,
    /// When the lastest request was sent
    latest_requested: Option<Instant>,
    /// the delay to prior to requests
    delay: Option<RequestDelay>,
}

impl<T> DomainDelay<T> {
    pub fn delay(&self) -> Option<&RequestDelay> {
        self.delay.as_ref()
    }

    fn queue_mut(&mut self) -> &mut VecDeque<(reqwest::Request, Option<T>)> {
        &mut self.in_progress_queue
    }

    pub fn delay_mut(&mut self) -> Option<&mut RequestDelay> {
        self.delay.as_mut()
    }

    /// How long to wait until a new request is allowed
    pub fn next_delay(&self, now: Instant) -> Option<Duration> {
        if let Some(dur) = self.delay().map(|d| d.delay()) {
            if let Some(last) = self.latest_requested {
                let ready_in = last + dur;
                if ready_in > now {
                    return Some(ready_in - now);
                }
            }
        }
        None
    }

    /// Stores the timestamp when the domain was last requested
    fn set_requested_timestamp(&mut self, now: Instant) {
        self.latest_requested = Some(now)
    }

    /// Return the request next in line
    pub fn queue_or_pop_front(
        &mut self,
        request: reqwest::Request,
        state: Option<T>,
        now: Instant,
    ) -> (reqwest::Request, Option<T>, Option<Duration>) {
        let delay = self.next_delay(now);
        if let Some((r, s)) = self.in_progress_queue.pop_front() {
            self.in_progress_queue.push_back((request, state));
            (r, s, delay)
        } else {
            (request, state, delay)
        }
    }
}

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
    #[error("Received response with non 2xx status {:?} for {:?} carrying state: {:?}", .response, .request_url, .state)]
    NoSuccessResponse {
        request_url: Option<Url>,
        /// The received response
        response: reqwest::Response,
        /// The attached state to this request, if any
        state: Option<T>,
    },
    #[error("Failed to construct a request: {} while carrying state: {:?}", .error, .state)]
    FailedToBuildRequest {
        error: reqwest::Error,
        state: Option<T>,
    },
    #[error("Failed to process invalid request while carrying state: {:?}", .state)]
    InvalidRequest {
        request: reqwest::Request,
        state: Option<T>,
    },
    #[error("Failed to fetch robots.txt from {}", .host)]
    RobotsTxtError { host: String },
    #[error("Rejected a request, because its url is disallowed due to {:?}, while carrying state: {:?}", .reason, .state)]
    DisallowedRequest {
        reason: DisallowReason,
        request: reqwest::Request,
        state: Option<T>,
    },
}

#[derive(Debug, Clone)]
pub enum DisallowReason {
    /// Request disallowed by respecting robots.txt
    RobotsTxt,
    /// Request disallowed by user config
    User,
}

#[derive(Debug, Copy, Clone, Error)]
#[error("Received response with unexpected status {0}")]
pub struct UnexpectedStatusError(u16);

impl UnexpectedStatusError {
    pub fn new(status: u16) -> Self {
        UnexpectedStatusError(status)
    }

    pub fn status(&self) -> u16 {
        self.0
    }
}
