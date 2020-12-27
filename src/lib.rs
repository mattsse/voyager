use crate::robots::{RobotsData, RobotsHandler};
use anyhow::Result;
use futures::stream::Stream;
use futures::{FutureExt, TryFutureExt};
use reqwest::header::HeaderMap;
use reqwest::{IntoUrl, StatusCode, Url};
use scraper::Html;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
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
    pub fn new(scraper: T, config: CrawlerConfig) -> Self {
        Self {
            crawler: Crawler::new(config),
            scraper,
        }
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

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        loop {
            match pin.crawler.poll(cx) {
                Poll::Ready(Some(result)) => match result {
                    CrawlResult::Finished(Ok(output)) => return Poll::Ready(Some(Ok(output))),
                    CrawlResult::Finished(Err(err)) => return Poll::Ready(Some(Err(err))),
                    CrawlResult::Crawled(Ok(response)) => {
                        // make sure the crawler knows the depth
                        pin.crawler.current_depth = response.depth;
                        pin.crawler.stats.response_count =
                            pin.crawler.stats.response_count.wrapping_add(1);

                        let output = pin.scraper.scrape(response, &mut pin.crawler);

                        pin.crawler.current_depth = 0;

                        match output {
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
    request_queue: VecDeque<QueuedRequest<T::State>>,
    /// Buffer for requests that wait until robots txt is finished
    buffered_until_robots: VecDeque<BufferedRequest<T::State>>,
    /// The client that issues all the requests
    client: Arc<reqwest::Client>,
    /// Domain whitelist, if empty any domains are allowed to visit
    allowed_domains: DomainMap<T::State>,
    /// Used to iterate consistently over the domains
    allowed_domains_keys: Vec<String>,
    /// Domain blacklist
    disallowed_domains: HashSet<String>,
    /// parsed robots.txt infos hashed by domain
    robots_map: HashSet<RobotsData>,
    /// Currently in progress robot txt crawl request
    in_progress_robots_txt_crawl_hosts: HashMap<String, String>,
    /// used to track the depth of submitted requests
    current_depth: usize,
    /// The configured delay to use when no specific delay is configured for a
    /// domain
    request_delay: Option<RequestDelay>,
    /// Stats about requests
    stats: Stats,
    /// Number of concurrent requests
    max_requests: usize,
    /// The maximum depth request are allowed to next
    max_depth: usize,
    /// Respect any restrictions set by the target host's robots.txt file
    respect_robots_txt: bool,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
    /// The filters to apply before issuing requests
    url_filter: Vec<Box<dyn UrlFilter>>,
}

impl<T: Scraper> Crawler<T> {
    pub fn new(config: CrawlerConfig) -> Self {
        let client = config
            .client
            .unwrap_or_else(|| Arc::new(Default::default()));

        let mut allowed_domains = HashMap::with_capacity(config.allowed_domains.len());
        let mut allowed_domains_keys = Vec::with_capacity(config.allowed_domains.len());

        for (domain, delay) in config.allowed_domains {
            allowed_domains_keys.push(domain.clone());
            allowed_domains.insert(
                domain,
                DomainRequestQueue {
                    is_waiting_for_robots: false,
                    in_progress_queue: Default::default(),
                    latest_requested: None,
                    delay,
                },
            );
        }

        Self {
            queued_results: Default::default(),
            in_progress_crawl_requests: Default::default(),
            in_progress_complete_requests: Default::default(),
            in_progress_robots_txt_crawls: Default::default(),
            request_queue: Default::default(),
            buffered_until_robots: Default::default(),
            client,
            allowed_domains,
            allowed_domains_keys,
            disallowed_domains: config.disallowed_domains,
            robots_map: Default::default(),
            in_progress_robots_txt_crawl_hosts: Default::default(),
            current_depth: 0,
            request_delay: None,
            stats: Default::default(),
            max_requests: config.max_depth.unwrap_or(usize::MAX),
            max_depth: config.max_depth.unwrap_or(usize::MAX),
            respect_robots_txt: config.respect_robots_txt,
            skip_non_successful_responses: config.skip_non_successful_responses,
            url_filter: config.url_filter,
        }
    }
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
        let depth = self.current_depth + 1;
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

    fn queue_request(&mut self, request: reqwest::RequestBuilder, state: Option<T::State>) {
        let req = QueuedRequest {
            request,
            state,
            depth: self.current_depth + 1,
        };
        self.request_queue.push_back(req)
    }

    fn is_valid_url(&self, url: &Url) -> bool {
        if self.url_filter.is_empty() {
            true
        } else {
            self.url_filter.iter().any(|filter| filter.is_valid(url))
        }
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

    fn get_response(&self, request: BufferedRequest<T::State>) -> CrawlRequest<T::State> {
        let BufferedRequest {
            request,
            state,
            depth,
        } = request;
        let request_url = request.url().clone();
        let skip_http_error_response = self.skip_non_successful_responses;

        let request = self.client.execute(request);

        Box::pin(async move {
            let mut resp = request.await?;

            if !resp.status().is_success() && skip_http_error_response {
                // skip unsuccessful response
                return Err(CrawlError::NoSuccessResponse {
                    request_url: Some(request_url),
                    response: resp,
                    state,
                }
                .into());
            }

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
        &mut self,
        request: BufferedRequest<T::State>,
        delay: Option<Duration>,
    ) -> CrawlRequest<T::State> {
        let request = self.get_response(request);

        self.stats.request_count = self.stats.request_count.wrapping_add(1);

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
    fn get_robots_txt(&mut self, mut url: Url, host: String, domain: String) {
        self.in_progress_robots_txt_crawl_hosts
            .insert(host.clone(), domain);

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

    fn execute_buffered_request(
        &mut self,
        req: BufferedRequest<T::State>,
        delay: Option<Duration>,
        cx: &mut Context<'_>,
    ) {
        let mut fut = self.execute_request(req, delay);
        if let Poll::Ready(resp) = fut.poll_unpin(cx) {
            self.queued_results.push_back(CrawlResult::Crawled(resp));
        } else {
            self.in_progress_crawl_requests.push(fut);
        }
    }

    fn handle_not_whitelisted_and_not_blacklisted(
        &mut self,
        req: BufferedRequest<T::State>,
        host: String,
        domain: String,
    ) -> Option<(BufferedRequest<T::State>, Option<Duration>)> {
        let mut next_request = None;
        if self.respect_robots_txt {
            if let Some(robots) = self.robots_map.get(host.as_str()) {
                if robots.is_not_disallowed(&req.request) {
                    // not explicitly disallowed
                    next_request = Some((req, self.request_delay().map(|d| d.delay())))
                } else {
                    self.queued_results.push_back(CrawlResult::Crawled(Err(
                        CrawlError::DisallowedRequest {
                            request: req.request,
                            state: req.state,
                            reason: DisallowReason::RobotsTxt,
                        }
                        .into(),
                    )));
                }
            } else if self.in_progress_robots_txt_crawl_hosts.contains_key(&host) {
                self.buffered_until_robots.push_back(req);
            } else {
                // robots txt not requested yet for this
                // request's host
                self.get_robots_txt(req.request.url().clone(), host, domain);
                self.buffered_until_robots.push_back(req);
            }
        } else {
            next_request = Some((req, self.request_delay().map(|d| d.delay())))
        }
        next_request
    }

    fn handle_request_for_whitelisted_domain(
        &mut self,
        mut domain: DomainRequestQueue<T::State>,
        req: BufferedRequest<T::State>,
        key: String,
        host: String,
        now: Instant,
        cx: &mut Context<'_>,
    ) {
        let mut next_request = None;
        // the domain is whitelisted
        if self.respect_robots_txt {
            // check if robots txt was already fetched, or fetching is in
            // progress
            if let Some(robots) = self.robots_map.get(host.as_str()) {
                if robots.is_not_disallowed(&req.request) {
                    // not explicitly disallowed
                    domain.queue_mut().push_back(req);
                    next_request = domain.poll(now);
                } else {
                    self.queued_results.push_back(CrawlResult::Crawled(Err(
                        CrawlError::DisallowedRequest {
                            request: req.request,
                            state: req.state,
                            reason: DisallowReason::RobotsTxt,
                        }
                        .into(),
                    )));
                }
            } else if self.in_progress_robots_txt_crawl_hosts.contains_key(&host) {
                // robots txt request currently in progress
                domain.queue_mut().push_back(req);
            } else {
                // robots txt not requested yet for this
                // request's host
                self.get_robots_txt(req.request.url().clone(), host, key.clone());
                domain.is_waiting_for_robots = true;
                domain.queue_mut().push_back(req);
            }
        } else {
            domain.queue_mut().push_back(req);
            next_request = domain.poll(now);
        }

        // The `next_request` is send, so we track the timestamp to
        // delay any further requests to this domain
        if let Some(req) = next_request {
            self.execute_buffered_request(req, None, cx);
            domain.set_requested_timestamp(now);
        }

        self.allowed_domains.insert(key, domain);
    }

    /// advance all requests
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Option<CrawlResult<T>>> {
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
                            if let Some(domain) = self
                                .in_progress_robots_txt_crawl_hosts
                                .remove(&robots.host)
                                .and_then(|domain| self.allowed_domains.get_mut(&domain))
                            {
                                domain.is_waiting_for_robots = false;
                            }
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

            for _ in 0..self.buffered_until_robots.len() {
                if let Some(req) = self.buffered_until_robots.pop_front() {
                    if req
                        .request
                        .url()
                        .host_str()
                        .map(|host| self.in_progress_robots_txt_crawl_hosts.contains_key(host))
                        .unwrap_or(true)
                    {
                        let fut = self.execute_request(req, None);
                        self.in_progress_crawl_requests.push(fut);
                    } else {
                        self.buffered_until_robots.push_back(req)
                    }
                }
            }

            // drain whitelist
            for n in (0..self.allowed_domains_keys.len()).rev() {
                let domain_id = self.allowed_domains_keys.swap_remove(n);
                if let Some((key, mut domain)) = self.allowed_domains.remove_entry(&domain_id) {
                    if !domain.is_waiting_for_robots {
                        while self.in_progress_crawl_requests.len() < self.max_requests {
                            let now = Instant::now();
                            if let Some(req) = domain.poll(now) {
                                self.execute_buffered_request(req, None, cx);
                                domain.set_requested_timestamp(now);
                            } else {
                                break;
                            }
                        }
                    }
                    self.allowed_domains.insert(key, domain);
                }
                self.allowed_domains_keys.push(domain_id);
            }

            // queue in pending request
            while self.in_progress_crawl_requests.len() < self.max_requests {
                let now = Instant::now();

                if let Some(QueuedRequest {
                    request,
                    state,
                    depth,
                }) = self.request_queue.pop_front()
                {
                    // check that we did not reach max depth
                    if depth > self.max_depth {
                        self.queued_results.push_back(CrawlResult::Crawled(Err(
                            CrawlError::ReachedMaxDepth {
                                request,
                                state,
                                depth,
                            }
                            .into(),
                        )));
                        continue;
                    }

                    match request.build() {
                        Ok(request) => {
                            if !self.is_valid_url(request.url()) {
                                // apply urlfilter
                                self.queued_results.push_back(CrawlResult::Crawled(Err(
                                    CrawlError::DisallowedRequest {
                                        request: request,
                                        state: state,
                                        reason: DisallowReason::User,
                                    }
                                    .into(),
                                )));
                                continue;
                            }

                            let req = BufferedRequest {
                                request,
                                state,
                                depth,
                            };
                            let mut next_request = None;
                            if let Some((domain, host)) = req.request.url().domain().and_then(|d| {
                                req.request
                                    .url()
                                    .host_str()
                                    .map(|h| (d.to_lowercase(), h.to_lowercase()))
                            }) {
                                if let Some((key, domain)) =
                                    self.allowed_domains.remove_entry(&domain)
                                {
                                    self.handle_request_for_whitelisted_domain(
                                        domain,
                                        req,
                                        key,
                                        host.clone(),
                                        now,
                                        cx,
                                    );
                                } else if self.allowed_domains.is_empty() {
                                    // no domains whitelisted, all domains that are not blacklisted
                                    // are allowed
                                    if self.disallowed_domains.contains(&domain) {
                                        // domain is blacklisted
                                        self.queued_results.push_back(CrawlResult::Crawled(Err(
                                            CrawlError::DisallowedRequest {
                                                request: req.request,
                                                state: req.state,
                                                reason: DisallowReason::User,
                                            }
                                            .into(),
                                        )));
                                    } else {
                                        // whitelist empty and domain not on
                                        // blacklist
                                        next_request = self
                                            .handle_not_whitelisted_and_not_blacklisted(
                                                req, host, domain,
                                            );
                                    }
                                } else {
                                    // the domain is not whitelisted and the request therefore
                                    // rejected
                                    self.queued_results.push_back(CrawlResult::Crawled(Err(
                                        CrawlError::DisallowedRequest {
                                            request: req.request,
                                            state: req.state,
                                            reason: DisallowReason::User,
                                        }
                                        .into(),
                                    )));
                                }
                            } else {
                                self.queued_results.push_back(CrawlResult::Crawled(Err(
                                    CrawlError::InvalidRequest {
                                        request: req.request,
                                        state: req.state,
                                    }
                                    .into(),
                                )));
                            }

                            // queue in the next request
                            if let Some((req, delay)) = next_request {
                                self.execute_buffered_request(req, delay, cx);
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
                    // queue emptied
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

struct QueuedRequest<T> {
    request: reqwest::RequestBuilder,
    state: Option<T>,
    depth: usize,
}

struct BufferedRequest<T> {
    request: reqwest::Request,
    state: Option<T>,
    depth: usize,
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
}

pub trait UrlFilter {
    /// Checks whether the url is valid and should be requested
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
    /// number of received successful responses
    pub response_count: usize,
}

/// Configure a `Collector` and its `Crawler`
pub struct CrawlerConfig {
    /// Limits the recursion depth of visited URLs.
    max_depth: Option<usize>,
    /// Limits request to execute concurrently.
    max_requests: Option<usize>,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
    /// Domain whitelist, if empty any domains are allowed to visit
    allowed_domains: HashMap<String, Option<RequestDelay>>,
    /// Domain blacklist
    disallowed_domains: HashSet<String>,
    /// respects the any restrictions set by the target host's
    /// robots.txt file. See (http://www.robotstxt.org/)[http://www.robotstxt.org/] for more information.
    respect_robots_txt: bool,
    // /// Delay a request
    // request_delay: Option<RequestDelay>,
    /// The client that will be used to send the requests
    client: Option<Arc<reqwest::Client>>,
    /// The filters to apply before issuing requests
    url_filter: Vec<Box<dyn UrlFilter>>,
}

impl Default for CrawlerConfig {
    fn default() -> Self {
        Self {
            max_depth: None,
            max_requests: None,
            skip_non_successful_responses: true,
            allowed_domains: Default::default(),
            disallowed_domains: Default::default(),
            respect_robots_txt: false,
            // request_delay: None,
            client: None,
            url_filter: Default::default(),
        }
    }
}

impl CrawlerConfig {
    pub fn max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = Some(max_depth);
        self
    }

    pub fn max_requests(mut self, max_requests: usize) -> Self {
        self.max_requests = Some(max_requests);
        self
    }

    pub fn respect_robots_txt(mut self) -> Self {
        self.respect_robots_txt = true;
        self
    }

    pub fn scrape_non_success_response(mut self) -> Self {
        self.skip_non_successful_responses = false;
        self
    }

    pub fn set_client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(Arc::new(client));
        self
    }

    pub fn with_shared_client(mut self, client: Arc<reqwest::Client>) -> Self {
        self.client = Some(client);
        self
    }

    pub fn disallow_domain(mut self, domain: impl Into<String>) -> Self {
        self.disallowed_domains.insert(domain.into());
        self
    }

    pub fn disallow_domains<I, T>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        for domain in domains.into_iter() {
            self.disallowed_domains.insert(domain.into());
        }
        self
    }

    pub fn allow_domain_with_delay(
        mut self,
        domain: impl Into<String>,
        delay: RequestDelay,
    ) -> Self {
        self.allowed_domains.insert(domain.into(), Some(delay));
        self
    }

    pub fn allow_domain(mut self, domain: impl Into<String>) -> Self {
        self.allowed_domains.insert(domain.into(), None);
        self
    }

    pub fn allow_domains<I, T>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        for domain in domains.into_iter() {
            self.allowed_domains.insert(domain.into(), None);
        }
        self
    }

    pub fn allow_domains_with_delay<I, T>(mut self, domains: I) -> Self
    where
        I: IntoIterator<Item = (T, RequestDelay)>,
        T: Into<String>,
    {
        for (domain, delay) in domains.into_iter() {
            self.allowed_domains.insert(domain.into(), Some(delay));
        }
        self
    }

    pub fn url_filter(mut self, filter: impl UrlFilter + 'static) -> Self {
        self.url_filter.push(Box::new(filter));
        self
    }

    pub fn url_filters<I, T>(mut self, filters: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: UrlFilter + 'static,
    {
        for filter in filters.into_iter() {
            self.url_filter.push(Box::new(filter));
        }
        self
    }
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

/// A set of whitelisted domains
pub type DomainMap<T> = HashMap<String, DomainRequestQueue<T>>;

pub struct DomainRequestQueue<T> {
    /// If true, the robots.txt is currently fetched and waited on
    is_waiting_for_robots: bool,
    /// Currently queued requests to this domain
    in_progress_queue: VecDeque<BufferedRequest<T>>,
    /// When the latest request was sent
    latest_requested: Option<Instant>,
    /// the delay to prior to requests
    delay: Option<RequestDelay>,
}

impl<T> DomainRequestQueue<T> {
    pub fn delay(&self) -> Option<&RequestDelay> {
        self.delay.as_ref()
    }

    fn queue_mut(&mut self) -> &mut VecDeque<BufferedRequest<T>> {
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

    fn poll(&mut self, now: Instant) -> Option<BufferedRequest<T>> {
        if self.is_waiting_for_robots {
            return None;
        }
        if let Some(delay) = self.delay().map(|d| d.delay()) {
            if let Some(last) = self.latest_requested {
                let ready_in = last + delay;
                if ready_in > now {
                    return self.in_progress_queue.pop_front();
                }
            }
            None
        } else {
            self.in_progress_queue.pop_front()
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
    #[error("Reached max depth at {} while carrying state: {:?}", .depth ,.state)]
    ReachedMaxDepth {
        request: reqwest::RequestBuilder,
        state: Option<T>,
        depth: usize,
    },
    #[error("Failed to fetch robots.txt from {}", .host)]
    RobotsTxtError { host: String },
    #[error("Rejected a request, because its url is disallowed due to {}, while carrying state: {:?}", .reason, .state)]
    DisallowedRequest {
        reason: DisallowReason,
        request: reqwest::Request,
        state: Option<T>,
    },
}

impl<T: fmt::Debug> CrawlError<T> {
    /// Get the state this error is carrying
    pub fn state(&self) -> Option<&T> {
        match self {
            CrawlError::NoSuccessResponse { state, .. } => state.as_ref(),
            CrawlError::FailedToBuildRequest { state, .. } => state.as_ref(),
            CrawlError::InvalidRequest { state, .. } => state.as_ref(),
            CrawlError::ReachedMaxDepth { state, .. } => state.as_ref(),
            CrawlError::RobotsTxtError { .. } => None,
            CrawlError::DisallowedRequest { state, .. } => state.as_ref(),
        }
    }

    /// Recover the state this error may carrying
    pub fn into_state(self) -> Option<T> {
        match self {
            CrawlError::NoSuccessResponse { state, .. } => state,
            CrawlError::FailedToBuildRequest { state, .. } => state,
            CrawlError::InvalidRequest { state, .. } => state,
            CrawlError::ReachedMaxDepth { state, .. } => state,
            CrawlError::RobotsTxtError { .. } => None,
            CrawlError::DisallowedRequest { state, .. } => state,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DisallowReason {
    /// Request disallowed by respecting robots.txt
    RobotsTxt,
    /// Request disallowed by user config
    User,
}

impl fmt::Display for DisallowReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisallowReason::RobotsTxt => {
                write!(f, "URL blocked by robots.txt")
            }
            DisallowReason::User => {
                write!(f, "URL blocked by user config")
            }
        }
    }
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
