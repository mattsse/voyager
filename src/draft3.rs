use async_trait::async_trait;
use futures::stream::Stream;
use futures::{FutureExt, TryFutureExt};
use reqwest::header::HeaderMap;
use reqwest::{IntoUrl, StatusCode, Url};
use scraper::Html;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as StdContext, Poll};
use std::time::Duration;
use anyhow::Result;

pub struct Collector<T: Scraper>

{
    scrapings: Vec<Pin<Box<dyn Future<Output = Option<T::Output>>>>>,
    crawler: Crawler<T>,
    pub scraper: T,
    /// Number of concurrent requests
    max: usize,
}

impl<T:Scraper> Collector<T> {


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

}

impl<T> Stream for Collector<T>
where
    T: Scraper<Output = String, CrawlContext = String> + Unpin + 'static,
    /* <T as Scraper>::Output: Unpin + 'static,
     * <T as Scraper>::CrawlContext: Unpin + 'static, */
{
    type Item = Result<T::Output>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {

        Poll::Pending
    }
}

pub struct Crawler<T: Scraper> {
    running_selections: Vec<Pin<Box<dyn Future<Output = Option<T>>>>>,
    crawls: Vec<Pin<Box<dyn Future<Output = Result<T::Output>> + 'static>>>,
    client: Arc<reqwest::Client>,
    // marker: PhantomData<&'a T>,
}

impl<T> Crawler<T>
where
    T: Scraper + 'static + Unpin,
{
    fn poll(&mut self, html: Html) {}


    /// Send an intermediate job whose result is returned to the scraper again
    fn transition<TTransitionFuture>(&mut self, fun: TTransitionFuture)
    where
        TTransitionFuture: Future<Output = Result<T::CrawlContext>> + 'static,
    {
    }

    /// Submit a complete crawling job that is driven to completion and directly returned once finished.
    pub fn crawl<TCrawlFunction, TCrawlFuture>(&mut self, fun: TCrawlFunction)
    where
        TCrawlFunction: FnOnce(Arc<reqwest::Client>) -> TCrawlFuture,
        TCrawlFuture: Future<Output = Result<T::Output>> + 'static,
    {
        let fut = (fun)(Arc::clone(&self.client));
        self.crawls.push(Box::pin(fut))
    }

    pub fn visit(&mut self, url: impl IntoUrl) {}

    pub fn visit_with_context(&mut self, url: impl IntoUrl, ctx: T::CrawlContext) {}
}

pub struct Context<T> {
    depth: usize,
    request_url: Url,
    response_status: StatusCode,
    response_headers: HeaderMap,
    pub url_context: Option<T>,
}

impl<T> Context<T>
where
    T: Scraper
{
}

pub trait Scraper: Sized {
    type CrawlContext;
    type Output;

    fn on_document(&mut self, document: Html, ctx: Context<Self::CrawlContext>, crawler: &mut Crawler<Self>)
        -> Option<Self::Output>;

    /// This checks whether a submitted url should in fact be requested
    fn is_valid_url(&self, url: &Url) -> bool {
        true
    }
}

pub struct Reddit;

impl Scraper for Reddit {
    type Output = String;
    type CrawlContext = String;

    fn on_document(&mut self, document: Html, ctx: Context<Self::CrawlContext>, crawler: &mut Crawler<Self>) -> Option<Self::Output> {
        // let x = ctx.url_context.take();
        crawler.crawl(move |client| async move {
            dbg!(ctx.url_context);
            client.get("").send().await?;
            Ok("".to_string())
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

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    request_count: usize,
    response_count: usize,
}

#[derive(Debug)]
pub struct CollectorConfig {
    /// Limits the recursion depth of visited URLs.
    max_depth: Option<usize>,
    /// Whether to allow multiple downloads of the same URL
    allow_url_revisit: bool,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_http_error_response: bool,
    /// allows the Collector to ignore any restrictions set by the target host's
    /// robots.txt file.
    ignore_robots_txt: bool,
    /// Limit the retrieved response body in bytes.
    response_body_limit: Option<u64>,
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
}