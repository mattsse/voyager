use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use futures::stream::Stream;
use futures::{Future, FutureExt};
use reqwest::header::HeaderValue;

use crate::error::{CrawlError, DisallowReason};
use crate::requests::{
    response_info, QueuedRequest, QueuedRequestBuilder, RequestDelay, RequestQueue,
};
use crate::response::Response;
use crate::robots::{RobotsData, RobotsHandler};

pub enum DomainListing<T> {
    AllowList(AllowList<T>),
    BlockList(BlockList<T>),
}

impl<T> DomainListing<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    pub(crate) fn add_request(
        &mut self,
        request: QueuedRequestBuilder<T>,
    ) -> Result<(), CrawlError<T>> {
        let QueuedRequestBuilder {
            request,
            state,
            depth,
        } = request;

        match request.build() {
            Ok(request) => {
                let queued = QueuedRequest {
                    request,
                    state,
                    depth,
                };
                match self {
                    DomainListing::AllowList(list) => Ok(list.add_request(queued)?),
                    DomainListing::BlockList(list) => Ok(list.add_request(queued)?),
                }
            }
            Err(error) => Err(CrawlError::<T>::FailedToBuildRequest {
                error,
                state,
                depth,
            }),
        }
    }
}

impl<T> Stream for DomainListing<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            DomainListing::AllowList(list) => Stream::poll_next(Pin::new(list), cx),
            DomainListing::BlockList(list) => Stream::poll_next(Pin::new(list), cx),
        }
    }
}

pub struct AllowList<T> {
    /// All queued results that are gathered from all the different futures
    ///
    /// polling them all consecutively and buffering all the results prevents
    /// bias
    queued_results: VecDeque<Result<Response<T>>>,
    /// all allowed domains
    allowed: HashMap<String, AllowedDomain<T>>,
    domains: Vec<String>,
}

impl<T> Default for AllowList<T> {
    fn default() -> Self {
        Self {
            queued_results: Default::default(),
            allowed: Default::default(),
            domains: Vec::new(),
        }
    }
}

impl<T: fmt::Debug> AllowList<T> {
    pub fn allow(&mut self, domain: String, config: AllowListConfig) {
        self.allowed
            .insert(domain.clone(), AllowedDomain::new(config));
        self.domains.push(domain)
    }

    pub fn disallow(&mut self, domain: &str) -> Option<AllowedDomain<T>> {
        if let Some(list) = self.allowed.remove(domain) {
            let idx = self.domains.iter().position(|d| d == domain).unwrap();
            self.domains.remove(idx);
            Some(list)
        } else {
            None
        }
    }

    /// The matching handler for the allowed domain if any
    pub fn get_domain(&self, domain: impl AsRef<str>) -> Option<&AllowedDomain<T>> {
        self.allowed.get(domain.as_ref())
    }

    /// Get mutable access to the matching handler for the allowed domain if any
    pub fn get_domain_mut(&mut self, domain: impl AsRef<str>) -> Option<&mut AllowedDomain<T>> {
        self.allowed.get_mut(domain.as_ref())
    }
}

impl<T> AllowList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    pub(crate) fn add_request(&mut self, req: QueuedRequest<T>) -> Result<(), CrawlError<T>> {
        if let Some(host) = req.request.url().host_str() {
            if let Some(allowed) = self.allowed.get_mut(host) {
                allowed.add_request(req)
            } else {
                Err(CrawlError::DisallowedRequest {
                    request: req.request,
                    state: req.state,
                    reason: DisallowReason::User,
                })
            }
        } else {
            Err(CrawlError::InvalidRequest {
                request: req.request,
                state: req.state,
            })
        }
    }
}

impl<T> Stream for AllowList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        loop {
            if let Some(res) = pin.queued_results.pop_front() {
                return Poll::Ready(Some(res));
            }
            let mut busy = false;
            for n in (0..pin.domains.len()).rev() {
                let domain = pin.domains.swap_remove(n);
                let (key, mut list) = pin.allowed.remove_entry(&domain).unwrap();
                loop {
                    match Stream::poll_next(Pin::new(&mut list), cx) {
                        Poll::Ready(Some(res)) => {
                            pin.queued_results.push_back(res);
                        }
                        Poll::Pending => {
                            busy = true;
                            break;
                        }
                        _ => break,
                    }
                }
                pin.allowed.insert(key, list);
                pin.domains.push(domain);
            }

            if pin.queued_results.is_empty() {
                return if !busy {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                };
            }
        }
    }
}

