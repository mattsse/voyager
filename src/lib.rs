//! voyager
//!
//! With voyager you can easily extract structured data from websites.
//!
//! Write your own crawler/crawler with Voyager following a state machine model.
//!
//! # Example
//!
//! ```no_run
//! # use voyager::scraper::Selector;
//! # use reqwest::Url;
//! /// Declare your scraper, with all the selectors etc.
//! struct HackernewsScraper {
//!     post_selector: Selector,
//!     author_selector: Selector,
//!     title_selector: Selector,
//!     comment_selector: Selector,
//!     max_page: usize,
//! }
//!
//! /// The state model
//! #[derive(Debug)]
//! enum HackernewsState {
//!     Page(usize),
//!     Post,
//! }
//!
//! /// The ouput the scraper should eventually produce
//! #[derive(Debug)]
//! struct Entry {
//!     author: String,
//!     url: Url,
//!     link: Option<String>,
//!     title: String,
//! }
//! ```
//!
//! # Implement the `voyager::Scraper` trait
//!
//! A `Scraper` consists of two associated types:
//!
//! * `Output`, the type the scraper eventually produces
//! * `State`, the type, the scraper can drag along several requests that
//!   eventually lead to an `Output`
//!
//! and the `scrape` callback, which is invoked after each received response.
//!
//! Based on the state attached to `response` you can supply the crawler with
//! new urls to visit with, or without a state attached to it.
//!
//! Scraping is done with [causal-agent/scraper](https://github.com/causal-agent/scraper).
//!
//! ```ignore
//! # use voyager::{Scraper, Response, Crawler};
//! impl Scraper for HackernewsScraper {
//!     type Output = Entry;
//!     type State = HackernewsState;
//!
//!     /// do your scraping
//!     fn scrape(
//!         &mut self,
//!         response: Response<Self::State>,
//!         crawler: &mut Crawler<Self>,
//!     ) -> Result<Option<Self::Output>> {
//!         let html = response.html();
//!
//!         if let Some(state) = response.state {
//!             match state {
//!                 HackernewsState::Page(page) => {
//!                     // find all entries
//!                     for id in html
//!                         .select(&self.post_selector)
//!                         .filter_map(|el| el.value().attr("id"))
//!                     {
//!                         // submit an url to a post
//!                         crawler.visit_with_state(
//!                             &format!("https://news.ycombinator.com/item?id={}", id),
//!                             HackernewsState::Post,
//!                         );
//!                     }
//!                     if page < self.max_page {
//!                         // queue in next page
//!                         crawler.visit_with_state(
//!                             &format!("https://news.ycombinator.com/news?p={}", page + 1),
//!                             HackernewsState::Page(page + 1),
//!                         );
//!                     }
//!                 }
//!
//!                 HackernewsState::Post => {
//!                     // scrape the entry
//!                     let entry = Entry {
//!                         // ...
//!                     };
//!                     return Ok(Some(entry));
//!                 }
//!             }
//!         }
//!
//!         Ok(None)
//!     }
//! }
//! ```
//!
//!
//! # Setup and collect all the output
//!
//! Configure the crawler with via `CrawlerConfig`:
//!
//! * Allow/Block list of URLs
//! * Delays between requests
//! * Whether to respect the `Robots.txt` rules
//!
//! Feed your config and an instance of your scraper to the `Collector` that
//! drives the `Crawler` and forwards the responses to your `Scraper`.
//!
//! ```ignore
//! # use voyager::scraper::Selector;
//! # use voyager::*;
//! # use tokio::stream::StreamExt;
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // only fulfill requests to `news.ycombinator.com`
//!     let config = CrawlerConfig::default().allow_domain_with_delay(
//!         "news.ycombinator.com",
//!         // add a delay between requests
//!         RequestDelay::Fixed(std::time::Duration::from_millis(2_000)),
//!     );
//!
//!     let mut collector = Collector::new(HackernewsScraper::default(), config);
//!
//!     collector.crawler_mut().visit_with_state(
//!         "https://news.ycombinator.com/news",
//!         HackernewsState::Page(1),
//!     );
//!
//!     while let Some(output) = collector.next().await {
//!         let post = output?;
//!         dbg!(post);
//!     }
//!
//! #  Ok(())
//! # }
//! ```

use anyhow::Result;
use futures::stream::Stream;
use futures::FutureExt;
use reqwest::IntoUrl;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

mod domain;
pub mod error;
mod requests;
pub mod response;

// /// Contains the robots txt types
pub mod robots;
pub use crate::requests::RequestDelay;
use crate::requests::{response_info, QueuedRequestBuilder};
pub use crate::response::Response;
pub use domain::{AllowList, AllowListConfig, BlockList, DomainListing};
/// Reexport all the scraper types
pub use scraper;

