use async_trait::async_trait;
use futures::stream::Stream;
use futures::{FutureExt, TryFutureExt};
use reqwest::header::HeaderMap;
use reqwest::{StatusCode, Url};
use scraper::Html;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as StdContext, Poll};
use std::time::Duration;

type Selection<T> = Pin<Box<dyn Future<Output = Option<T>> + 'static>>;

type HtmlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>, reqwest::Error>>>>;

pub struct Collector<'a, T: Selector> {
    /// Currently running jobs
    // requests: Vec<HtmlRequest<T::CrawlContext>>,
    selector: &'a mut T,
    state: State<T::CrawlContext>,
    /// Number of concurrent requests
    max: usize,
}

// impl<T: Selector2> Collector<T> {
//     pub fn scrape() {}
// }

impl<'a, T> Stream for Collector<'a, T>
where
    T: Selector + Unpin,
    <T as Selector>::Output: Unpin + 'static,
    <T as Selector>::CrawlContext: Unpin + 'static,
{
    type Item = Result<T::Output, ()>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut StdContext<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // for n in (0..pin.state..len()).rev() {
        //
        // }
        // // Drain pending selection processes
        // for n in (0..pin.requests.len()).rev() {
        //     let mut request = pin.requests.swap_remove(n);
        //     if let Poll::Ready(res) = request.poll_unpin(cx) {
        //
        //         if let Some(res) = res {
        //             return Poll::Ready(Some(Ok(res)));
        //         }
        //     }
        //     pin.running_selections.push(request);
        // }
        //
        // while pin.requests.len() < pin.max {
        //     if let Some(url) = pin.state.pending.pop_front() {
        //         pin.requests.push(Box::pin(pin.state.get_html(url)));
        //     } else {
        //         break;
        //     }
        // }
        //
        //
        //
        // for n in (0..pin.running_selections.len()).rev() {
        //     let mut request = pin.requests.swap_remove(n);
        //     if let Poll::Ready(resp) = request.poll_unpin(cx) {
        //         let resp = resp.unwrap();
        //         let doc = resp.html;
        //         let ctx = Context {
        //             depth: resp.info.depth,
        //             request_url: resp.info.url,
        //             response_status: resp.status,
        //             response_headers: resp.headers,
        //             state: &mut pin.state,
        //             url_context: resp.info.ctx,
        //         };
        //         // let req = pin.selector.on_document2(doc, ctx);
        //         // pin.running_selections.push(req);
        //     }
        // }

        unimplemented!()
    }
}

pub struct State<T> {
    requests: Vec<HtmlRequest<T>>,
    client: Arc<reqwest::Client>,
    pending: VecDeque<RequestInfo<T>>,
    stats: Stats,
    request_delay: Option<RequestDelay>,
}

impl<T: 'static + Unpin> State<T> {
    fn get_html(&self, info: RequestInfo<T>) -> impl Future<Output = reqwest::Result<Response<T>>> {
        self.client
            .get(info.url.clone())
            .send()
            .and_then(|mut response| {
                let status = response.status();
                let mut headers = HeaderMap::default();
                std::mem::swap(&mut headers, response.headers_mut());
                response.text().and_then(move |txt| {
                    let html = Html::parse_document(&txt);
                    futures::future::ok(Response {
                        status,
                        headers,
                        info,
                        html,
                    })
                })
            })
    }
}

struct RequestInfo<T> {
    ctx: T,
    url: Url,
    depth: usize,
    kind: ResponseKind,
}

pub struct Response<T> {
    html: Html,
    status: StatusCode,
    headers: HeaderMap,
    info: RequestInfo<T>,
}

#[derive(Debug, Clone)]
pub enum ResponseKind {
    Text,
    Html,
    Json,
    Bytes,
}

pub struct Context<'a, T> {
    depth: usize,
    request_url: Url,
    response_status: StatusCode,
    response_headers: HeaderMap,
    state: &'a mut State<T>,
    pub url_context: Option<T>,
}

impl<'a, T> Context<'a, T> {
    pub fn visit(&self, url: Url) {}

    pub fn visit_with_context(&self, url: Url, ctx: T) {}
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

pub trait Selector {
    /// The Output type this Selector produces upon scraping
    type Output;
    /// Used to add additional context to an url that should be requested
    type CrawlContext;

    /// Once a html page was retrieved successfully, the selector gets notified.
    fn on_document(&mut self, doc: Html, ctx: Context<Self::CrawlContext>) -> Option<Self::Output>;

    /// This checks whether a submitted url should in fact be requested
    fn is_valid_url(&self, url: &Url) -> bool {
        true
    }
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

#[async_trait]
pub trait Job {
    type Output;

    async fn run(&mut self) -> Option<Self::Output>;
}