type CrawlRequest<T> = Pin<Box<dyn Future<Output = Result<Response<T>>>>>;

type RobotsTxtRequest = Pin<Box<dyn Future<Output = Result<RobotsData>>>>;

pub struct AllowedDomain<T> {
    client: reqwest_middleware::ClientWithMiddleware,
    /// Futures that eventually return a http response that is passed to the
    /// scraper
    in_progress_crawl_requests: Vec<CrawlRequest<T>>,
    /// stores the future that requests the robots txt for a host
    in_progress_robots_txt_crawls: Option<RobotsTxtRequest>,
    /// Temporary buffer for requests that may arrive while robots.txt is being
    /// fetched
    tmp_request_buffer: Option<VecDeque<QueuedRequest<T>>>,
    /// The request in process to fetch the the robots.txt
    /// all queued requests
    request_queue: RequestQueue<T>,
    /// The parsed robots.txt contents of this domain
    robots: Option<RobotsData>,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
    /// Respect any restrictions set by the target host's robots.txt file
    respect_robots_txt: bool,
    /// The maximum depth request are allowed to next
    max_depth: usize,
    /// Limits request to execute concurrently.
    max_requests: usize,
}

impl<T: fmt::Debug> AllowedDomain<T> {
    pub fn new(config: AllowListConfig) -> Self {
        Self {
            client: config.client,
            in_progress_crawl_requests: Vec::new(),
            in_progress_robots_txt_crawls: None,
            request_queue: config
                .delay
                .map(RequestQueue::with_delay)
                .unwrap_or_default(),
            tmp_request_buffer: None,
            robots: None,
            skip_non_successful_responses: config.skip_non_successful_responses,
            respect_robots_txt: config.respect_robots_txt,
            max_depth: config.max_depth,
            max_requests: config.max_requests,
        }
    }

    pub(crate) fn add_request(&mut self, req: QueuedRequest<T>) -> Result<(), CrawlError<T>> {
        if req.depth > self.max_depth {
            return Err(CrawlError::ReachedMaxDepth {
                request: req.request,
                state: req.state,
                depth: req.depth,
            });
        }

        if self.respect_robots_txt && self.robots.is_none() {
            if self.in_progress_robots_txt_crawls.is_none() {
                // add request to fetch robots.txt
                let mut url = req.request.url().clone();
                url.set_path("robots.txt");
                let fut = self.client.get(url).send();
                let fut = Box::pin(async move {
                    let resp = fut.await?;
                    RobotsHandler::from_response(resp).await
                });
                self.in_progress_robots_txt_crawls = Some(fut);
            }
            // robots not ready yet
            let buf = self.tmp_request_buffer.get_or_insert(VecDeque::default());
            buf.push_back(req);
        } else {
            self.request_queue.queue_mut().push_back(req);
        }
        Ok(())
    }

    /// Remove the configured delay
    pub fn remove_delay(&mut self) -> Option<RequestDelay> {
        self.request_queue.remove_delay()
    }

    /// Set a delay between requests
    pub fn set_delay(&mut self, delay: RequestDelay) -> Option<RequestDelay> {
        self.request_queue.set_delay(delay)
    }
}