/// Collector controls the `Crawler` and forwards the successful requests to the
/// `Scraper`. and reports the `Scraper`'s `Output` back to the user.
#[must_use = "Collector does nothing until polled."]
pub struct Collector<T: Scraper> {
    /// The crawler that requests all the pages
    crawler: Crawler<T>,
    /// The scraper that extracts the information from a `Response`
    pub scraper: T,
}

impl<T> Collector<T>
where
    T: Scraper,
    <T as Scraper>::State: fmt::Debug,
{
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
    <T as Scraper>::State: Unpin + Send + Sync + 'static,
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

type OutputRequest<T> = Pin<Box<dyn Future<Output = Result<Option<T>>>>>;
type CrawlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>>>>>;
/// The crawler that is responsible for driving the requests to completion and
/// providing the crawl response for the `Scraper`.
pub struct Crawler<T: Scraper> {
    /// Futures that eventually result in `T::Output` and are piped directly to
    /// caller
    in_progress_complete_requests: Vec<OutputRequest<T::Output>>,
    /// All injected futures that create a new state
    in_progress_crawl_requests: Vec<CrawlRequest<T::State>>,
    queued_results: VecDeque<CrawlResult<T>>,
    /// The client that issues all the requests
    client: reqwest_middleware::ClientWithMiddleware,
    /// used to track the depth of submitted requests
    current_depth: usize,
    /// Either a list that only allows a set of domains or disallows a set of
    /// domains
    list: DomainListing<T::State>,
    /// Stats about requests
    stats: Stats,
    /// The maximum depth request are allowed to next
    max_depth: usize,
    /// Respect any restrictions set by the target host's robots.txt file
    respect_robots_txt: bool,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
}

impl<T: Scraper> Crawler<T> {
    /// Create a new crawler following the config
    pub fn new(config: CrawlerConfig) -> Self {
        let client = config
            .client
            .unwrap_or(reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build());

        let list = if config.allowed_domains.is_empty() {
            let block_list = BlockList::new(
                config.disallowed_domains,
                client.clone(),
                config.respect_robots_txt,
                config.skip_non_successful_responses,
                config.max_depth.unwrap_or(usize::MAX),
                config
                    .max_requests
                    .unwrap_or(CrawlerConfig::MAX_CONCURRENT_REQUESTS),
            );
            DomainListing::BlockList(block_list)
        } else {
            let mut allow_list = AllowList::default();
            let max_requests = config
                .max_requests
                .unwrap_or(CrawlerConfig::MAX_CONCURRENT_REQUESTS)
                / config.allowed_domains.len();
            for (domain, delay) in config.allowed_domains {
                let allow = AllowListConfig {
                    delay,
                    respect_robots_txt: config.respect_robots_txt,
                    client: client.clone(),
                    skip_non_successful_responses: config.skip_non_successful_responses,
                    max_depth: config.max_depth.unwrap_or(usize::MAX),
                    max_requests,
                };
                allow_list.allow(domain, allow);
            }
            DomainListing::AllowList(allow_list)
        };

        Self {
            in_progress_complete_requests: Default::default(),
            in_progress_crawl_requests: Default::default(),
            queued_results: Default::default(),
            client,
            current_depth: 0,
            list,
            stats: Default::default(),
            max_depth: config.max_depth.unwrap_or(usize::MAX),
            respect_robots_txt: config.respect_robots_txt,
            skip_non_successful_responses: config.skip_non_successful_responses,
        }
    }

    /// The maximum allowed depth of requests
    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    /// Whether this crawler respects the domains robots.txt rules
    pub fn respects_robots_txt(&self) -> bool {
        self.respect_robots_txt
    }

    /// Whether non 2xx responses are treated as failures and are not being
    /// scraped
    pub fn skips_non_successful_responses(&self) -> bool {
        self.skip_non_successful_responses
    }
}

impl<T> Crawler<T>
where
    T: Scraper + Unpin + 'static,
    <T as Scraper>::State: Unpin + Send + Sync + 'static,
    <T as Scraper>::Output: Unpin,
{
    /// Send a crawling request whose html response and context is returned to
    /// the scraper again
    pub fn crawl<TCrawlFunction, TCrawlFuture>(&mut self, fun: TCrawlFunction)
    where
        TCrawlFunction: FnOnce(&reqwest_middleware::ClientWithMiddleware) -> TCrawlFuture,
        TCrawlFuture: Future<Output = Result<(reqwest::Response, Option<T::State>)>> + 'static,
    {
        let depth = self.current_depth + 1;
        let fut = (fun)(&self.client);
        let fut = Box::pin(async move {
            let (mut resp, state) = fut.await?;
            let (status, url, headers) = response_info(&mut resp);

            // debug
            let dbg_headers = resp.headers();
            dbg!("lib");
            dbg!(&headers);
            dbg!(dbg_headers);

            let text = resp.text().await?;
            dbg!(&text);
            

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
        TCrawlFunction: FnOnce(&reqwest_middleware::ClientWithMiddleware) -> TCrawlFuture,
        TCrawlFuture: Future<Output = Result<Option<T::Output>>> + 'static,
    {
        let fut = (fun)(&self.client);
        self.in_progress_complete_requests.push(Box::pin(fut))
    }

    /// This queues in a GET request for the `url`, without any state attached
    pub fn visit(&mut self, url: impl IntoUrl) {
        self.request(self.client.request(reqwest::Method::GET, url))
    }

    /// This queues in a GET request for the `url` with state attached
    pub fn visit_with_state(&mut self, url: impl IntoUrl, state: T::State) {
        self.request_with_state(self.client.request(reqwest::Method::GET, url), state)
    }

    /// This queues in a whole request with no state attached
    pub fn request(&mut self, req: reqwest_middleware::RequestBuilder) {
        self.queue_request(req, None)
    }

    /// This queues in a whole request with a state attached
    pub fn request_with_state(&mut self, req: reqwest_middleware::RequestBuilder, state: T::State) {
        self.queue_request(req, Some(state))
    }

    fn queue_request(
        &mut self,
        request: reqwest_middleware::RequestBuilder,
        state: Option<T::State>,
    ) {
        let req = QueuedRequestBuilder {
            request,
            state,
            depth: self.current_depth + 1,
        };
        if let Err(err) = self.list.add_request(req) {
            self.queued_results
                .push_back(CrawlResult::Crawled(Err(err.into())))
        }
    }

    /// The client that performs all request
    pub fn client(&self) -> &reqwest_middleware::ClientWithMiddleware {
        &self.client
    }

    /// advance all requests
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Option<CrawlResult<T>>> {
        loop {
            // drain all results
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
                } else {
                    self.in_progress_crawl_requests.push(request);
                }
            }

            let mut busy = false;
            loop {
                match Stream::poll_next(Pin::new(&mut self.list), cx) {
                    Poll::Ready(Some(resp)) => {
                        self.queued_results.push_back(CrawlResult::Crawled(resp));
                    }
                    Poll::Pending => {
                        busy = true;
                        break;
                    }
                    _ => break,
                }
            }

            // If no new results have been queued either, signal `NotReady` or `Done` if all
            // queues are drained
            if self.queued_results.is_empty() {
                if !busy
                    && self.in_progress_crawl_requests.is_empty()
                    && self.in_progress_complete_requests.is_empty()
                {
                    return Poll::Ready(None);
                }
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

/// A trait that is takes in successfully fetched responses, scrapes the
/// valuable content from the responses html document and provides the
/// with additional requests to visit and drive the scraper's model
/// completion.
pub trait Scraper: Sized {
    /// The type this scraper eventually produces
    type Output;
    /// The type used to track the progress of a scraping task that needs
    /// several consecutive request.
    type State: fmt::Debug;

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
    ///
    /// Default is `MAX_CONCURRENT_REQUESTS`
    max_requests: Option<usize>,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
    /// Domain whitelist, if empty any domains are allowed to visit
    allowed_domains: HashMap<String, Option<RequestDelay>>,
    /// Domain blacklist
    disallowed_domains: HashSet<String>,
    /// respects the any restrictions set by the target host's
    /// robots.txt file. See <http://www.robotstxt.org/>` for more information.
    respect_robots_txt: bool,
    // /// Delay a request
    // request_delay: Option<RequestDelay>,
    /// The client that will be used to send the requests
    client: Option<reqwest_middleware::ClientWithMiddleware>,
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
            client: None,
        }
    }
}

impl CrawlerConfig {
    const MAX_CONCURRENT_REQUESTS: usize = 1_00;

    pub fn max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = Some(max_depth);
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

    pub fn set_client(mut self, client: reqwest_middleware::ClientWithMiddleware) -> Self {
        self.client = Some(client);
        self
    }

    /// *NOTE* [`reqwest_middleware::ClientWithMiddleware`] already uses Arc under the hood, so
    /// it's preferable to just `clone` it and pass via [`Self::set_client`]
    #[deprecated(
        since = "0.2.0",
        note = "You do not have to wrap the Client it in a `Arc` to reuse it, because it already uses an `Arc` internally. Users should use `set_client` instead."
    )]
    pub fn with_shared_client(
        mut self,
        client: std::sync::Arc<reqwest_middleware::ClientWithMiddleware>,
    ) -> Self {
        self.client = Some(client.as_ref().clone());
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

    pub fn max_concurrent_requests(mut self, max_requests: usize) -> Self {
        self.max_requests = Some(max_requests);
        self
    }
}