impl<T> Stream for AllowedDomain<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // fetch robots.txt
        if let Some(mut fut) = pin.in_progress_robots_txt_crawls.take() {
            if let Poll::Ready(result) = fut.poll_unpin(cx) {
                match result {
                    Ok(data) => {
                        pin.robots = Some(data);
                    }
                    Err(err) => return Poll::Ready(Some(Err(err))),
                }
            } else {
                pin.in_progress_robots_txt_crawls = Some(fut);
                return Poll::Pending;
            }
        }

        if let Some(mut tmp) = pin.tmp_request_buffer.take() {
            // queue in all requests that arrived while robots.txt was being fetched
            if let Some(robots) = pin.robots.take() {
                while let Some(req) = tmp.pop_front() {
                    if robots.is_not_disallowed(&req.request) {
                        pin.request_queue.queue_mut().push_back(req);
                    } else {
                        pin.robots = Some(robots);
                        pin.tmp_request_buffer = Some(tmp);
                        return Poll::Ready(Some(Err(CrawlError::DisallowedRequest {
                            request: req.request,
                            state: req.state,
                            reason: DisallowReason::RobotsTxt,
                        }
                        .into())));
                    }
                }
                pin.robots = Some(robots);
            } else {
                // robots request failed
                pin.request_queue.queue_mut().extend(tmp.into_iter());
            }
        }

        while let Poll::Ready(Some(req)) = Stream::poll_next(Pin::new(&mut pin.request_queue), cx) {
            if pin
                .robots
                .as_ref()
                .map(|robots| robots.is_not_disallowed(&req.request))
                .unwrap_or(true)
            {
                // respect robots.txt
                let mut fut = Box::pin(get_response(
                    pin.client.clone(),
                    req,
                    pin.skip_non_successful_responses,
                ));
                if let Poll::Ready(resp) = fut.poll_unpin(cx) {
                    return Poll::Ready(Some(resp));
                } else {
                    pin.in_progress_crawl_requests.push(fut);

                    if pin.in_progress_crawl_requests.len() > pin.max_requests {
                        // stop when reached maximum of active requests
                        break;
                    }
                }
            } else {
                return Poll::Ready(Some(Err(CrawlError::DisallowedRequest {
                    request: req.request,
                    state: req.state,
                    reason: DisallowReason::RobotsTxt,
                }
                .into())));
            }
        }

        // advance all http requests
        for n in (0..pin.in_progress_crawl_requests.len()).rev() {
            let mut request = pin.in_progress_crawl_requests.swap_remove(n);

            if let Poll::Ready(resp) = request.poll_unpin(cx) {
                return Poll::Ready(Some(resp));
            } else {
                pin.in_progress_crawl_requests.push(request);
            }
        }

        if pin.in_progress_crawl_requests.is_empty()
            && pin.in_progress_robots_txt_crawls.is_none()
            && pin.request_queue.is_empty()
        {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

pub struct AllowListConfig {
    pub delay: Option<RequestDelay>,
    pub respect_robots_txt: bool,
    pub client: reqwest_middleware::ClientWithMiddleware,
    pub skip_non_successful_responses: bool,
    pub max_depth: usize,
    pub max_requests: usize,
}

pub struct BlockList<T> {
    client: reqwest_middleware::ClientWithMiddleware,
    /// list of domains that are blocked
    blocked_domains: HashSet<String>,
    /// Futures that eventually return a http response that is passed to the
    /// scraper
    in_progress_crawl_requests: Vec<CrawlRequest<T>>,
    /// mapping of hosts to robots.txt data
    robots_map: HashMap<String, RobotsData>,
    /// stores the futures that request the robots txt for a host
    in_progress_robots_txt_crawls: Vec<(String, RobotsTxtRequest)>,
    /// Currently in progress robot txt crawl request
    in_progress_robots_txt_crawl_hosts: HashSet<String>,
    /// Respect any restrictions set by the target host's robots.txt file
    respect_robots_txt: bool,
    /// Whether to ignore responses with a non 2xx response code see
    /// `reqwest::Response::is_success`
    skip_non_successful_responses: bool,
    /// The maximum depth request are allowed to next
    max_depth: usize,
    /// all queued requests
    request_queue: RequestQueue<T>,
    /// Limits request to execute concurrently.
    max_requests: usize,
}

impl<T> BlockList<T> {
    pub fn new(
        blocked_domains: HashSet<String>,
        client: reqwest_middleware::ClientWithMiddleware,
        respect_robots_txt: bool,
        skip_non_successful_responses: bool,
        max_depth: usize,
        max_requests: usize,
    ) -> Self {
        BlockList {
            client,
            blocked_domains,
            in_progress_crawl_requests: Vec::new(),
            robots_map: Default::default(),
            in_progress_robots_txt_crawls: Vec::new(),
            in_progress_robots_txt_crawl_hosts: Default::default(),
            respect_robots_txt,
            skip_non_successful_responses,
            request_queue: Default::default(),
            max_depth,
            max_requests,
        }
    }
}

impl<T> BlockList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    pub(crate) fn add_request(&mut self, req: QueuedRequest<T>) -> Result<(), CrawlError<T>> {
        if req.depth > self.max_depth {
            return Err(CrawlError::ReachedMaxDepth {
                request: req.request,
                state: req.state,
                depth: req.depth,
            });
        }
        if let Some(host) = req.request.url().host_str() {
            if self.blocked_domains.contains(host) {
                Err(CrawlError::DisallowedRequest {
                    request: req.request,
                    state: req.state,
                    reason: DisallowReason::User,
                })
            } else {
                self.request_queue.queue_mut().push_back(req);
                Ok(())
            }
        } else {
            Err(CrawlError::InvalidRequest {
                request: req.request,
                state: req.state,
            })
        }
    }

    /// Block requests to that domain
    pub fn disallow(&mut self, domain: impl Into<String>) {
        self.blocked_domains.insert(domain.into());
    }

    /// Remove a domain from the block list
    pub fn allow(&mut self, domain: impl AsRef<str>) {
        self.blocked_domains.remove(domain.as_ref());
    }

    /// Remove the configured delay
    pub fn remove_delay(&mut self) -> Option<RequestDelay> {
        self.request_queue.remove_delay()
    }

    /// Set a delay between requests
    pub fn set_delay(&mut self, delay: RequestDelay) -> Option<RequestDelay> {
        self.request_queue.set_delay(delay)
    }
}

impl<T> Stream for BlockList<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    type Item = Result<Response<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // drive all robots.txt to completion
        for n in (0..pin.in_progress_robots_txt_crawls.len()).rev() {
            let (host, mut fut) = pin.in_progress_robots_txt_crawls.swap_remove(n);
            if let Poll::Ready(result) = fut.poll_unpin(cx) {
                match result {
                    Ok(robots) => {
                        pin.in_progress_robots_txt_crawl_hosts.remove(&host);
                        pin.robots_map.insert(host, robots);
                    }
                    Err(_) => {
                        // robots.txt lookup failed
                        pin.in_progress_robots_txt_crawl_hosts.remove(&host);
                        return Poll::Ready(Some(Err(
                            CrawlError::<T>::RobotsTxtError { host }.into()
                        )));
                    }
                }
            } else {
                pin.in_progress_robots_txt_crawls.push((host, fut))
            }
        }

        // we add back to the queue in case robots.txt is required and not ready
        for _ in 0..pin.request_queue.len() {
            if pin.in_progress_crawl_requests.len() > pin.max_requests {
                break;
            }

            if let Poll::Ready(Some(req)) = Stream::poll_next(Pin::new(&mut pin.request_queue), cx)
            {
                if pin.respect_robots_txt {
                    if let Some(host) = req.request.url().host_str() {
                        if let Some(robots) = pin.robots_map.get(host) {
                            if robots.is_not_disallowed(&req.request) {
                                let fut = Box::pin(get_response(
                                    pin.client.clone(),
                                    req,
                                    pin.skip_non_successful_responses,
                                ));
                                pin.in_progress_crawl_requests.push(fut);
                            } else {
                                return Poll::Ready(Some(Err(CrawlError::DisallowedRequest {
                                    request: req.request,
                                    state: req.state,
                                    reason: DisallowReason::RobotsTxt,
                                }
                                .into())));
                            }
                        } else {
                            let mut url = req.request.url().clone();
                            url.set_path("robots.txt");
                            let fut = pin.client.get(url).send();

                            let fut = Box::pin(async move {
                                let resp = fut.await?;
                                RobotsHandler::from_response(resp).await
                            });

                            pin.in_progress_robots_txt_crawls
                                .push((host.to_string(), fut));
                            pin.in_progress_robots_txt_crawl_hosts
                                .insert(host.to_string());
                            pin.request_queue.queue_mut().push_back(req);
                        }
                    } else {
                        return Poll::Ready(Some(Err(CrawlError::InvalidRequest {
                            request: req.request,
                            state: req.state,
                        }
                        .into())));
                    }
                } else {
                    let fut = Box::pin(get_response(
                        pin.client.clone(),
                        req,
                        pin.skip_non_successful_responses,
                    ));
                    pin.in_progress_crawl_requests.push(fut);
                }
            }
        }

        for n in (0..pin.in_progress_crawl_requests.len()).rev() {
            let mut request = pin.in_progress_crawl_requests.swap_remove(n);

            if let Poll::Ready(resp) = request.poll_unpin(cx) {
                return Poll::Ready(Some(resp));
            } else {
                pin.in_progress_crawl_requests.push(request);
            }
        }

        if pin.in_progress_crawl_requests.is_empty()
            && pin.request_queue.is_empty()
            && pin.in_progress_robots_txt_crawls.is_empty()
        {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

fn get_response<T>(
    client: reqwest_middleware::ClientWithMiddleware,
    request: QueuedRequest<T>,
    skip_non_successful_responses: bool,
) -> CrawlRequest<T>
where
    T: Unpin + Send + Sync + fmt::Debug + 'static,
{
    let QueuedRequest {
        request,
        state,
        depth,
    } = request;
    let request_url = request.url().clone();
    let skip_http_error_response = skip_non_successful_responses;

    Box::pin(async move {
        let request = client.execute(request);
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
